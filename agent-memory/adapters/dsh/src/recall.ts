/**
 * 自动上下文注入（doc/05 §5、doc/14 §4）。
 *
 * 结构已核对宿主参考实现（riko-memory/src/index.ts:231-272）：
 * ctx.on('agent/pre-step', async ({agent,messages,signal}, next) => {
 *   const decision = await next();
 *   if (decision.kind !== 'enter' || signal.aborted) return decision;
 *   ... return {...decision, messages:[...decision.messages, memoryMessage]};
 * })
 *
 * makePluginSourceMessage 由宿主 glue 用目标 DSH 版本的 createUserMessage 实现，
 * 必须带 source:{kind:'plugin'}，防止下一轮被当成新用户陈述（doc/06 接入纪律 3）。
 */
import type { MemoryClient } from "./client.js";
import type { Logger } from "./events.js";

export interface PreStepArgs {
  agent: { id: unknown };
  messages: readonly unknown[];
  signal: { aborted: boolean };
}

export interface AgentDecision {
  kind: string;
  messages: unknown[];
  [key: string]: unknown;
}

export interface HostGlue {
  createUserMessage(opts: {
    content: { type: "text"; text: string }[];
    source: { kind: string; plugin: string; form: string };
  }): unknown;
}

const PLUGIN_NAME = "@agent-memory/dsh-adapter";
const INJECTION_FORM = "agent-memory";

export interface RecallMessageLike {
  readonly source?: { kind?: string; plugin?: string };
}

/** 只选最后一条真实用户消息的文本（不选历史注入或助手消息）。 */
export function latestOriginalUserText(messages: readonly unknown[]): string {
  for (const message of [...messages].reverse()) {
    const m = message as {
      role?: string;
      source?: { kind?: string };
      content?: readonly { type?: string; text?: string }[];
    };
    if (m.role !== "user") continue;
    if (m.source?.kind !== undefined && m.source.kind !== "user") continue;
    const text = (m.content ?? [])
      .filter((b) => b.type === "text" && typeof b.text === "string")
      .map((b) => b.text as string)
      .join("\n")
      .trim();
    if (text) return text;
  }
  return "";
}

/** 同回合重入检查：decision.messages 已有本插件消息则跳过。 */
function alreadyInjected(decision: AgentDecision): boolean {
  return decision.messages.some((m) => {
    const msg = m as RecallMessageLike;
    return msg.source?.kind === "plugin" && msg.source?.plugin === PLUGIN_NAME;
  });
}

export function makeComposeHook(
  client: MemoryClient,
  glue: HostGlue,
  logger: Logger,
  composeTimeoutMs: number,
) {
  return async function preStep(
    args: PreStepArgs,
    next: () => Promise<AgentDecision>,
  ): Promise<AgentDecision> {
    const decision = await next();
    if (decision.kind !== "enter" || args.signal.aborted) return decision;
    if (alreadyInjected(decision)) return decision;
    const query = latestOriginalUserText(args.messages);
    if (!query) return decision;
    try {
      const r = await client.compose({
        agent_id: String(args.agent.id),
        query,
        max_items: 5,
        max_chars: 2000,
      });
      if (r.status !== 200) {
        logger.warn(`compose 失败 status=${r.status} request_id=${r.requestId ?? "?"}`);
        return decision;
      }
      const body = r.body as { text?: string };
      if (!body.text) return decision; // 无可用记忆 → 空块，不加占位（doc/05 §5）
      const memoryMessage = glue.createUserMessage({
        content: [{ type: "text", text: body.text }],
        source: { kind: "plugin", plugin: PLUGIN_NAME, form: INJECTION_FORM },
      });
      return { ...decision, messages: [...decision.messages, memoryMessage] };
    } catch (error) {
      // 超时/离线：返回原 decision，对话不中断（doc/14 §4）。
      logger.warn(`compose 跳过注入: ${error instanceof Error ? error.message : String(error)}`);
      return decision;
    }
  };
}
