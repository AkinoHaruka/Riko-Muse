/** Custom retry strategy for model request failures (H12).
 *
 * Listens to the `agent/request-error` waterfall and applies twin-specific
 * retry policy:
 * - 429/503: exponential backoff, max 3 retries
 * - 401/403: no retry (auth problem, retrying is futile)
 * - timeout: retry once
 * - other: retry once
 *
 * Retry counts are tracked per (agent, turn, step) to avoid infinite loops.
 */
import type { Context } from "@deepseek-ai/cordis";
import type { LlmFailure } from "@deepseek-ai/dsh-llm";

type RequestErrorAction = { kind: "retry" } | undefined;

interface RequestErrorPayload {
  agent: { session?: { id?: unknown } };
  turn: number;
  step: number;
  provider: string;
  failure: LlmFailure;
  signal: AbortSignal;
}

/** Track retry attempts per request key. */
const attempts = new Map<string, number>();

function requestKey(p: RequestErrorPayload): string {
  const sid = String(p.agent.session?.id ?? "unknown");
  return `${sid}:${p.turn}:${p.step}`;
}

function warn(ctx: Context, message: string): void {
  const logger = (ctx as unknown as { logger?: { warn?: (text: string) => void } }).logger;
  logger?.warn?.(`riko-dsh: ${message}`);
}

function isTimeout(failure: LlmFailure): boolean {
  const msg = failure.message.toLowerCase();
  const code = failure.code.toLowerCase();
  return msg.includes("timeout") || code.includes("timeout") || code === "etimedout";
}

/**
 * Pure decision function, exported for tests.
 * Returns {action, delayMs}: action to take and how long to wait before retry.
 */
export function decideRetry(
  failure: LlmFailure,
  attemptCount: number,
): { action: RequestErrorAction; delayMs: number } {
  const status = failure.status;

  // 401/403: auth problem, never retry
  if (status === 401 || status === 403) {
    return { action: undefined, delayMs: 0 };
  }

  // 429/503: exponential backoff, max 3 retries
  if (status === 429 || status === 503) {
    if (attemptCount >= 3) return { action: undefined, delayMs: 0 };
    // Respect provider-requested delay if present, else exponential backoff
    const base = failure.providerRetryAfterMs ?? 1000 * Math.pow(2, attemptCount);
    return { action: { kind: "retry" }, delayMs: Math.min(base, 30000) };
  }

  // Timeout: retry once
  if (isTimeout(failure)) {
    if (attemptCount >= 1) return { action: undefined, delayMs: 0 };
    return { action: { kind: "retry" }, delayMs: 2000 };
  }

  // Other errors: retry once
  if (attemptCount >= 1) return { action: undefined, delayMs: 0 };
  return { action: { kind: "retry" }, delayMs: 1000 };
}

/** Register the agent/request-error waterfall listener. */
export function registerRetryPolicy(ctx: Context): void {
  ctx.on("agent/request-error" as never, (async (
    payload: RequestErrorPayload,
    next: () => Promise<RequestErrorAction>,
  ): Promise<RequestErrorAction> => {
    const key = requestKey(payload);
    const count = attempts.get(key) ?? 0;
    const { action, delayMs } = decideRetry(payload.failure, count);

    if (!action) {
      attempts.delete(key);
      warn(ctx, `not retrying ${payload.provider} failure (status=${payload.failure.status}, attempts=${count})`);
      return next();
    }

    attempts.set(key, count + 1);
    warn(ctx, `retrying ${payload.provider} failure in ${delayMs}ms (attempt ${count + 1}, status=${payload.failure.status})`);

    if (delayMs > 0) {
      await new Promise((resolve, reject) => {
        const timer = setTimeout(resolve, delayMs);
        payload.signal.addEventListener("abort", () => {
          clearTimeout(timer);
          reject(new Error("aborted"));
        }, { once: true });
      }).catch(() => {
        // Aborted during backoff; fall through to default handling.
        attempts.delete(key);
        return next();
      });
    }

    return action;
  }) as never);
}

/** Clear retry state (for tests). */
export function clearRetryState(): void {
  attempts.clear();
}
