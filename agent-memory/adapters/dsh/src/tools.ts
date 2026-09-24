/**
 * 按需工具注册（doc/05 §6、doc/14 §5）。
 *
 * 已核对的宿主事实（riko-memory/src/index.ts:641-702）：
 * - defineTool({name, description, parameters, output, execute})，经 ctx.tools.register 注册；
 * - execute(args, exec) 中 exec.agent.session 存在；
 * - latestUserText 遍历 session.deriveMessages() 反向找 role=user 且 source.kind=user。
 *
 * 本实现的 user_evidence_id 来自 EventPipeline 的本地映射（内核返回的 evidence_id），
 * 不靠文字相等猜 ID。找不到最新用户证据时写操作直接拒绝（doc/14 §5）。
 */
import type { MemoryClient } from "./client.js";
import type { EventPipeline, Logger } from "./events.js";

export interface ToolDef {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
  execute: (args: Record<string, unknown>, exec: { agent?: unknown }) => Promise<unknown>;
}

export interface ToolHost {
  register(def: ToolDef): void;
  /** 从宿主 session 对象取 session.id。 */
  sessionId(session: unknown): string;
  /** 宿主会话最近真实用户消息文本（deriveMessages 反向找 source.kind=user）。 */
  latestUserText(session: unknown): string;
}

export function registerMemoryTools(
  host: ToolHost,
  client: MemoryClient,
  pipeline: EventPipeline,
  logger: Logger,
): void {
  const evidenceFor = (session: unknown): { sessionId: string; evidenceId?: string; userText: string } => {
    const sessionId = host.sessionId(session);
    const userText = host.latestUserText(session);
    const seq = pipeline.latestSeqFor(sessionId);
    const evidenceId = seq === undefined ? undefined : pipeline.evidenceIdFor(sessionId, seq);
    return { sessionId, evidenceId, userText };
  };

  const originOf = (agentId: string, sessionId: string) => ({
    host_id: "dsh",
    agent_id: agentId,
    session_id: sessionId,
  });

  host.register({
    name: "memory_search",
    description: "搜索当前用户 active 记忆，返回简短摘要与 ID。",
    parameters: {
      query: { type: "string", required: true, description: "搜索词" },
      limit: { type: "number", description: "1-20，默认 5" },
    },
    execute: async (args, exec) => {
      const agentId = exec.agent ? "unknown-agent" : "unknown-agent";
      void agentId;
      const r = await client.search({
        query: String(args.query ?? ""),
        limit: typeof args.limit === "number" ? args.limit : 5,
      });
      return r.body;
    },
  });

  host.register({
    name: "memory_get",
    description: "取当前用户单条可见 active 记忆和证据概要。",
    parameters: { id: { type: "string", required: true, description: "memory_id" } },
    execute: async (args) => {
      const r = await client.getMemory(String(args.id ?? ""));
      return r.body;
    },
  });

  host.register({
    name: "memory_remember",
    description:
      "只接受当前用户明确说出的内容；quote 必须来自最近原始用户消息。Agent 自己编造的内容会被拒绝。",
    parameters: {
      quote: { type: "string", required: true, description: "用户原话的连续片段" },
      kind: { type: "string", required: true, enum: ["fact", "preference", "instruction", "episode"] },
    },
    execute: async (args, exec) => {
      if (!exec.agent) throw new Error("memory tool requires an agent session");
      const ctx = evidenceFor(exec.agent);
      if (!ctx.evidenceId) {
        return { status: "stale", reason: "当前用户消息尚未入库，请重试" };
      }
      const r = await client.remember({
        origin: originOf("unknown-agent", ctx.sessionId),
        user_evidence_id: ctx.evidenceId,
        quote: String(args.quote ?? ""),
        kind: String(args.kind ?? "fact"),
      });
      return r.body;
    },
  });

  host.register({
    name: "memory_correct",
    description: "纠错：要求最近用户消息明确给出替代内容。",
    parameters: {
      id: { type: "string", required: true, description: "memory_id" },
      replacement_quote: { type: "string", required: true, description: "用户给出的新原话" },
      expected_version: { type: "number", description: "期望版本（乐观锁）" },
    },
    execute: async (args, exec) => {
      if (!exec.agent) throw new Error("memory tool requires an agent session");
      const ctx = evidenceFor(exec.agent);
      if (!ctx.evidenceId) {
        return { status: "stale", reason: "当前用户消息尚未入库，请重试" };
      }
      const body: Record<string, unknown> = {
        origin: originOf("unknown-agent", ctx.sessionId),
        user_evidence_id: ctx.evidenceId,
        replacement_quote: String(args.replacement_quote ?? ""),
      };
      if (typeof args.expected_version === "number") body.expected_version = args.expected_version;
      const r = await client.correct(String(args.id ?? ""), body);
      return r.body;
    },
  });

  host.register({
    name: "memory_forget",
    description: "遗忘：要求最近用户消息明确指定目标，原始会话仍保留。",
    parameters: {
      id: { type: "string", required: true, description: "memory_id" },
      target_quote: { type: "string", required: true, description: "用户指认目标的原话片段" },
      expected_version: { type: "number", description: "期望版本（乐观锁）" },
    },
    execute: async (args, exec) => {
      if (!exec.agent) throw new Error("memory tool requires an agent session");
      const ctx = evidenceFor(exec.agent);
      if (!ctx.evidenceId) {
        return { status: "stale", reason: "当前用户消息尚未入库，请重试" };
      }
      const body: Record<string, unknown> = {
        origin: originOf("unknown-agent", ctx.sessionId),
        user_evidence_id: ctx.evidenceId,
        target_quote: String(args.target_quote ?? ""),
      };
      if (typeof args.expected_version === "number") body.expected_version = args.expected_version;
      const r = await client.forget(String(args.id ?? ""), body);
      return r.body;
    },
  });

  void logger;
}
