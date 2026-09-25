/**
 * 适配器事件线回归测试（node --test，对 dist/ 运行；先 npm run build）。
 *
 * dispose 宽限回归（doc-handoff/06 F4）：one-shot 进程退出时，turn/end 的
 * flush 曾因「先置 stopped 再等待」被留在 spool，记忆可用性滞后一轮。
 * 修复后 dispose 应先等队列与在途链排空，flush 在本进程内真实发出。
 */
import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { EventPipeline } from "../dist/events.js";
import { Spool } from "../dist/spool.js";

const ok = (extra = {}) => ({ failure: "ok", status: 200, requestId: "req-test", ...extra });

function fakeClient() {
  const calls = { version: 0, recordEvent: 0, flush: 0 };
  return {
    calls,
    async version() { calls.version += 1; return ok({ body: { protocol_version: 1 } }); },
    async recordEvent() { calls.recordEvent += 1; return ok({ status: 201, body: { evidence_id: "ev-1" } }); },
    async flush() { calls.flush += 1; return ok({ body: {} }); },
  };
}

const logger = { warn() {}, error() {}, info() {} };

test("dispose 让 turn/end flush 在本进程内发出", async () => {
  const dir = mkdtempSync(join(tmpdir(), "am-dispose-"));
  try {
    const spool = new Spool(dir);
    const client = fakeClient();
    const pipeline = new EventPipeline(spool, client, logger, "test-host", true);
    await pipeline.start(undefined);
    pipeline.observeSessionEvent("s1", {
      type: "user/message", seq: 8, time: Date.now(),
      data: { source: { kind: "user" }, content: [{ type: "text", text: "你好" }] },
    });
    pipeline.observeSessionEvent("s1", { type: "turn/end" });
    await pipeline.dispose();
    assert.equal(client.calls.recordEvent, 1, "用户事件应已发出");
    assert.equal(client.calls.flush, 1, "turn/end flush 应在 dispose 宽限期内发出，而不是留在 spool");
    const receiptsPath = join(dir, "receipts.jsonl");
    assert.ok(existsSync(receiptsPath), "receipts 应已落盘");
    const receipts = readFileSync(receiptsPath, "utf8").trim().split("\n").map((l) => JSON.parse(l));
    const flushReceipt = receipts.find((r) => r.kind === "flush");
    assert.ok(flushReceipt, "flush receipt 应已写入");
    assert.match(flushReceipt.opId, /flush:8$/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
