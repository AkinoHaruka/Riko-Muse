import { test } from "node:test";
import assert from "node:assert/strict";

const { SUMMARY_RETENTION_CHECKLIST, POST_COMPACTION_INJECTION_ORDER } = await import("../dist/compaction.js");

test("retention checklist covers key categories", () => {
  const list = [...SUMMARY_RETENTION_CHECKLIST];
  assert.ok(list.some((s) => s.includes("decisions")));
  assert.ok(list.some((s) => s.includes("todos")));
  assert.ok(list.some((s) => s.includes("preferences")));
  assert.ok(list.some((s) => s.includes("corrections")));
  assert.ok(list.some((s) => s.includes("goal")));
});

test("injection order is summary-first", () => {
  const order = [...POST_COMPACTION_INJECTION_ORDER];
  assert.equal(order[0], "summary");
  assert.ok(order.includes("memory_snapshot"));
  assert.ok(order.includes("goal_state"));
});
