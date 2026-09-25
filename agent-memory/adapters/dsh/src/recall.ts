/**
 * 自动上下文注入（doc2/04 §4，官方 agent/pre-step 已核对 @477b4f4）。
 *
 * - waterfall 先 await next()；decision.kind!=='enter' 或 signal.aborted → 原样返回。
 * - 查询只从本次 args.messages 选最后一条 source.kind==='user' 的原始用户消息；
 *   缺少明确 source 不提升为用户。
 * - compose 与 args.signal 共同取消，默认 500ms；空/离线/错误 → 原 decision。
 * - 注入用官方 createUserMessage。
 *   冲突记录（doc2/04 §4 写 source.kind='plugin'，官方 v4 会话格式已退役该 kind，
 *   session-format-v3-to-v4/src/message-sources.ts 明确拒绝 kind==='plugin'）：
 *   按一方惯例（agent-instructions/time-context）使用 producer 自有 kind 名
 *   {kind:'agent-memory', form:'recall'}；事件线只认 source.kind==='user'，
 *   故注入不会回流为用户证据。
 * - 同一 decision 已有本插件消息 → 跳过，防重复注入。
 */
import { createUserMessage } from "@deepseek-ai/dsh-llm";
import type { UserMessage } from "@deepseek-ai/dsh-llm";
import type { Agent } from "@deepseek-ai/dsh-agent";
import type { PreStepDecision } from "@deepseek-ai/dsh-agent";
import type { MemoryClient } from "./client.js";
import type { Logger } from "./events.js";

declare module "@deepseek-ai/dsh-llm" {
  interface MessageSourceMap {
    /** 记忆上下文注入：producer 自有 kind（v4 格式要求），事件线拒绝其为用户证据。 */
    "agent-memory": { kind: "agent-memory"; form: "recall" };
  }
}

export const PLUGIN_KIND = "agent-memory";

export interface PreStepPayload {
  agent: Agent;
  messages: UserMessage[];
  signal: AbortSignal;
}

function textOf(message: UserMessage): string {
  return message.content
    .filter((b): b is { type: "text"; text: string } => b.type === "text" && typeof (b as { text?: unknown }).text === "string")
    .map((b) => b.text)
    .join("\n")
    .trim();
}

/** 只选最后一条 source.kind==='user' 的原始用户消息。 */
export function latestOriginalUserText(messages: readonly UserMessage[]): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i];
    if (!m) continue;
    if (m.source?.kind !== "user") continue;
    const text = textOf(m);
    if (text.length > 0) return text;
  }
  return "";
}

function alreadyInjected(decision: PreStepDecision): boolean {
  if (decision.kind !== "enter") return false;
  return decision.messages.some((m) => m.source?.kind === PLUGIN_KIND);
}

export function makePreStepHook(client: MemoryClient, logger: Logger, composeTimeoutMs: number) {
  return async function preStep(payload: PreStepPayload, next: () => Promise<PreStepDecision>): Promise<PreStepDecision> {
    const decision = await next();
    if (decision.kind !== "enter" || payload.signal.aborted) return decision;
    if (alreadyInjected(decision)) return decision;
    const query = latestOriginalUserText(payload.messages);
    if (!query) return decision;
    try {
      const ctrl = new AbortController();
      const timer = setTimeout(() => ctrl.abort(), composeTimeoutMs);
      const onParentAbort = () => ctrl.abort();
      payload.signal.addEventListener("abort", onParentAbort, { once: true });
      try {
        const r = await client.compose(
          { agent_id: String(payload.agent.id), query, max_items: 5, max_chars: 2000 },
          ctrl.signal,
        );
        if (r.failure !== "ok" || r.status !== 200) {
          logger.warn(`compose 跳过注入 status=${r.status} request_id=${r.requestId ?? "?"}`);
          return decision;
        }
        const body = (r.body ?? {}) as { text?: string };
        if (!body.text) return decision; // 无可用记忆不加占位
        const memoryMessage = createUserMessage({
          content: [{ type: "text", text: body.text }],
          source: { kind: PLUGIN_KIND, form: "recall" },
        });
        return { ...decision, messages: [...decision.messages, memoryMessage] };
      } finally {
        clearTimeout(timer);
        payload.signal.removeEventListener("abort", onParentAbort);
      }
    } catch (error) {
      logger.warn(`compose 跳过注入: ${error instanceof Error ? error.message : String(error)}`);
      return decision;
    }
  };
}
