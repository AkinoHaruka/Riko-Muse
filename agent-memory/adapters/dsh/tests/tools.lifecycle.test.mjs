import assert from "node:assert/strict";
import test from "node:test";
import { buildMemoryTools } from "../dist/tools.js";

function setup(content = "请把这条记忆退休。") {
  const captured = [];
  const latestUser = { seq: 12, messageId: "user-message-12", content };
  const pipeline = {
    latestUserOf: () => latestUser,
    awaitEvidenceId: async () => "evidence-12",
  };
  const client = {
    retire: async (id, body) => {
      captured.push({ op: "retire", id, body });
      return { failure: "ok", status: 200, body: { memory_id: id, changed: true } };
    },
    restore: async (id, body) => {
      captured.push({ op: "restore", id, body });
      return { failure: "ok", status: 200, body: { memory_id: id, changed: true } };
    },
  };
  const tools = buildMemoryTools({
    client,
    pipeline,
    logger: { warn() {}, error() {} },
    hostId: "dsh-test",
    writeTimeoutMs: 100,
  });
  const exec = {
    agent: { id: "agent-stable", session: { id: "session-1" } },
    signal: new AbortController().signal,
  };
  return { tools, exec, captured, latestUser };
}

test("retire and restore send exact UTF-8 instruction byte spans and receipts", async () => {
  const { tools, exec, captured, latestUser } = setup("前缀：请把这条记忆退休。然后继续。");
  const retire = tools.find((tool) => tool.name === "memory_retire");
  const restore = tools.find((tool) => tool.name === "memory_restore");
  assert.ok(retire);
  assert.ok(restore);

  await retire.execute({ id: "memory-1", expected_version: 4, instruction_quote: "把这条记忆退休" }, exec);
  await retire.execute({ id: "memory-1", expected_version: 4, instruction_quote: "把这条记忆退休" }, exec);
  await restore.execute({ id: "memory-1", expected_version: 4, instruction_quote: "把这条记忆退休" }, exec);

  assert.equal(captured.length, 3);
  for (const { body } of captured) {
    const start = Buffer.byteLength(latestUser.content.slice(0, latestUser.content.indexOf("把这条记忆退休")), "utf8");
    const end = start + Buffer.byteLength("把这条记忆退休", "utf8");
    assert.equal(body.user_evidence_id, "evidence-12");
    assert.equal(body.target_quote, "把这条记忆退休");
    assert.equal(body.start_byte, start);
    assert.equal(body.end_byte, end);
    assert.equal(body.expected_version, 4);
    assert.match(body.idempotency_key, /^dsh-(retire|restore)-[a-f0-9]{64}$/);
  }
  assert.equal(captured[0].body.idempotency_key, captured[1].body.idempotency_key);
  assert.notEqual(captured[0].body.idempotency_key, captured[2].body.idempotency_key);
});

test("lifecycle tools reject absent or ambiguous quotes without a write", async () => {
  const { tools, exec, captured } = setup("请把这条记忆退休，再把这条记忆退休。");
  const retire = tools.find((tool) => tool.name === "memory_retire");
  assert.ok(retire);

  const result = await retire.execute(
    { id: "memory-1", expected_version: 4, instruction_quote: "这条记忆退休" },
    exec,
  );
  assert.equal(result.error.code, "AMBIGUOUS_TARGET");
  assert.equal(captured.length, 0);
});

test("lifecycle tools require a positive integer version", async () => {
  const { tools, exec, captured } = setup();
  const restore = tools.find((tool) => tool.name === "memory_restore");
  assert.ok(restore);

  const result = await restore.execute(
    { id: "memory-1", expected_version: 0, instruction_quote: "这条记忆" },
    exec,
  );
  assert.equal(result.error.code, "INVALID_FIELD");
  assert.equal(captured.length, 0);
});
