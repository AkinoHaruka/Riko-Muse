import { test } from "node:test";
import assert from "node:assert/strict";

const { decideApproval } = await import("../dist/approval.js");

test("non-sensitive tools return null (pass through)", () => {
  assert.equal(decideApproval("read_file"), null);
  assert.equal(decideApproval("list_directory"), null);
  assert.equal(decideApproval("memory_search"), null);
});

test("sensitive tools return ask decision", () => {
  const d1 = decideApproval("rm");
  assert.equal(d1?.kind, "ask");
  assert.ok(d1.displayReason.en.includes("危险"));

  const d2 = decideApproval("curl");
  assert.equal(d2?.kind, "ask");

  const d3 = decideApproval("sudo");
  assert.equal(d3?.kind, "ask");
});

test("ask decision has personality-styled prompt", () => {
  const d = decideApproval("rm -rf /");
  assert.equal(d?.kind, "ask");
  // Should NOT be a dry "Allow?" - should have personality
  assert.ok(!d.displayReason.en.includes("Allow?"));
  assert.ok(d.displayReason.en.length > 20);
});

test("case insensitive matching", () => {
  assert.equal(decideApproval("RM")?.kind, "ask");
  assert.equal(decideApproval("Curl")?.kind, "ask");
});
