/** Personality-styled approval interception for sensitive tools.
 *
 * Listens to the `tools/pre-execute` waterfall. Sensitive tools trigger a
 * personality-flavored confirmation prompt (via the `ask` decision) instead
 * of a dry "Allow?". Non-sensitive tools pass through untouched.
 */
import type { Context } from "@deepseek-ai/cordis";
import type { PreToolDecision, ToolExecution } from "@deepseek-ai/dsh-tools";

/** Tool name patterns considered sensitive. Matched case-insensitively. */
const SENSITIVE_PATTERNS: RegExp[] = [
  // File deletion
  /\brm\b/i,
  /\bunlink\b/i,
  /\bdelete\b/i,
  /rimraf/i,
  // Network / exfiltration
  /\bcurl\b/i,
  /\bwget\b/i,
  /\bfetch\b/i,
  // Privilege escalation
  /\bsudo\b/i,
  /\bsu\b/i,
  /\bchmod\s+.*777/i,
  // Payment / financial
  /\bpay\b/i,
  /\bpurchase\b/i,
  /\bstripe/i,
];

/** Extra patterns from config (comma-separated). */
function extraPatterns(): RegExp[] {
  const raw = process.env.RIKO_DSH_SENSITIVE_TOOLS ?? "";
  const patterns: RegExp[] = [];
  for (const s of raw.split(",").map((s) => s.trim()).filter(Boolean)) {
    try {
      patterns.push(new RegExp(s, "i"));
    } catch (err) {
      console.warn(`[riko-dsh] Invalid regex pattern in RIKO_DSH_SENSITIVE_TOOLS: "${s}"`, err);
    }
  }
  return patterns;
}

function isSensitive(toolName: string): boolean {
  return [...SENSITIVE_PATTERNS, ...extraPatterns()].some((re) => re.test(toolName));
}

function warn(ctx: Context, message: string): void {
  const logger = (ctx as unknown as { logger?: { warn?: (text: string) => void } }).logger;
  logger?.warn?.(`riko-dsh: ${message}`);
}

/**
 * Build a personality-styled approval prompt.
 * 尽量 aligns with a warm assistant tone; the twin adapts wording per scenario.
 */
function approvalPrompt(toolName: string): string {
  return (
    `主人，这个操作看起来有点危险喵…\n\n` +
    `工具 \`${toolName}\` 可能会删除文件、对外发请求或者动到钱。\n` +
    `璃先拦一下，确认主人是真的想执行吗？`
  );
}

/** Pure decision helper, exported for tests. */
export function decideApproval(toolName: string): PreToolDecision | null {
  if (!isSensitive(toolName)) return null;
  return {
    kind: "ask",
    reason: `sensitive tool intercepted: ${toolName}`,
    displayReason: { en: approvalPrompt(toolName) },
  };
}

/** Register the tools/pre-execute waterfall listener. */
export function registerApprovalInterception(ctx: Context): void {
  ctx.on("tools/pre-execute" as never, (async (
    exec: ToolExecution,
    next: () => Promise<PreToolDecision>,
  ): Promise<PreToolDecision> => {
    const toolName = String((exec as { name?: unknown }).name ?? "");
    const decision = decideApproval(toolName);
    if (decision) {
      warn(ctx, `sensitive tool intercepted, asking user: ${toolName}`);
      return decision;
    }
    return next();
  }) as never);
}
