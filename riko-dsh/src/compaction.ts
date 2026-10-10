/** Compaction micro-adjustments (H9-micro).
 *
 * DSH provides native compaction via @deepseek-ai/dsh-compaction. This module
 * does NOT rewrite it. It provides two extension points for twin-specific
 * tuning, to be enabled only after measuring DSH's default behavior:
 *
 * 1. Summary quality: custom summary prompt ensuring key decisions, todos,
 *    and user preferences survive compaction (Muse-style retention).
 * 2. Injection format: after compaction, inject not just the summary but also
 *    a memory snapshot + goal state (Muse's standing-context order).
 *
 * Status: STUB. Real DSH compaction behavior must be measured in a live
 * session before enabling either adjustment. See doc7/12 H9-micro spec.
 */
import type { Context } from "@deepseek-ai/cordis";

/**
 * Muse-style summary retention checklist.
 * When customizing the summary prompt, ensure these survive:
 */
export const SUMMARY_RETENTION_CHECKLIST = [
  "key decisions made in the conversation",
  "open todos and pending commitments",
  "user preferences stated or implied",
  "corrections the user made to prior statements",
  "active goal state and next steps",
] as const;

/**
 * Recommended injection order after compaction (Muse-style):
 * summary -> memory snapshot -> goal state
 */
export const POST_COMPACTION_INJECTION_ORDER = [
  "summary",
  "memory_snapshot",
  "goal_state",
] as const;

function warn(ctx: Context, message: string): void {
  const logger = (ctx as unknown as { logger?: { warn?: (text: string) => void } }).logger;
  logger?.warn?.(`riko-dsh: ${message}`);
}

/**
 * Register compaction hooks. Currently a no-op stub that logs a reminder
 * to measure DSH default behavior first.
 */
export function registerCompactionTuning(ctx: Context): void {
  warn(
    ctx,
    "compaction tuning is a stub (H9-micro): measure DSH default summary " +
      "quality in a live session before enabling custom prompt or injection format.",
  );
  // Future: hook into dsh-compaction events here once measured.
}
