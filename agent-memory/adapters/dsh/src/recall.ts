/**
 * 上下文注入（doc2/04 §4 + doc6/04、doc6/11，官方 @477b4f4 已核对）。
 *
 * 旧路径（injectionEnabled，默认）：`agent/pre-step` 中 `await next()` 后把旧
 * compose 结果**追加**在消息尾部，source.kind="agent-memory"/form="recall"。
 *
 * D6 v6 路径（contextBundleEnabled，doc6/06 §3）：
 * - `system-prompt/assemble` waterfall：每 step 异步 GET /v1/soul，非空正文 push
 *   唯一具名 section `agent-memory:soul`（interpolate:false，不 complete）。
 * - `agent/pre-step`：POST /v1/context/bundle（空 query 也取 resident），把
 *   resident/retrieved 作为独立、有来源的 user-role 消息**前插**在原始用户正文
 *   之前；正文 XML 转义并包裹 `<agent_memory kind=...>` 数据边界；不合并进
 *   用户原文、不回流 L0（events.ts 只认 source.kind==='user'）。
 * - 同一 decision 已有本插件消息 → 跳过；超时/离线不注入旧缓存。
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
    "agent-memory": { kind: "agent-memory"; form: "recall" | "resident" | "retrieved" };
  }
}

export const PLUGIN_KIND = "agent-memory";
/** Soul system section 唯一具名（doc6/03 §1）。 */
export const SOUL_SECTION_NAME = "agent-memory:soul";
/** Soul section 排序：在官方 persona suffix（10200）之后，不覆盖官方布局。 */
export const SOUL_SECTION_ORDER = 10300;

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

/** XML 数据边界内的文本转义（doc6/11 §5：正文作为数据，不得突破包裹结构）。 */
export function escapeXmlData(text: string): string {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function wrapData(kind: string, text: string): string {
  return `<agent_memory kind="${kind}">\n${escapeXmlData(text)}\n</agent_memory>`;
}

/**
 * D6 v6：`system-prompt/assemble` waterfall 监听（doc6/03 §1、doc6/06 §3）。
 * 事件签名（官方 @477b4f4）：`(assembly, context, next)`，返回值 authoritative。
 * 先 `next()` 让官方监听器完成，再对返回的 assembly 追加唯一
 * `agent-memory:soul` section（interpolate:false，不 complete，order 在官方
 * persona suffix 之后）；GET /v1/soul 受 context.signal 与 soulTimeoutMs 共同
 * 取消；超时/离线/取消不注入、不缓存旧正文（删除/改版下一步生效）；空正文
 * 不产生空 section。
 */
export function makeSoulAssembleHook(
  client: MemoryClient,
  logger: Logger,
  soulTimeoutMs: number,
  disabled: () => boolean,
  /** 部署级稳定 agent ID（doc6/03 §1）；空则回退会话 ID（人格按会话隔离）。 */
  agentName: string,
) {
  return async function soulAssemble(
    assembly: { sections: Array<Record<string, unknown>> },
    context: { agent?: { id?: unknown }; signal?: AbortSignal },
    next: () => Promise<{ sections: Array<Record<string, unknown>> }>,
  ): Promise<{ sections: Array<Record<string, unknown>> }> {
    void assembly;
    const result = await next();
    if (disabled()) return result;
    if (result.sections.some((s) => s.name === SOUL_SECTION_NAME)) return result;
    const fallback = context.agent?.id === undefined ? "" : String(context.agent.id);
    const agentId = agentName || fallback;
    if (!agentId) return result;
    try {
      const r = await client.soul(agentId, context.signal);
      if (r.failure !== "ok" || r.status !== 200) {
        logger.warn(`soul 跳过注入 status=${r.status} request_id=${r.requestId ?? "?"}`);
        return result;
      }
      const body = (r.body ?? {}) as { version?: number; body_md?: string };
      const bodyMd = body.body_md ?? "";
      if (bodyMd.length === 0) return result; // 空 Soul 不注入空壳（doc6/01 §3）
      result.sections.push({
        name: SOUL_SECTION_NAME,
        order: SOUL_SECTION_ORDER,
        text: `<agent_memory:soul version="${body.version ?? 0}">\n${escapeXmlData(bodyMd)}\n</agent_memory:soul>`,
        interpolate: false,
      });
      return result;
    } catch (error) {
      logger.warn(`soul 跳过注入: ${error instanceof Error ? error.message : String(error)}`);
      return result;
    }
  };
}

/**
 * D6 v6：`agent/pre-step` bundle 前插 hook（doc6/06 §3）。
 * 空 query 仍取 resident；resident/retrieved 非空正文各成一条独立、有来源的
 * user-role 消息前插在原始用户正文之前；超时/离线跳过注入；同 decision 去重。
 */
export function makeBundleHook(
  client: MemoryClient,
  logger: Logger,
  bundleTimeoutMs: number,
  /** 部署级稳定 agent ID（doc6/03 §1）；空则回退会话 ID。 */
  agentName: string,
) {
  return async function bundlePreStep(
    payload: PreStepPayload,
    next: () => Promise<PreStepDecision>,
  ): Promise<PreStepDecision> {
    const decision = await next();
    if (decision.kind !== "enter" || payload.signal.aborted) return decision;
    if (alreadyInjected(decision)) return decision;
    const query = latestOriginalUserText(payload.messages);
    const agentId = agentName || String(payload.agent.id);
    try {
      const ctrl = new AbortController();
      const timer = setTimeout(() => ctrl.abort(), bundleTimeoutMs);
      const onParentAbort = () => ctrl.abort();
      payload.signal.addEventListener("abort", onParentAbort, { once: true });
      try {
        const r = await client.contextBundle(
          {
            agent_id: agentId,
            query,
            // 预算用服务端默认（24/3000/8/2400，doc6/01 §4）；需要收窄再由配置加。
          },
          ctrl.signal,
        );
        if (r.failure !== "ok" || r.status !== 200) {
          logger.warn(`bundle 跳过注入 status=${r.status} request_id=${r.requestId ?? "?"}`);
          return decision;
        }
        const body = (r.body ?? {}) as { resident?: { text?: string }; retrieved?: { text?: string } };
        const messages: UserMessage[] = [];
        if (body.resident?.text) {
          messages.push(
            createUserMessage({
              content: [{ type: "text", text: wrapData("resident", body.resident.text) }],
              source: { kind: PLUGIN_KIND, form: "resident" },
            }),
          );
        }
        if (body.retrieved?.text) {
          messages.push(
            createUserMessage({
              content: [{ type: "text", text: wrapData("retrieved", body.retrieved.text) }],
              source: { kind: PLUGIN_KIND, form: "retrieved" },
            }),
          );
        }
        if (messages.length === 0) return decision; // 空结果不加占位（doc6/11 §5）
        // 前插：记忆上下文在原始用户正文之前（doc6/11 §2）；原消息对象不改写。
        return { ...decision, messages: [...messages, ...decision.messages] };
      } finally {
        clearTimeout(timer);
        payload.signal.removeEventListener("abort", onParentAbort);
      }
    } catch (error) {
      logger.warn(`bundle 跳过注入: ${error instanceof Error ? error.message : String(error)}`);
      return decision;
    }
  };
}
