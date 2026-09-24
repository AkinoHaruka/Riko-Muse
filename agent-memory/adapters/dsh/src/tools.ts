/**
 * 五个记忆工具（doc2/04，官方 defineTool 已核对 @477b4f4）。
 *
 * - 证据定位：exec.agent.session.id + exec.agent.id；最新 user/user 消息来自事件线维护的
 *   记录（不按"最后一条正文事件"猜，也不用 deriveMessages 反推）。
 * - 写工具等待该用户事件的内核 receipt（受 writeTimeoutMs 与 exec.signal 限制）；
 *   未 ack 返回"当前消息尚未入库，请重试"，不改用旧事件。
 * - correct 必填 old_quote + expected_version；forget 必填 expected_version + target_quote。
 * - HTTP 失败按 doc2/04 §1 分类：用户可处理 → {ok:false,...}；基础设施 → throw（带 request_id）。
 * - 输出固定为 JSON 对象（schema 与 execute 返回值一致），render 为文本块。
 */
import { defineTool } from "@deepseek-ai/dsh-tools";
import type { ToolDefinition } from "@deepseek-ai/dsh-tools";
import type { MemoryClient, ApiResult } from "./client.js";
import type { EventPipeline, Logger } from "./events.js";

export interface ToolServices {
  client: MemoryClient;
  pipeline: EventPipeline;
  logger: Logger;
  hostId: string;
  writeTimeoutMs: number;
}

export interface AgentLike {
  id: string;
  session: { id: string };
}

export interface ToolExecLike {
  agent?: AgentLike;
  signal: AbortSignal;
}

/** 与 defineTool 输出推导兼容的 JSON 值。 */
type Json = string | number | boolean | null | Json[] | { [key: string]: Json };

interface ToolResultBody {
  ok: boolean;
  data?: Json;
  error?: { code: string; message: string };
}

const RESULT_SCHEMA = {
  type: "object",
  additionalProperties: false,
  properties: {
    ok: { type: "boolean", required: true },
    data: { type: "json" },
    error: {
      type: "object",
      additionalProperties: false,
      properties: {
        code: { type: "string", required: true },
        message: { type: "string", required: true },
      },
    },
  },
} as const;

function renderResult(_args: unknown, value: ToolResultBody): { type: "text"; text: string }[] {
  return [{ type: "text", text: JSON.stringify(value) }];
}

/** 用户可处理错误码（按 HTTP 状态 + error.code 分类）。 */
const USER_FIXABLE = new Set([
  "QUOTE_MISMATCH",
  "STALE_USER_EVIDENCE",
  "VERSION_CONFLICT",
  "AMBIGUOUS_TARGET",
  "MEMORY_NOT_FOUND",
  "EVENT_CONFLICT",
  "INVALID_FIELD",
]);

function toJson(value: unknown): Json {
  return JSON.parse(JSON.stringify(value ?? null)) as Json;
}

function toToolResult(r: ApiResult): ToolResultBody {
  if (r.failure === "ok" && r.status >= 200 && r.status < 300) {
    return { ok: true, data: toJson(r.body) };
  }
  const body = (r.body ?? {}) as { error?: { code?: string; message?: string } };
  const code = body.error?.code ?? (r.failure === "unauthorized" ? "UNAUTHENTICATED" : "INTERNAL");
  const message = body.error?.message ?? `内核返回 status=${r.status} request_id=${r.requestId ?? "?"}`;
  if (USER_FIXABLE.has(code)) return { ok: false, error: { code, message } };
  // 基础设施/协议错误：抛出并记录 request_id，不把失败包装成成功。
  throw new Error(`agent-memory 内核错误 code=${code} request_id=${r.requestId ?? "?"}: ${message}`);
}

export function buildMemoryTools(svc: ToolServices): ToolDefinition[] {
  type EvidenceCtx =
    | { kind: "ok"; origin: { host_id: string; agent_id: string; session_id: string }; userEvidenceId: string }
    | { kind: "err"; body: ToolResultBody };
  const evidenceContext = async (exec: ToolExecLike): Promise<EvidenceCtx> => {
    const agent = exec.agent;
    if (!agent) return { kind: "err", body: { ok: false, error: { code: "NO_AGENT", message: "memory 工具需要 agent 会话上下文" } } };
    const sessionId = agent.session.id;
    const agentId = agent.id;
    const latest = svc.pipeline.latestUserOf(sessionId);
    if (!latest) {
      return { kind: "err", body: { ok: false, error: { code: "STALE_USER_EVIDENCE", message: "当前没有已捕获的用户消息，无法定位证据" } } };
    }
    const evidenceId = await svc.pipeline.awaitEvidenceId(sessionId, latest.seq, svc.writeTimeoutMs, exec.signal);
    if (!evidenceId) {
      return { kind: "err", body: { ok: false, error: { code: "STALE_USER_EVIDENCE", message: "当前消息尚未入库，请重试" } } };
    }
    return {
      kind: "ok",
      origin: { host_id: svc.hostId, agent_id: agentId, session_id: sessionId },
      userEvidenceId: evidenceId,
    };
  };

  return [
    defineTool({
      name: "memory_search",
      description: "搜索当前用户 active 记忆，返回简短摘要与 ID（含版本）。历史查询需明确历史词。",
      parameters: {
        query: { type: "string", required: true, description: "搜索词" },
        limit: { type: "number", description: "1-20，默认 5" },
        include_history: { type: "boolean", description: "是否含已被取代的历史记忆（需查询含历史词）" },
      },
      output: { schema: RESULT_SCHEMA, render: renderResult },
      async execute(args: { query: string; limit?: number; include_history?: boolean }, exec: ToolExecLike) {
        const r = await svc.client.search({
          query: args.query,
          limit: args.limit ?? 5,
          include_history: args.include_history ?? false,
        });
        void exec;
        return toToolResult(r);
      },
    }),
    defineTool({
      name: "memory_get",
      description: "取当前用户一条可见 active 记忆（跨用户 ID 返回 404）。",
      parameters: { id: { type: "string", required: true, description: "memory_id" } },
      output: { schema: RESULT_SCHEMA, render: renderResult },
      async execute(args: { id: string }) {
        return toToolResult(await svc.client.getMemory(args.id));
      },
    }),
    defineTool({
      name: "memory_remember",
      description: "只接受当前用户明确说出的内容；quote 必须是最近原始用户消息的连续片段。",
      parameters: {
        quote: { type: "string", required: true, description: "用户原话的连续片段" },
        kind: { type: "string", required: true, enum: ["fact", "preference", "instruction", "episode"] },
      },
      output: { schema: RESULT_SCHEMA, render: renderResult },
      async execute(args: { quote: string; kind: string }, exec: ToolExecLike) {
        const ctx = await evidenceContext(exec);
        if (ctx.kind === "err") return ctx.body;
        return toToolResult(
          await svc.client.remember({
            origin: ctx.origin,
            user_evidence_id: ctx.userEvidenceId,
            quote: args.quote,
            kind: args.kind,
          }),
        );
      },
    }),
    defineTool({
      name: "memory_correct",
      description: "纠错：最近用户消息须同时给出旧片段与新片段；旧记忆 superseded，新记忆 active。",
      parameters: {
        id: { type: "string", required: true, description: "memory_id" },
        expected_version: { type: "number", required: true, description: "来自 search/get 的版本（乐观锁）" },
        old_quote: { type: "string", required: true, description: "被替换的旧原话连续片段" },
        replacement_quote: { type: "string", required: true, description: "用户给出的新原话" },
      },
      output: { schema: RESULT_SCHEMA, render: renderResult },
      async execute(
        args: { id: string; expected_version: number; old_quote: string; replacement_quote: string },
        exec: ToolExecLike,
      ) {
        if (typeof args.expected_version !== "number") {
          return { ok: false, error: { code: "INVALID_FIELD", message: "expected_version 必填（先用 memory_search/memory_get 查版本）" } };
        }
        const ctx = await evidenceContext(exec);
        if (ctx.kind === "err") return ctx.body;
        return toToolResult(
          await svc.client.correct(args.id, {
            origin: ctx.origin,
            user_evidence_id: ctx.userEvidenceId,
            expected_version: args.expected_version,
            old_quote: args.old_quote,
            replacement_quote: args.replacement_quote,
          }),
        );
      },
    }),
    defineTool({
      name: "memory_forget",
      description: "遗忘：最近用户消息须明确含遗忘动词与目标片段；原始会话证据仍保留。",
      parameters: {
        id: { type: "string", required: true, description: "memory_id" },
        expected_version: { type: "number", required: true, description: "来自 search/get 的版本（乐观锁）" },
        target_quote: { type: "string", required: true, description: "用户实际指认的目标原话连续片段" },
      },
      output: { schema: RESULT_SCHEMA, render: renderResult },
      async execute(args: { id: string; expected_version: number; target_quote: string }, exec: ToolExecLike) {
        if (typeof args.expected_version !== "number") {
          return { ok: false, error: { code: "INVALID_FIELD", message: "expected_version 必填（先用 memory_search/memory_get 查版本）" } };
        }
        const ctx = await evidenceContext(exec);
        if (ctx.kind === "err") return ctx.body;
        return toToolResult(
          await svc.client.forget(args.id, {
            origin: ctx.origin,
            user_evidence_id: ctx.userEvidenceId,
            expected_version: args.expected_version,
            target_quote: args.target_quote,
          }),
        );
      },
    }),
  ];
}
