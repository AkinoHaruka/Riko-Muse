import { test } from "node:test";
import assert from "node:assert/strict";

const { decideRetry, clearRetryState } = await import("../dist/retry.js");

function failure(overrides = {}) {
  return { message: "error", code: "UNKNOWN", ...overrides };
}

test("401/403 never retry", () => {
  clearRetryState();
  assert.equal(decideRetry(failure({ status: 401 }), 0).action, undefined);
  assert.equal(decideRetry(failure({ status: 403 }), 0).action, undefined);
  // Even on first attempt
  assert.equal(decideRetry(failure({ status: 401 }), 5).action, undefined);
});

test("429 retries with exponential backoff, max 3", () => {
  clearRetryState();
  const r0 = decideRetry(failure({ status: 429 }), 0);
  assert.equal(r0.action?.kind, "retry");
  assert.ok(r0.delayMs >= 1000);

  const r1 = decideRetry(failure({ status: 429 }), 1);
  assert.equal(r1.action?.kind, "retry");
  assert.ok(r1.delayMs > r0.delayMs, "backoff should increase");

  const r2 = decideRetry(failure({ status: 429 }), 2);
  assert.equal(r2.action?.kind, "retry");

  // 4th attempt (count=3) should not retry
  assert.equal(decideRetry(failure({ status: 429 }), 3).action, undefined);
});

test("503 same as 429", () => {
  clearRetryState();
  assert.equal(decideRetry(failure({ status: 503 }), 0).action?.kind, "retry");
  assert.equal(decideRetry(failure({ status: 503 }), 3).action, undefined);
});

test("respects providerRetryAfterMs", () => {
  clearRetryState();
  const r = decideRetry(failure({ status: 429, providerRetryAfterMs: 5000 }), 0);
  assert.equal(r.delayMs, 5000);
});

test("timeout retries once", () => {
  clearRetryState();
  assert.equal(decideRetry(failure({ message: "request timeout" }), 0).action?.kind, "retry");
  assert.equal(decideRetry(failure({ message: "request timeout" }), 1).action, undefined);
  assert.equal(decideRetry(failure({ code: "ETIMEDOUT" }), 0).action?.kind, "retry");
});

test("other errors retry once", () => {
  clearRetryState();
  assert.equal(decideRetry(failure({ status: 500 }), 0).action?.kind, "retry");
  assert.equal(decideRetry(failure({ status: 500 }), 1).action, undefined);
});
