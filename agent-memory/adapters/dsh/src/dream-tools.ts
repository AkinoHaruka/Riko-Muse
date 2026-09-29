import { defineTool } from "@deepseek-ai/dsh-tools";
import type { ParameterSchemaSpec, ToolDefinition } from "@deepseek-ai/dsh-tools";
import type { MemoryClient } from "./client.js";
import type { DreamChildBinding } from "./dream-scope.js";

interface DreamToolAgent {
  id: string;
  session?: { header?: { origin?: string; parentSession?: unknown } };
}

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

const resultSchema = {
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

function render(_args: unknown, value: { ok: boolean; data?: Json; error?: { code: string; message: string } }) {
  return [{ type: "text" as const, text: JSON.stringify(value) }];
}

function toJson(value: unknown): Json {
  return JSON.parse(JSON.stringify(value ?? null)) as Json;
}

function fail(code: string, message: string) {
  return { ok: false, error: { code, message } };
}

function defineReadTool(
  client: MemoryClient,
  binding: DreamChildBinding,
  childAgentId: string,
  name: string,
  description: string,
  parameters: ParameterSchemaSpec,
  request: (args: Record<string, unknown>) => Record<string, unknown>,
): ToolDefinition {
  return defineTool({
    name,
    description,
    parameters,
    output: { schema: resultSchema, render },
    async execute(args, exec) {
      const agent = exec.agent as DreamToolAgent | undefined;
      if (!agent || agent.id !== childAgentId
        || agent.session?.header?.origin !== "subagent"
        || agent.session.header.parentSession === undefined) {
        return fail("DREAM_CHILD_ONLY", "该读取能力只对当前已授权的 Dream 子 Agent 开放");
      }
      const result = await client.dreamRead(binding.jobId, {
        runner_id: binding.runnerId,
        dream_generation: binding.generation,
        ...request(args),
      }, exec.signal);
      if (result.failure !== "ok" || result.status < 200 || result.status >= 300) {
        const body = result.body as { error?: { code?: string; message?: string } } | undefined;
        return fail(body?.error?.code ?? "DREAM_READ_UNAVAILABLE", body?.error?.message ?? "受限记忆读取失败；不得据此判断无匹配");
      }
      return { ok: true, data: toJson(result.body) };
    },
  });
}

export function buildDreamReadTools(
  client: MemoryClient,
  binding: DreamChildBinding,
  childAgentId: string,
): ToolDefinition[] {
  if (!binding.readToolsEnabled) return [];
  const tools: ToolDefinition[] = [
    defineReadTool(client, binding, childAgentId, "dream_read_manifest",
      "读取本 Dream job 冻结 evidence ID、角色、序号、purpose 和 fingerprint；不能读取其他 job。",
      {}, () => ({ operation: "manifest" })),
    defineReadTool(client, binding, childAgentId, "dream_read_evidence",
      "读取本 job 冻结的最多 20 条 evidence 原文。只引用 role=user 的内容，assistant/tool 仅供语境。",
      { ids: { type: "array", required: true, items: { type: "string" }, description: "来自 dream_read_manifest 的 evidence_id，最多 20 个" } },
      (args) => ({ operation: "evidence", ids: args.ids as string[] })),
    defineReadTool(client, binding, childAgentId, "dream_search_memories",
      "在本认证 scope 内搜索当前有效记忆。结果只是候选；只有本工具返回的 ID 才能作为读取目标。",
      { query: { type: "string", required: true }, limit: { type: "number" }, candidate_id: { type: "string", description: "adjudicate 阶段必须传当前候选的 candidate_id" } },
      (args) => ({ operation: "search_memories", query: args.query as string, limit: args.limit as number | undefined ?? 8, ...(args.candidate_id ? { candidate_id: args.candidate_id as string } : {}) })),
    defineReadTool(client, binding, childAgentId, "dream_get_memory",
      "读取此前 dream_search_memories 返回且已冻结版本的记忆详情与来源引用。",
      { id: { type: "string", required: true } },
      (args) => ({ operation: "get_memory", ids: [args.id as string] })),
    defineReadTool(client, binding, childAgentId, "dream_search_pages",
      "搜索当前有效的已发布派生页。结果只是候选，失效来源的页面不会返回。",
      { query: { type: "string", required: true }, limit: { type: "number" } },
      (args) => ({ operation: "search_pages", query: args.query as string, limit: args.limit as number | undefined ?? 5 })),
    defineReadTool(client, binding, childAgentId, "dream_get_page",
      "读取此前 dream_search_pages 返回且已冻结版本的页面详情。",
      { id: { type: "string", required: true } },
      (args) => ({ operation: "get_page", ids: [args.id as string] })),
  ];
  const allowed = binding.phase === "extract" || binding.phase === "redecision"
    ? new Set(["dream_read_manifest", "dream_read_evidence"])
    : binding.phase === "adjudicate"
      ? new Set(["dream_read_manifest", "dream_read_evidence", "dream_search_memories", "dream_get_memory"])
      : new Set(["dream_read_manifest", "dream_read_evidence", "dream_search_memories", "dream_get_memory", "dream_search_pages", "dream_get_page"]);
  return tools.filter((tool) => allowed.has(tool.name));
}
