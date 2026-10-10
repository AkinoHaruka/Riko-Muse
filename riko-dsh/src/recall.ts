/**
 * 上下文注入（doc2/04 §4 + doc6/04、doc6/11，官方 @477b4f4 已核对）。
 *
 * 旧路径（injectionEnabled，默认）：`agent/pre-step` 中 `await next()` 后把旧
 * compose 结果**追加**在消息尾部，source.kind="riko-memory"/form="recall"。
 *
 * D6 v6 路径（contextBundleEnabled，doc6/06 §3）：
 * - `system-prompt/assemble` waterfall：每 step 异步 GET /v1/soul，非空正文 push
 *   唯一具名 section `riko-dsh:soul`（interpolate:false，不 complete）。
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
import { isSubagentSessionHeader, type Logger } from "./events.js";

declare module "@deepseek-ai/dsh-llm" {
  interface MessageSourceMap {
    /** 记忆上下文注入：producer 自有 kind（v4 格式要求），事件线拒绝其为用户证据。 */
    "riko-memory": {
      kind: "riko-memory";
      // V2-H1（doc7/09 §3）：bundle 分段后每段有自己的 form，便于事件线区分来源。
      form: "recall" | "resident" | "retrieved" | "compact" | "alignment" | "relationships";
    };
  }
}

export const PLUGIN_KIND = "riko-memory";
/** Soul system section 唯一具名（doc6/03 §1）。 */
export const SOUL_SECTION_NAME = "riko-dsh:soul";
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

/** V2-H1：bundle 分段形状（服务端 doc7/09 §2 契约）。 */
export type BundleSegmentForm = "compact" | "alignment" | "relationships" | "retrieved";
export const SEGMENT_FORMS: readonly BundleSegmentForm[] = [
  "compact",
  "alignment",
  "relationships",
  "retrieved",
];

export interface BundleSegment {
  segment?: string;
  domain_id?: string;
  version?: number;
  char_count?: number;
  text?: string;
  body?: string;
  items?: Array<Record<string, unknown>>;
}

/**
 * 把一段渲染成注入正文。服务端已做好预算与去重，这里只负责**保真呈现**：
 * 逐字正文与稳定引用原样带出，不做摘要、不重排来源。
 */
export function segmentText(seg: BundleSegment): string {
  if (typeof seg.text === "string" && seg.text.trim().length > 0) return seg.text;
  if (typeof seg.body === "string" && seg.body.trim().length > 0) return seg.body;
  const items = Array.isArray(seg.items) ? seg.items : [];
  const lines: string[] = [];
  for (const item of items) {
    const refs = Array.isArray(item.stable_refs)
      ? item.stable_refs.filter((x): x is string => typeof x === "string")
      : [];
    const detailRef = typeof item.detail_ref === "string" ? item.detail_ref : "";
    const suffix = refs.length > 0 ? " <" + refs.join(" ") + ">" : detailRef ? " <" + detailRef + ">" : "";
    if (typeof item.body === "string") {
      lines.push("- " + item.body + suffix);
    } else if (typeof item.display_name === "string") {
      const relation = typeof item.relation === "string" ? "（" + item.relation + "）" : "";
      lines.push("- " + item.display_name + relation + suffix);
    }
  }
  return lines.join("\n");
}
function alreadyInjected(decision: PreStepDecision): boolean {
  if (decision.kind !== "enter") return false;
  return decision.messages.some((m) => m.source?.kind === PLUGIN_KIND);
}

export function makePreStepHook(client: MemoryClient, logger: Logger, composeTimeoutMs: number) {
  return async function preStep(payload: PreStepPayload, next: () => Promise<PreStepDecision>): Promise<PreStepDecision> {
    const decision = await next();
    if (isSubagentSessionHeader(payload.agent.session.header)) return decision;
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
 * `riko-dsh:soul` section（interpolate:false，不 complete，order 在官方
 * persona suffix 之后）；GET /v1/soul 受 context.signal 与 soulTimeoutMs 共同
 * 取消；超时/离线/取消不注入、不缓存旧正文（删除/改版下一步生效）；空正文
 * 不产生空 section。
 */
/** 部署级稳定 agent ID（doc6/03 §1）；空则回退会话 ID（人格按会话隔离）。 */
export function makeSoulAssembleHook(
  client: MemoryClient,
  logger: Logger,
  soulTimeoutMs: number,
  disabled: () => boolean,
  agentName: string,
) {
  return async function soulAssemble(
    assembly: { sections: Array<Record<string, unknown>> },
    context: {
      agent?: { id?: unknown; session?: { header?: { origin?: unknown; parentSession?: unknown } } };
      signal?: AbortSignal;
    },
    next: () => Promise<{ sections: Array<Record<string, unknown>> }>,
  ): Promise<{ sections: Array<Record<string, unknown>> }> {
    void assembly;
    const result = await next();
    // DSH assembles system prompt before pre-step. Exclude child sessions here,
    // before the first request, so a Dream child never receives user Soul.
    if (isSubagentSessionHeader(context.agent?.session?.header)) return result;
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
 * user-role 消息前插在原始用户正文之前；离线跳过注入，跟随 DSH signal 取消；同 decision 去重。
 */
/** 部署级稳定 agent ID（doc6/03 §1）；空则回退会话 ID。 */
export function makeBundleHook(
  client: MemoryClient,
  logger: Logger,
  agentName: string,
) {
  return async function bundlePreStep(
    payload: PreStepPayload,
    next: () => Promise<PreStepDecision>,
  ): Promise<PreStepDecision> {
    const decision = await next();
    if (isSubagentSessionHeader(payload.agent.session.header)) return decision;
    if (decision.kind !== "enter" || payload.signal.aborted) return decision;
    if (alreadyInjected(decision)) return decision;
    const query = latestOriginalUserText(payload.messages);
    const agentId = agentName || String(payload.agent.id);
    try {
      const r = await client.contextBundle(
        {
          agent_id: agentId,
          query,
          // 预算用服务端默认（24/3000/8/2400，doc6/01 §4）；需要收窄再由配置加。
        },
        payload.signal,
      );
      if (r.failure !== "ok" || r.status !== 200) {
        logger.warn(`bundle 跳过注入 status=${r.status} request_id=${r.requestId ?? "?"}`);
        return decision;
      }
      const body = (r.body ?? {}) as {
        resident?: { text?: string };
        retrieved?: { text?: string };
        segments?: BundleSegment[];
      };
      const messages: UserMessage[] = [];
      // V2-H1（doc7/09 §3）：服务端给了 segments 就按序逐段注入，每段一条独立消息；
      // 没有 segments（旧服务端）回退到 resident/retrieved 两段，行为与 D6 完全一致。
      // 预算与去重由服务端负责（唯一真相），这里不再二次裁剪。
      const segments = Array.isArray(body.segments)
        ? body.segments.filter((seg) => seg && typeof seg.segment === "string")
        : undefined;
      if (segments && segments.length > 0) {
        for (const seg of segments) {
          const form = seg.segment as BundleSegmentForm;
          if (!SEGMENT_FORMS.includes(form)) continue;
          const text = segmentText(seg);
          if (!text) continue; // 空段不加占位
          messages.push(
            createUserMessage({
              content: [{ type: "text", text: wrapData(form, text) }],
              source: { kind: PLUGIN_KIND, form },
            }),
          );
        }
      } else {
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
      }
      if (messages.length === 0) return decision; // 空结果不加占位（doc6/11 §5）
      // 前插：记忆上下文在原始用户正文之前（doc6/11 §2）；原消息对象不改写。
      return { ...decision, messages: [...messages, ...decision.messages] };
    } catch (error) {
      logger.warn(`bundle 跳过注入: ${error instanceof Error ? error.message : String(error)}`);
      return decision;
    }
  };
}
