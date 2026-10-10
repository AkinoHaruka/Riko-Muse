/**
 * 主动 flush 与捕获光标回归（doc4/03 §5、卡 D4-5；node --test，对 dist/ 运行）。
 *
 * 覆盖：80 事件/24 KiB 阈值触发 flush 且 through/opId 递增；队列满拒收不推进
 * lastBodySeq/latestUser/计数（不 flush 到未入队事件）；前序事件 receipt ack 前
 * 不发送 flush；重启后 spool 重放不丢 flush。
 */
import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { EventPipeline, isCapturableSessionHeader } from "../dist/events.js";
import { Spool } from "../dist/spool.js";

const ok = (extra = {}) => ({ failure: "ok", status: 200, requestId: "req-test", ...extra });

function fakeClient(overrides = {}) {
  const calls = { version: 0, recordEvent: 0, flush: 0, dreamTrigger: 0 };
  const flushRequests = [];
  const recordRequests = [];
  const dreamRequests = [];
  return {
    calls,
    flushRequests,
    recordRequests,
    dreamRequests,
    order: [],
    async version() { calls.version += 1; return ok({ body: { protocol_version: 1 } }); },
    async recordEvent(request) {
      calls.recordEvent += 1;
      this.order.push("event");
      recordRequests.push(request);
      if (overrides.recordEvent) return overrides.recordEvent(request);
      return ok({ status: 201, body: { evidence_id: "ev" } });
    },
    async flush(request) {
      calls.flush += 1;
      this.order.push("flush");
      flushRequests.push(request);
      if (overrides.flush) return overrides.flush(request);
      return ok({ body: {} });
    },
    async dreamTrigger(request) {
      calls.dreamTrigger += 1;
      this.order.push("dream");
      dreamRequests.push(request);
      if (overrides.dreamTrigger) return overrides.dreamTrigger(request);
      return ok({ status: 202, body: { status: "trigger_queued" } });
    },
  };
}

const quiet = { warn() {}, error() {}, info() {} };

test("L0 capture excludes DSH subagent and fork session headers", () => {
  assert.equal(isCapturableSessionHeader({}), true);
  assert.equal(isCapturableSessionHeader({ origin: "subagent" }), false);
  assert.equal(isCapturableSessionHeader({ parentSession: "parent-session" }), false);
  assert.equal(isCapturableSessionHeader(undefined), false);
});

function spyLogger() {
  return {
    warns: [],
    warn(m) { this.warns.push(m); },
    error() {},
    info() {},
  };
}

function userEvent(seq, text) {
  return { type: "user/message", seq, time: Date.now(), data: { source: { kind: "user" }, content: [{ type: "text", text }] } };
}

test("80 事件阈值触发主动 flush，多次 flush 的 through 递增", async () => {
  const dir = mkdtempSync(join(tmpdir(), "am-flush-80-"));
  try {
    const spool = new Spool(dir);
    const client = fakeClient();
    const pipeline = new EventPipeline(spool, client, quiet, "h", true);
    await pipeline.start(undefined);
    for (let seq = 1; seq <= 81; seq++) {
      pipeline.observeSessionEvent("s1", userEvent(seq, `消息${seq}`));
    }
    pipeline.observeSessionEvent("s1", { type: "turn/end" });
    await pipeline.dispose();
    assert.ok(client.calls.flush >= 2, `应至少两次 flush，实际 ${client.calls.flush}`);
    // 第 80 个事件后触发阈值 flush（through=80），turn/end flush（through=81）。
    assert.equal(client.flushRequests[0].through_event_seq, 80, "阈值 flush 应覆盖到第 80 个事件");
    const last = client.flushRequests[client.flushRequests.length - 1];
    assert.equal(last.through_event_seq, 81, "turn/end flush 到最后事件");
    const throughs = client.flushRequests.map((r) => r.through_event_seq);
    assert.deepEqual([...throughs].sort((a, b) => a - b), throughs, "多次 flush 的 through 必须递增");
    assert.equal(client.calls.recordEvent, 81, "全部事件都已发送");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("24 KiB 字节阈值触发主动 flush", async () => {
  const dir = mkdtempSync(join(tmpdir(), "am-flush-bytes-"));
  try {
    const spool = new Spool(dir);
    const client = fakeClient();
    const pipeline = new EventPipeline(spool, client, quiet, "h", true);
    await pipeline.start(undefined);
    const big = "x".repeat(12 * 1024); // 两条的请求 JSON 估算字节 > 24 KiB
    pipeline.observeSessionEvent("s1", userEvent(1, big));
    assert.equal(client.calls.flush, 0, "第一条未达阈值");
    pipeline.observeSessionEvent("s1", userEvent(2, big));
    pipeline.observeSessionEvent("s1", { type: "turn/end" });
    await pipeline.dispose();
    assert.ok(client.calls.flush >= 1, "字节阈值应触发 flush");
    assert.equal(client.flushRequests[0].through_event_seq, 2, "阈值 flush 覆盖到第 2 条");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("队列满拒收：不推进 lastBodySeq/latestUser，不 flush 到未入队事件", async () => {
  const dir = mkdtempSync(join(tmpdir(), "am-flush-full-"));
  try {
    const spool = new Spool(dir);
    const client = fakeClient();
    const logger = spyLogger();
    const pipeline = new EventPipeline(spool, client, logger, "h", true);
    await pipeline.start(undefined);
    // 单条 9 MiB 正文超过队列字节上限（8 MiB）→ 拒收。
    const huge = "y".repeat(9 * 1024 * 1024);
    pipeline.observeSessionEvent("s1", userEvent(7, huge));
    assert.equal(client.calls.recordEvent, 0, "拒收事件不得发送");
    assert.equal(pipeline.latestUserOf("s1"), undefined, "拒收事件不得推进最新用户消息");
    assert.ok(logger.warns.some((w) => w.includes("CAPTURE_GAP")), "拒收应记 CAPTURE_GAP");
    // turn/end：无任何已接受正文 → 不排 flush。
    pipeline.observeSessionEvent("s1", { type: "turn/end" });
    await pipeline.dispose();
    assert.equal(client.calls.flush, 0, "不得 flush 到未入队事件");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("前序事件 receipt ack 前不发送 flush", async () => {
  const dir = mkdtempSync(join(tmpdir(), "am-flush-order-"));
  try {
    const spool = new Spool(dir);
    let releaseEvent;
    const gate = new Promise((resolve) => { releaseEvent = resolve; });
    const client = fakeClient({
      recordEvent: () => gate.then(() => ok({ status: 201, body: { evidence_id: "ev" } })),
    });
    const pipeline = new EventPipeline(spool, client, quiet, "h", true);
    await pipeline.start(undefined);
    pipeline.observeSessionEvent("s1", userEvent(3, "你好"));
    pipeline.observeSessionEvent("s1", { type: "turn/end" });
    await new Promise((r) => setTimeout(r, 80));
    assert.equal(client.calls.recordEvent, 1, "事件已进入发送");
    assert.equal(client.calls.flush, 0, "事件 ack 前 flush 不得发送");
    releaseEvent();
    await pipeline.dispose();
    assert.equal(client.calls.flush, 1, "事件 ack 后 flush 发出");
    assert.equal(client.flushRequests[0].through_event_seq, 3);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("重启重放不丢 flush：未 ack 的 flush 在新实例 start 后发出", async () => {
  const dir = mkdtempSync(join(tmpdir(), "am-flush-replay-"));
  try {
    // 第一实例：内核不可达（timeout），事件与 flush 落 spool 但未 ack。
    const offline = fakeClient({ recordEvent: () => ({ failure: "timeout" }), flush: () => ({ failure: "timeout" }) });
    const spoolA = new Spool(dir);
    const pipelineA = new EventPipeline(spoolA, offline, quiet, "h", true);
    await pipelineA.start(undefined);
    pipelineA.observeSessionEvent("s1", userEvent(5, "你好"));
    pipelineA.observeSessionEvent("s1", { type: "turn/end" });
    // 等写入者把两个 op 都落盘。
    const deadline = Date.now() + 3000;
    while (Date.now() < deadline && spoolA.pending().length < 2) {
      await new Promise((r) => setTimeout(r, 20));
    }
    assert.equal(spoolA.pending().length, 2, "事件与 flush 都应已入 spool");
    await pipelineA.dispose();

    // 第二实例：同一 spool，内核正常 → 重放补发。
    const client = fakeClient();
    const spoolB = new Spool(dir);
    const pipelineB = new EventPipeline(spoolB, client, quiet, "h", true);
    await pipelineB.start(undefined);
    await pipelineB.dispose();
    assert.ok(client.calls.recordEvent >= 1, "重放应补发事件");
    assert.ok(client.calls.flush >= 1, "重放应补发 flush（不丢）");
    const replayedFlush = client.flushRequests.find((r) => r.through_event_seq === 5);
    assert.ok(replayedFlush, "flush 应指向既有 through=5");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("compaction/end 按 event → flush receipt → 持久 Dream trigger 排序", async () => {
  const dir = mkdtempSync(join(tmpdir(), "am-dream-trigger-order-"));
  try {
    const spool = new Spool(dir);
    const client = fakeClient();
    const pipeline = new EventPipeline(spool, client, quiet, "stable-host", true);
    await pipeline.start(undefined);
    pipeline.observeSessionEvent("session-a", userEvent(18, "我在杭州做 Rust 开发"));
    pipeline.observeSessionEvent("session-a", {
      type: "compaction/end",
      data: { compactionId: "compact-01" },
    });
    await pipeline.dispose();

    assert.deepEqual(client.order, ["event", "flush", "dream"]);
    assert.equal(client.flushRequests[0].through_event_seq, 18);
    assert.equal(client.calls.dreamTrigger, 1);
    assert.equal(client.dreamRequests[0].trigger_kind, "compact");
    assert.match(client.dreamRequests[0].trigger_key, /^dsh-compact-[a-f0-9]{64}$/);
    assert.equal("compaction_id" in client.dreamRequests[0], false, "只发送 Rust trigger DTO 中定义的字段");
    const pending = spool.pending();
    assert.deepEqual(pending, [], "三个操作均已收到 receipt");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("DSH 离线时 compaction trigger 与前序 flush 一起落盘并按序恢复", async () => {
  const dir = mkdtempSync(join(tmpdir(), "am-dream-trigger-replay-"));
  try {
    const offline = fakeClient({
      recordEvent: () => ({ failure: "offline", status: 0 }),
      flush: () => ({ failure: "offline", status: 0 }),
      dreamTrigger: () => ({ failure: "offline", status: 0 }),
    });
    const spoolA = new Spool(dir);
    const pipelineA = new EventPipeline(spoolA, offline, quiet, "stable-host", true);
    await pipelineA.start(undefined);
    pipelineA.observeSessionEvent("session-b", userEvent(7, "我平时使用 PostgreSQL"));
    pipelineA.observeSessionEvent("session-b", {
      type: "compaction/end",
      data: { compactionId: "compact-offline" },
    });
    const deadline = Date.now() + 3000;
    while (Date.now() < deadline && spoolA.pending().length < 3) {
      await new Promise((r) => setTimeout(r, 20));
    }
    assert.deepEqual(spoolA.pending().map((op) => op.op), ["event", "flush", "dream"]);
    await pipelineA.dispose();

    const client = fakeClient();
    const spoolB = new Spool(dir);
    const pipelineB = new EventPipeline(spoolB, client, quiet, "stable-host", true);
    await pipelineB.start(undefined);
    await pipelineB.dispose();
    assert.deepEqual(client.order, ["event", "flush", "dream"]);
    assert.equal(client.flushRequests[0].through_event_seq, 7);
    assert.equal(client.calls.dreamTrigger, 1);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("compaction/end 遇到满内存队列时先持久化旧队列并同步落盘 trigger", () => {
  const dir = mkdtempSync(join(tmpdir(), "am-dream-trigger-full-"));
  try {
    const spool = new Spool(dir);
    const pipeline = new EventPipeline(spool, fakeClient(), quiet, "stable-host", true);
    // 模拟回调执行前已接收但尚未由异步 writer 落盘的满队列。
    pipeline.queue = Array.from({ length: 1024 }, (_, index) => ({
      op: {
        opId: `stable-host/session-full/${index}`,
        op: "event",
        request: { event_seq: index },
      },
      sessionId: "session-full",
      bodySeq: index,
      bytes: 1,
    }));
    pipeline.queueBytes = 1024;
    pipeline.lastBodySeq.set("session-full", 1023);

    pipeline.observeSessionEvent("session-full", {
      type: "compaction/end",
      data: { compactionId: "compact-queue-full" },
    });

    const pending = spool.pending();
    assert.equal(pending.length, 1026, "1024 个已接收事件、flush、Dream trigger 全部 fsync 落盘");
    assert.deepEqual(pending.slice(-2).map((op) => op.op), ["flush", "dream"]);
    assert.equal(pipeline.queue.length, 0, "compact 回调返回前控制操作已从内存队列写入 spool");
    spool.dispose();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
