import assert from "node:assert/strict";
import test from "node:test";
import { Context } from "../../../deepseek-harness/vendor/cordis/lib/index.js";
import { createScope } from "../../../deepseek-harness/packages/core/scope/lib/index.js";
import SystemPrompt from "../../../deepseek-harness/packages/core/system-prompt/lib/index.js";
import ToolRuntime from "../../../deepseek-harness/packages/core/tools/lib/index.js";
import { installDreamChildTools } from "../dist/dream-tools.js";
import { DreamRunner } from "../dist/dream-runner.js";

const binding = (phase, jobId) => ({
  runnerId: "runner", jobId, generation: 1, phase,
  parentAgentId: "parent", readToolsEnabled: true,
});

test("dead activation reports failure and disposes without submitting a Dream result", async () => {
  const agent = { id: "parent" };
  let disposed = 0;
  let submitted = 0;
  const failures = [];
  const ctx = {
    get: (name) => name === "agents" ? { get: (id) => id === "parent" ? agent : undefined } : undefined,
    subagents: { startActivation: async (spec) => {
      assert.equal(spec.delivery, "caller");
      assert.equal(spec.request.toolFilter, undefined);
      return { childId: "dead-child", result: Promise.reject(new Error("activation died")),
        dispose: async () => { disposed += 1; } };
    } },
  };
  const client = {
    dreamRunnerFailure: async (request) => { failures.push(request); return { failure: "ok", status: 200 }; },
    dreamSubmitCandidates: async () => { submitted += 1; },
  };
  const runner = new DreamRunner(ctx, client, { warn() {} }, "host", () => false);
  await runner.execute(agent, { phase: "extract", dream_job_id: "job", dream_generation: 2 },
    new AbortController().signal);
  assert.equal(disposed, 1);
  assert.equal(submitted, 0);
  assert.equal(failures.length, 1);
  assert.equal(failures[0].error_code, "SUBAGENT_FAILED");
});

const globalTool = {
  name: "global_probe", description: "global probe",
  parameters: { type: "object", properties: {} },
  output: { schema: { type: "string" }, render: (_a, value) => [{ type: "text", text: value }] },
  execute: async () => "ran",
};

test("Dream child schema and execution are isolated per concurrent child before prompt", async () => {
  const ctx = new Context();
  await ctx.plugin(SystemPrompt, {});
  await ctx.plugin(ToolRuntime);
  ctx.tools.register(globalTool);
  const scopes = new Map();
  const mint = async (id) => {
    let scope;
    const key = { id };
    await ctx.plugin(Object.assign((inner) => { scope = createScope(inner, key); },
      { inject: ["tools", "systemPrompt"] }));
    scopes.set(id, { scope, key });
    return scope;
  };
  const first = await mint("extract-child");
  const second = await mint("consolidate-child");
  const client = { dreamRead: async () => ({ failure: "ok", status: 200, body: {} }) };
  // The awaited creation listener runs before the caller can assemble a prompt.
  await Promise.all([
    Promise.resolve().then(() => installDreamChildTools(first.ctx, client, binding("extract", "a"), "extract-child")),
    Promise.resolve().then(() => installDreamChildTools(second.ctx, client, binding("consolidate", "b"), "consolidate-child")),
  ]);
  const names = (id) => ctx.tools.schemas(scopes.get(id).key).map((tool) => tool.name).sort();
  assert.deepEqual(names("extract-child"), ["dream_read_evidence", "dream_read_manifest"]);
  assert.deepEqual(names("consolidate-child"), [
    "dream_get_memory", "dream_get_page", "dream_read_evidence", "dream_read_manifest",
    "dream_search_memories", "dream_search_pages",
  ]);
  assert.deepEqual(ctx.tools.schemas().map((tool) => tool.name), ["global_probe"]);
  assert.equal(ctx.tools.get("global_probe", scopes.get("extract-child").key), undefined);
  const denied = await ctx.tools.execute({
    signal: new AbortController().signal, callId: "call-1", name: "global_probe",
    arguments: {}, agent: scopes.get("extract-child").key,
  });
  assert.match(denied.content[0].text, /unknown tool/);
  assert.equal(ctx.tools.get("dream_get_page", scopes.get("extract-child").key), undefined);
  await first.dispose();
  assert.deepEqual(names("extract-child"), ["global_probe"]);
  assert.deepEqual(names("consolidate-child").length, 6);
  await second.dispose();
});
