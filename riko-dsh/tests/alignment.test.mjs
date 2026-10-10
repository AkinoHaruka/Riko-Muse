import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { Context } from "@deepseek-ai/cordis";
import { createUserMessage } from "@deepseek-ai/dsh-llm";
import {
  ALIGNMENT_MESSAGE_KIND,
  makeAlignmentPreStepHook,
  registerAlignment,
} from "../dist/alignment.js";
import { makeBundleHook, makePreStepHook, PLUGIN_KIND } from "../dist/recall.js";

const logger = { warn() {}, error() {}, info() {} };
const userMessage = (text) => createUserMessage({
  content: [{ type: "text", text }],
  source: { kind: "user" },
});
const textOf = (message) => message.content.map((part) => part.text ?? "").join("");
const payload = (messages = [userMessage("question")]) => ({
  agent: { id: "agent-1", session: { header: {} } },
  messages,
  signal: new AbortController().signal,
});

async function dispatch(ctx, input = payload()) {
  return ctx.waterfall("agent/pre-step", input, async () => ({ kind: "enter", messages: input.messages }));
}

async function withAlignment(file, run) {
  const ctx = new Context();
  try {
    await ctx.plugin({
      name: "test-alignment",
      apply(pluginCtx) {
        registerAlignment(pluginCtx, file);
      },
    });
    await run(ctx);
  } finally {
    await ctx.fiber.dispose();
  }
}

async function waitFor(check, timeoutMs = 2000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await check()) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.fail("timed out waiting for alignment.md hot reload");
}

test("injects local guidance after memory context and before the original user message", async () => {
  const original = userMessage("question");
  const composeClient = {
    compose: async () => ({ failure: "ok", status: 200, body: { text: "remembered preference" } }),
  };
  const input = payload([original]);
  const alignment = makeAlignmentPreStepHook(() => "## Style\nBe concise.");
  const recall = makePreStepHook(composeClient, logger, 100);
  const result = await alignment(input, () => recall(input, async () => ({ kind: "enter", messages: [original] })));

  assert.equal(result.kind, "enter");
  assert.equal(result.messages.length, 3);
  assert.equal(result.messages[0], original);
  assert.equal(result.messages[1].source.kind, PLUGIN_KIND);
  assert.match(textOf(result.messages[1]), /remembered preference/);
  assert.equal(result.messages[2].source.kind, ALIGNMENT_MESSAGE_KIND);
  assert.match(textOf(result.messages[2]), /<twin_alignment>/);
  assert.match(textOf(result.messages[2]), /Be concise\./);

  const bundleClient = {
    contextBundle: async () => ({
      failure: "ok",
      status: 200,
      body: { segments: [{ segment: "compact", text: "memory bundle" }] },
    }),
  };
  const bundle = makeBundleHook(bundleClient, logger, "agent-1");
  const bundled = await alignment(input, () => bundle(input, async () => ({ kind: "enter", messages: [original] })));
  assert.deepEqual(
    bundled.messages.map((message) => message.source.kind),
    [PLUGIN_KIND, ALIGNMENT_MESSAGE_KIND, "user"],
  );
  assert.match(textOf(bundled.messages[0]), /memory bundle/);
});

test("registers alignment in pre-step and picks up alignment.md edits without restart", async () => {
  const dir = await mkdtemp(join(tmpdir(), "riko-dsh-alignment-"));
  const file = join(dir, "alignment.md");
  await writeFile(file, "First guidance.", "utf8");

  try {
    await withAlignment(file, async (ctx) => {
      const first = await dispatch(ctx);
      assert.equal(first.messages[0]?.source.kind, ALIGNMENT_MESSAGE_KIND);
      assert.match(textOf(first.messages[0]), /First guidance\./);

      await writeFile(file, "Updated guidance.", "utf8");
      await waitFor(async () => {
        const next = await dispatch(ctx);
        return textOf(next.messages[0]).includes("Updated guidance.");
      });

      const updated = await dispatch(ctx);
      assert.match(textOf(updated.messages[0]), /Updated guidance\./);
      assert.doesNotMatch(textOf(updated.messages[0]), /First guidance\./);
    });
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("leaves rejected, aborted, and subagent decisions unchanged", async () => {
  const hook = makeAlignmentPreStepHook(() => "private relationship guidance");
  const input = payload();
  const rejected = { kind: "reject" };
  assert.equal(await hook(input, async () => rejected), rejected);

  const abortedInput = { ...input, signal: AbortSignal.abort() };
  const aborted = { kind: "enter", messages: input.messages };
  assert.equal(await hook(abortedInput, async () => aborted), aborted);

  const childInput = {
    ...input,
    agent: { ...input.agent, session: { header: { origin: "subagent", parentSession: "parent" } } },
  };
  const childDecision = { kind: "enter", messages: input.messages };
  assert.equal(await hook(childInput, async () => childDecision), childDecision);
});
