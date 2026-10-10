/**
 * 用户记忆工具（doc2/04，官方 defineTool 已核对 @477b4f4）。
 *
 * - 证据定位：exec.agent.session.id + exec.agent.id；最新 user/user 消息来自事件线维护的
 *   记录（不按"最后一条正文事件"猜，也不用 deriveMessages 反推）。
 * - 写工具等待该用户事件的内核 receipt（受 writeTimeoutMs 与 exec.signal 限制）；
 *   未 ack 返回"当前消息尚未入库，请重试"，不改用旧事件。
 * - correct 必填 old_quote + expected_version；forget 必填 expected_version + target_quote。
 * - HTTP 失败按 doc2/04 §1 分类：用户可处理 → {ok:false,...}；基础设施 → throw（带 request_id）。
 *   例外（doc5/09 决策 A）：memory_remember 的 409 STATE_CONFLICT 按端点窄映射为 {ok:false}，
 *   其余工具的同名错误仍 throw。
 * - 输出固定为 JSON 对象（schema 与 execute 返回值一致），render 为文本块。
 */
import { createHash } from "node:crypto";
import { defineTool } from "@deepseek-ai/dsh-tools";
import type { ToolDefinition } from "@deepseek-ai/dsh-tools";
import type { MemoryClient, ApiResult } from "./client.js";
import type { EventPipeline, LatestUserMessage, Logger } from "./events.js";

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

/** 用户可处理错误码（按 HTTP 状态 + error.code 分类）。错误码以 doc/12 与内核
 * memory-contract/src/lib.rs 为准：404 → NOT_FOUND（没有 MEMORY_NOT_FOUND 这个码值）。 */
const USER_FIXABLE = new Set([
  "QUOTE_MISMATCH",
  "STALE_USER_EVIDENCE",
  "VERSION_CONFLICT",
  "AMBIGUOUS_TARGET",
  "NOT_FOUND",
  "EVENT_CONFLICT",
  "INVALID_FIELD",
  "IDEMPOTENCY_CONFLICT",
]);

function toJson(value: unknown): Json {
  return JSON.parse(JSON.stringify(value ?? null)) as Json;
}

function toToolResult(
  r: ApiResult,
  opts?: { rememberStateConflict?: boolean; lifecycleStateConflict?: boolean },
): ToolResultBody {
  if (r.failure === "ok" && r.status >= 200 && r.status < 300) {
    return { ok: true, data: toJson(r.body) };
  }
  const body = (r.body ?? {}) as { error?: { code?: string; message?: string } };
  const code = body.error?.code ?? (r.failure === "unauthorized" ? "UNAUTHENTICATED" : "INTERNAL");
  const message = body.error?.message ?? `内核返回 status=${r.status} request_id=${r.requestId ?? "?"}`;
  // doc5/09 决策 A：仅 memory_remember 端点的 409 STATE_CONFLICT（SECRET/TEMPORAL/
  // 需保存指令/复合命题四类直写拒绝）是用户可理解的保存规则反馈，窄映射为
  // {ok:false,error}；其他工具的同名错误不在此列，仍走 throw 路径。
  if (opts?.rememberStateConflict && r.status === 409 && code === "STATE_CONFLICT") {
    return { ok: false, error: { code, message } };
  }
  if (opts?.lifecycleStateConflict && r.status === 409 && code === "STATE_CONFLICT") {
    return { ok: false, error: { code, message } };
  }
  if (USER_FIXABLE.has(code)) return { ok: false, error: { code, message } };
  // 基础设施/协议错误：抛出并记录 request_id，不把失败包装成成功。
  throw new Error(`riko-memory 内核错误 code=${code} request_id=${r.requestId ?? "?"}: ${message}`);
}

export function buildMemoryTools(svc: ToolServices): ToolDefinition[] {
  type EvidenceCtx =
    | {
        kind: "ok";
        origin: { host_id: string; agent_id: string; session_id: string };
        userEvidenceId: string;
        latestUser: LatestUserMessage;
      }
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
      latestUser: latest,
    };
  };

  const instructionSpan = (latest: LatestUserMessage, quote: string): [number, number] | undefined => {
    if (quote.length === 0) return undefined;
    const first = latest.content.indexOf(quote);
    if (first < 0 || latest.content.indexOf(quote, first + 1) >= 0) return undefined;
    const startByte = Buffer.byteLength(latest.content.slice(0, first), "utf8");
    return [startByte, startByte + Buffer.byteLength(quote, "utf8")];
  };
  const lifecycleKey = (
    op: string,
    memoryId: string,
    expectedVersion: number,
    ctx: Extract<EvidenceCtx, { kind: "ok" }>,
    span: [number, number],
  ) =>
    `dsh-${op}-${createHash("sha256")
      .update(
        `${memoryId}\0${expectedVersion}\0${ctx.userEvidenceId}\0${span[0]}\0${span[1]}\0${ctx.latestUser.messageId}`,
      )
      .digest("hex")}`;

  return [
    defineTool({
      name: "memory_search",
      description: "需要回忆已保存内容时调用。默认搜索 active 记忆；只有用户明确询问旧值或历史版本时才启用 include_history。",
      parameters: {
        query: { type: "string", required: true, description: "描述要查找的记忆内容" },
        limit: { type: "number", description: "1-20，默认 5" },
        include_history: { type: "boolean", description: "仅用户明确询问已被纠正、替代或历史版本的内容时设为 true；其他情况省略或设为 false" },
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
      description: "核对单条记忆详情时调用；ID 须来自 memory_search 或记忆工具的成功回执，不要猜 ID。只读当前 scope 可见的 active 记忆。",
      parameters: { id: { type: "string", required: true, description: "memory_search 或记忆工具回执中的 memory_id" } },
      output: { schema: RESULT_SCHEMA, render: renderResult },
      async execute(args: { id: string }) {
        return toToolResult(await svc.client.getMemory(args.id));
      },
    }),
    defineTool({
      name: "memory_explain",
      description:
        "用户问「你从哪知道的 / 之前那条是什么 / 依据是什么」时调用。返回逐字证据（span 精确的原文引文）、说话角色、版本与取代关系、保存原因码与稳定引用。只读当前 scope 与读域集；forgotten/已删除内容不会返回。",
      parameters: {
        id: {
          type: "string",
          required: true,
          description: "memory_id（来自 memory_search/memory_get）或服务端返回的 riko:// 稳定引用；不要自造",
        },
        history: {
          type: "boolean",
          description: "仅当用户明确询问已被纠正/替代的旧版本时设为 true；缺省只读当前有效版本",
        },
      },
      output: { schema: RESULT_SCHEMA, render: renderResult },
      async execute(args: { id: string; history?: boolean }) {
        return toToolResult(await svc.client.explainMemory(args.id, args.history ?? false));
      },
    }),
    defineTool({
      name: "memory_relationships",
      description:
        "涉及具体人物或群组的事实、推荐、计划时调用。action=index 先看有预算的索引；action=resolve 用名字/昵称/角色称呼解析实体（歧义时返回候选，不要猜）；action=get 按 entity_id 读详情。详情里每条都带来源引用；回答前先读当前页。",
      parameters: {
        action: { type: "string", required: true, description: "index | resolve | get" },
        query: { type: "string", description: "action=resolve 时的名称/昵称/角色称呼" },
        entity_id: { type: "string", description: "action=get 时的 entity_id（来自 index/resolve，不要自造）" },
        expected_version: { type: "number", description: "action=get 时可选；与当前版本不符会返回 409" },
        limit: { type: "number", description: "action=index 时的条数上限" },
      },
      output: { schema: RESULT_SCHEMA, render: renderResult },
      async execute(args: {
        action: string;
        query?: string;
        entity_id?: string;
        expected_version?: number;
        limit?: number;
      }) {
        if (args.action === "index") {
          return toToolResult(await svc.client.relationshipsIndex(args.limit));
        }
        if (args.action === "resolve") {
          if (!args.query) {
            return { ok: false, error: { code: "INVALID_FIELD", message: "action=resolve 需要 query" } };
          }
          return toToolResult(await svc.client.resolveRelationship(args.query));
        }
        if (args.action === "get") {
          if (!args.entity_id) {
            return { ok: false, error: { code: "INVALID_FIELD", message: "action=get 需要 entity_id" } };
          }
          return toToolResult(await svc.client.relationshipDetail(args.entity_id, args.expected_version));
        }
        return { ok: false, error: { code: "INVALID_FIELD", message: "action 必须是 index|resolve|get" } };
      },
    }),
    defineTool({
      name: "memory_facets",
      description:
        "需要按经历/观点/反思/处境四个分面查看已保存内容时调用（按需读取，不常驻）。缺省返回四段；每段只含当前有效来源，来源一改即不再返回。",
      parameters: {
        kind: { type: "string", description: "experience | opinions | reflections | world；缺省返回四段" },
      },
      output: { schema: RESULT_SCHEMA, render: renderResult },
      async execute(args: { kind?: string }) {
        return toToolResult(await svc.client.facets(args.kind));
      },
    }),    defineTool({
      name: "memory_remember",
      description: "持久保存本轮用户内容时调用。quote 必须是最近原始用户消息中的连续原文，kind 按原话选择；只有返回 ok=true 才能说已保存。",
      parameters: {
        quote: { type: "string", required: true, description: "最近原始用户消息中的连续原文，不要改写或拼接" },
        kind: {
          type: "string",
          required: true,
          enum: ["fact", "preference", "instruction", "episode"],
          description: "按原话选择：fact=事实，preference=偏好，instruction=指令，episode=经历",
        },
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
          { rememberStateConflict: true },
        );
      },
    }),
    defineTool({
      name: "memory_correct",
      description: "用户明确纠正已保存内容时调用。先查目标 ID 和版本；old_quote、replacement_quote 分别引用本轮更正中的旧值与新值。仅在 ok=true 后确认完成。",
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
      description: "仅用户明确要求忘记时调用。先查目标 ID 和版本，再引用本轮消息中唯一指认目标的原文；原始会话证据仍保留。仅在 ok=true 后确认完成。",
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
    defineTool({
      name: "memory_retire",
      description: "仅用户明确要求停用时调用。先查当前版本，再引用本轮消息中唯一的停用指令；停用保留记录，不等于删除。仅在 ok=true 后确认完成。",
      parameters: {
        id: { type: "string", required: true, description: "memory_id" },
        expected_version: { type: "number", required: true, description: "来自 memory_search/memory_get 的当前版本" },
        instruction_quote: { type: "string", required: true, description: "最新用户消息中唯一出现的连续原文指令片段" },
      },
      output: { schema: RESULT_SCHEMA, render: renderResult },
      async execute(
        args: { id: string; expected_version: number; instruction_quote: string },
        exec: ToolExecLike,
      ) {
        if (!Number.isSafeInteger(args.expected_version) || args.expected_version <= 0) {
          return { ok: false, error: { code: "INVALID_FIELD", message: "expected_version 必须是正整数，请先查当前记忆版本" } };
        }
        const ctx = await evidenceContext(exec);
        if (ctx.kind === "err") return ctx.body;
        const span = instructionSpan(ctx.latestUser, args.instruction_quote);
        if (!span) {
          return { ok: false, error: { code: "AMBIGUOUS_TARGET", message: "指令片段必须在最新用户消息中唯一、逐字出现" } };
        }
        return toToolResult(
          await svc.client.retire(args.id, {
            expected_version: args.expected_version,
            idempotency_key: lifecycleKey("retire", args.id, args.expected_version, ctx, span),
            origin: ctx.origin,
            user_evidence_id: ctx.userEvidenceId,
            target_quote: args.instruction_quote,
            start_byte: span[0],
            end_byte: span[1],
          }),
          { lifecycleStateConflict: true },
        );
      },
    }),
    defineTool({
      name: "memory_restore",
      description: "仅用户明确要求恢复已退休记忆时调用。先查当前版本，再引用本轮消息中唯一的恢复指令。仅在 ok=true 后确认完成。",
      parameters: {
        id: { type: "string", required: true, description: "memory_id" },
        expected_version: { type: "number", required: true, description: "来自 memory_search/memory_get 的当前版本" },
        instruction_quote: { type: "string", required: true, description: "最新用户消息中唯一出现的连续原文恢复指令片段" },
      },
      output: { schema: RESULT_SCHEMA, render: renderResult },
      async execute(
        args: { id: string; expected_version: number; instruction_quote: string },
        exec: ToolExecLike,
      ) {
        if (!Number.isSafeInteger(args.expected_version) || args.expected_version <= 0) {
          return { ok: false, error: { code: "INVALID_FIELD", message: "expected_version 必须是正整数，请先查当前记忆版本" } };
        }
        const ctx = await evidenceContext(exec);
        if (ctx.kind === "err") return ctx.body;
        const span = instructionSpan(ctx.latestUser, args.instruction_quote);
        if (!span) {
          return { ok: false, error: { code: "AMBIGUOUS_TARGET", message: "指令片段必须在最新用户消息中唯一、逐字出现" } };
        }
        return toToolResult(
          await svc.client.restore(args.id, {
            expected_version: args.expected_version,
            idempotency_key: lifecycleKey("restore", args.id, args.expected_version, ctx, span),
            origin: ctx.origin,
            user_evidence_id: ctx.userEvidenceId,
            target_quote: args.instruction_quote,
            start_byte: span[0],
            end_byte: span[1],
          }),
          { lifecycleStateConflict: true },
        );
      },
    }),
  ];
}
