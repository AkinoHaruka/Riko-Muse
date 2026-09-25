/**
 * 事件线 v2（doc2/03，官方形状已核对 deepseek-harness@477b4f4）。
 *
 * 已核对的宿主事实：
 * - `session/event`(session, event)：event.seq 会话内连续非负整数；event.time 为 Unix 毫秒数；
 *   user/message 的 event.data 直接是 UserMessage；assistant/message 与 tool/result 正文在
 *   event.data.message；UserMessage.source 必填，只有 source.kind==='user' 是直接用户输入。
 * - 回调是提交后的同步受保护通知；热路径不得阻塞 I/O → 回调只进有界内存队列，异步落盘。
 *
 * 顺序模型（doc2/03 §2）：
 * 1. 回调同步验证形状/来源，定 opId，入有界队列（1024 条 / 8 MiB，满即拒收并记 CAPTURE_GAP）后立即返回。
 * 2. 单一异步写入者按入队顺序写 spool 并 fsync；确认落盘后才进发送链。
 * 3. 同一 session 按 seq 发送；turn/end 先入队持久化，等该 session 前序事件全部 ack 才发 flush。
 * 4. 内核 200/201 返回 evidence_id 后先持久化 receipt 再算 ack；receipt 写失败保持未 ack 下次重发。
 * 5. 失败分类：可恢复(离线/超时/429/5xx)退避重试；401 暂停该令牌全部出站并报配置问题；
 *    400/409 永久标记该 session 停发，等人工修复。
 */
import type { MemoryClient } from "./client.js";
import { Spool, SpoolLimitError, type SpooledOp } from "./spool.js";

export interface Logger {
  warn(message: string): void;
  error(message: string): void;
  info?(message: string): void;
}

/** 内核 ingest 请求（doc/12 §3）。 */
export interface EventWireRequest extends Record<string, unknown> {
  origin: { host_id: string; agent_id: string; session_id: string };
  event_seq: number;
  role: string;
  source_kind: string;
  occurred_at: string;
  content: string;
}

export interface LatestUserMessage {
  seq: number;
  messageId: string;
  content: string;
}

/** 定点缺口对账（由 index.ts 用官方 ctx.sessionQuery.readEvent 实现）。 */
export interface GapChecker {
  /** 返回该 session 指定 seq 的原始事件；不存在/不可读返回 undefined。 */
  readEvent(sessionId: string, seq: number): Promise<unknown>;
}

const QUEUE_MAX_ENTRIES = 1024;
const QUEUE_MAX_BYTES = 8 * 1024 * 1024;
const RETRY_BASE_MS = 500;
const RETRY_CAP_MS = 15_000;
const GAP_CHECK_MAX_READS = 500;
const GAP_CHECK_MAX_MS = 30_000;
const PROTOCOL_CACHE_MS = 60_000;

interface QueueItem {
  op: SpooledOp;
  sessionId: string;
  /** 正文证据 seq（flush 无）；用于 cursor 推进与 flush 上界。 */
  bodySeq: number | undefined;
  bytes: number;
}

interface EvidenceWaiter {
  resolve: (evidenceId: string | undefined) => void;
  timer: NodeJS.Timeout;
}

export class EventPipeline {
  private readonly queue: QueueItem[] = [];
  private queueBytes = 0;
  private stopped = false;
  private captureBroken = false;
  private writerWakeup: (() => void) | undefined;
  /** 每 session 发送链。 */
  private readonly chains = new Map<string, Promise<void>>();
  private readonly chainPending = new Map<string, QueueItem[]>();
  /** 每 session 已排队（含已发）的最后正文证据 seq。 */
  private readonly lastBodySeq = new Map<string, number>();
  /** 每 session 最新 user/user 消息（按 seq 单调更新）。 */
  private readonly latestUser = new Map<string, LatestUserMessage>();
  private readonly evidenceIds = new Map<string, string>(); // opId -> evidence_id
  private readonly waiters = new Map<string, EvidenceWaiter[]>();
  private unauthorizedPaused = false;
  private protocolOkAt = 0;
  private readonly permanentlyBroken = new Set<string>();

  constructor(
    private readonly spool: Spool,
    private readonly client: MemoryClient,
    private readonly logger: Logger,
    private readonly hostId: string,
    private readonly captureEnabled: boolean,
  ) {}

  // ---------------------------------------------------------------- 回调入口

  /** session/event 回调（同步）：验证 → opId → 有界入队 → 立即返回。 */
  observeSessionEvent(sessionId: string, event: unknown): void {
    if (this.stopped || this.captureBroken || !this.captureEnabled) return;
    const ev = event as { type?: string; seq?: unknown; time?: unknown; data?: unknown };
    const type = ev.type;
    if (type === "turn/end") {
      this.enqueueFlush(sessionId);
      return;
    }
    const mapped = this.mapEvent(sessionId, ev);
    if (mapped === undefined) return;
    const opId = `${this.hostId}/${sessionId}/${mapped.event_seq}`;
    if (this.spool.isAcked(opId) || this.isPending(opId)) return; // 对账/重放去重
    const item: QueueItem = {
      op: { opId, op: "event", request: mapped },
      sessionId,
      bodySeq: mapped.event_seq,
      bytes: Buffer.byteLength(JSON.stringify(mapped)),
    };
    this.push(item);
    this.lastBodySeq.set(sessionId, mapped.event_seq);
    if (mapped.role === "user" && mapped.source_kind === "user") {
      const data = ev.data as { id?: unknown };
      this.latestUser.set(sessionId, {
        seq: mapped.event_seq,
        messageId: typeof data.id === "string" ? data.id : "",
        content: mapped.content,
      });
    }
  }

  // ---------------------------------------------------------------- 工具接口

  latestUserOf(sessionId: string): LatestUserMessage | undefined {
    return this.latestUser.get(sessionId);
  }

  evidenceIdFor(sessionId: string, seq: number): string | undefined {
    return this.evidenceIds.get(`${this.hostId}/${sessionId}/${seq}`);
  }

  /** 等待指定用户事件的内核 receipt（受 timeoutMs 与外部 signal 限制）。 */
  awaitEvidenceId(sessionId: string, seq: number, timeoutMs: number, signal?: AbortSignal): Promise<string | undefined> {
    const existing = this.evidenceIdFor(sessionId, seq);
    if (existing) return Promise.resolve(existing);
    if (signal?.aborted) return Promise.resolve(undefined);
    const opId = `${this.hostId}/${sessionId}/${seq}`;
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.removeWaiter(opId, waiter);
        resolve(undefined);
      }, timeoutMs);
      const waiter: EvidenceWaiter = { resolve, timer };
      if (signal) {
        const onAbort = () => {
          clearTimeout(timer);
          this.removeWaiter(opId, waiter);
          resolve(undefined);
        };
        signal.addEventListener("abort", onAbort, { once: true });
      }
      const list = this.waiters.get(opId) ?? [];
      list.push(waiter);
      this.waiters.set(opId, list);
    });
  }

  // ---------------------------------------------------------------- 生命周期

  /** 启动：重建发送队列（重启重放）→ 已知 session 定点对账。 */
  async start(gapChecker?: GapChecker): Promise<void> {
    void this.runWriter();
    for (const op of this.spool.pending()) {
      this.enqueueExisting(op);
    }
    if (gapChecker) {
      await this.reconcileGaps(gapChecker);
    } else {
      this.logger.warn("CAPTURE_GAP: 未装载 sessionQuery，无法对已知 session 定点对账");
    }
    this.logger.info?.(
      "agent-memory: 未覆盖窗口声明——首次捕获前即崩溃的 session 无法枚举，不能自动恢复",
    );
  }

  /** 卸载：停止接收，真实等待在途发送链最多 5 秒；未 ack 保留 spool。 */
  async dispose(): Promise<void> {
    this.stopped = true;
    const wakeup = this.writerWakeup;
    if (wakeup) {
      this.writerWakeup = undefined;
      wakeup();
    }
    const chains = [...this.chains.values()];
    await Promise.race([
      Promise.all(chains.map((c) => c.catch(() => undefined))),
      new Promise((r) => setTimeout(r, 5000)),
    ]);
    for (const list of this.waiters.values()) {
      for (const w of list) {
        clearTimeout(w.timer);
        w.resolve(undefined);
      }
    }
    this.waiters.clear();
  }

  // ---------------------------------------------------------------- 内部：队列与写入者

  private push(item: QueueItem): void {
    if (this.queue.length >= QUEUE_MAX_ENTRIES || this.queueBytes + item.bytes > QUEUE_MAX_BYTES) {
      const seq = item.bodySeq ?? "flush";
      this.logger.warn(`CAPTURE_GAP: 内存队列已满（${QUEUE_MAX_ENTRIES} 条/8 MiB），拒收 ${item.sessionId}/${seq}`);
      return;
    }
    this.queue.push(item);
    this.queueBytes += item.bytes;
    const wakeup = this.writerWakeup;
    if (wakeup) {
      this.writerWakeup = undefined;
      wakeup();
    }
  }

  private isPending(opId: string): boolean {
    if (this.queue.some((i) => i.op.opId === opId)) return true;
    for (const list of this.chainPending.values()) {
      if (list.some((i) => i.op.opId === opId)) return true;
    }
    return false;
  }

  private async runWriter(): Promise<void> {
    for (;;) {
      if (this.stopped && this.queue.length === 0) return;
      const item = this.queue.shift();
      if (item === undefined) {
        await new Promise<void>((r) => {
          this.writerWakeup = r;
          setTimeout(() => {
            this.writerWakeup = undefined;
            r();
          }, 250); // 周期性醒来检查 stopped
        });
        continue;
      }
      this.queueBytes -= item.bytes;
      try {
        this.spool.append(item.op, item.sessionId, item.bodySeq);
      } catch (e) {
        if (e instanceof SpoolLimitError) {
          this.captureBroken = true;
          this.logger.error?.(`CAPTURE_GAP: ${e.message}；已停止记忆捕获并启动对账`);
          this.rejectAllWaiters();
          return;
        }
        this.logger.error?.(`CAPTURE_GAP: spool 写入失败 op=${item.op.opId}: ${String(e)}`);
        continue; // 该 op 丢失风险已记录，继续后续（不阻塞 DSH）
      }
      this.enqueueToChain(item);
    }
  }

  private enqueueToChain(item: QueueItem): void {
    const sessionId = item.sessionId;
    const list = this.chainPending.get(sessionId) ?? [];
    list.push(item);
    this.chainPending.set(sessionId, list);
    void list;
    const prev = this.chains.get(sessionId) ?? Promise.resolve();
    const next = prev.then(() => this.drainChain(sessionId), () => this.drainChain(sessionId));
    this.chains.set(sessionId, next.then(() => undefined, () => undefined));
  }

  private enqueueExisting(op: SpooledOp): void {
    // 重启重放：op 已落盘，直接进发送链。
    // session_id 位置按 op 形状区分（doc/12 §3/§4）：event 请求在 origin 内，flush 请求在顶层。
    // 此前只读顶层 request.session_id，event op 全部被静默丢弃，重放从未发送过事件。
    const origin = op.request.origin as { session_id?: unknown } | undefined;
    const sessionId = String(origin?.session_id ?? op.request.session_id ?? "");
    if (sessionId.length === 0) return;
    const bodySeq = op.op === "event" ? Number(op.request.event_seq) : undefined;
    this.enqueueToChain({ op, sessionId, bodySeq, bytes: 0 });
  }

  private enqueueFlush(sessionId: string): void {
    const through = this.lastBodySeq.get(sessionId);
    if (through === undefined) return; // 无正文证据，无需 flush
    const opId = `${this.hostId}/${sessionId}/flush:${through}`;
    if (this.spool.isAcked(opId) || this.isPending(opId)) return;
    this.push({
      op: { opId, op: "flush", request: { host_id: this.hostId, session_id: sessionId, through_event_seq: through } },
      sessionId,
      bodySeq: undefined,
      bytes: 64,
    });
  }

  // ---------------------------------------------------------------- 内部：发送链

  private async drainChain(sessionId: string): Promise<void> {
    for (;;) {
      if (this.stopped) return;
      if (this.unauthorizedPaused) return;
      if (this.permanentlyBroken.has(sessionId)) return;
      const list = this.chainPending.get(sessionId);
      if (list === undefined || list.length === 0) return;
      const item = list[0] as QueueItem;
      if (!(await this.ensureProtocol())) return;
      const result =
        item.op.op === "event"
          ? await this.client.recordEvent(item.op.request)
          : await this.client.flush(item.op.request);
      if (result.failure === "ok" && (result.status === 200 || result.status === 201 || result.status === 202)) {
        const body = (result.body ?? {}) as { evidence_id?: string };
        const evidenceId = typeof body.evidence_id === "string" ? body.evidence_id : undefined;
        try {
          this.spool.markAcked(item.op.opId, item.op.op, evidenceId);
        } catch {
          // receipt 写失败：保持未 ack，下次重发（内核幂等），绝不删原请求。
          this.retryLater(sessionId);
          return;
        }
        if (evidenceId) {
          this.evidenceIds.set(item.op.opId, evidenceId);
          this.resolveWaiters(item.op.opId, evidenceId);
        }
        list.shift();
        this.chainPending.set(sessionId, list);
        continue;
      }
      if (result.failure === "unauthorized") {
        this.unauthorizedPaused = true;
        this.logger.error(
          `agent-memory: 内核返回 401，暂停该令牌全部出站发送；请检查 userTokenFile 配置（spool 保留，换令牌后可继续）`,
        );
        return;
      }
      if (result.failure === "permanent") {
        this.permanentlyBroken.add(sessionId);
        const err = (result.body ?? {}) as { error?: { code?: string } };
        this.logger.error(
          `agent-memory: 内核永久拒绝 op=${item.op.opId} status=${result.status} code=${err.error?.code ?? "?"} request_id=${result.requestId ?? "?"}；该 session 停止发送，等待修复映射/冲突`,
        );
        return;
      }
      this.retryLater(sessionId);
      return;
    }
  }

  private retryDelayMs = RETRY_BASE_MS;

  private retryLater(sessionId: string): void {
    const delay = this.retryDelayMs;
    this.retryDelayMs = Math.min(this.retryDelayMs * 2, RETRY_CAP_MS);
    setTimeout(() => {
      if (this.stopped) return;
      const prev = this.chains.get(sessionId) ?? Promise.resolve();
      const next = prev.then(() => this.drainChain(sessionId), () => this.drainChain(sessionId));
      this.chains.set(sessionId, next.then(() => undefined, () => undefined));
    }, delay);
  }

  /** 连通后先核 /v1/version 的 protocol_version=1，再发送；不兼容即暂停出站。 */
  private async ensureProtocol(): Promise<boolean> {
    const now = Date.now();
    if (now - this.protocolOkAt < PROTOCOL_CACHE_MS) return true;
    const r = await this.client.version();
    if (r.failure === "ok" && r.status === 200) {
      const body = (r.body ?? {}) as { protocol_version?: number };
      if (body.protocol_version === 1) {
        this.protocolOkAt = now;
        this.retryDelayMs = RETRY_BASE_MS;
        return true;
      }
      this.logger.error(
        `agent-memory: 内核协议不兼容 protocol_version=${body.protocol_version ?? "?"}（适配器=1），暂停出站请求`,
      );
      return false;
    }
    return false; // 离线：本轮不发送，退避重试会自动再来
  }

  // ---------------------------------------------------------------- 内部：缺口对账

  private async reconcileGaps(checker: GapChecker): Promise<void> {
    const deadline = Date.now() + GAP_CHECK_MAX_MS;
    for (const sessionId of this.spool.knownSessions()) {
      let seq = this.spool.cursorOf(sessionId) + 1;
      for (let i = 0; i < GAP_CHECK_MAX_READS && Date.now() < deadline; i++, seq++) {
        let raw: unknown;
        try {
          raw = await checker.readEvent(sessionId, seq);
        } catch {
          this.logger.warn(`CAPTURE_GAP: session ${sessionId} seq=${seq} 读取失败，停止该 session 对账，需人工处理`);
          break;
        }
        if (raw === undefined) break; // 已到当前日志末尾
        this.observeSessionEvent(sessionId, raw); // 同 source/形状规则，原 opId 幂等
      }
    }
  }

  // ---------------------------------------------------------------- 内部：事件映射（官方形状）

  private mapEvent(sessionId: string, ev: { type?: string; seq?: unknown; time?: unknown; data?: unknown }): EventWireRequest | undefined {
    const seq = ev.seq;
    if (typeof seq !== "number" || !Number.isSafeInteger(seq) || seq < 0) {
      this.logger.warn(`DSH_EVENT_SHAPE_UNSUPPORTED: event.seq 缺失或非法 type=${ev.type}，不发送`);
      return undefined;
    }
    const time = ev.time;
    if (typeof time !== "number" || !Number.isSafeInteger(time) || time < 0) {
      this.logger.warn(`DSH_EVENT_SHAPE_UNSUPPORTED: event.time 不是数字毫秒 type=${ev.type} seq=${seq}，不发送`);
      return undefined;
    }
    const occurred = new Date(time);
    if (Number.isNaN(occurred.getTime())) {
      this.logger.warn(`DSH_EVENT_SHAPE_UNSUPPORTED: event.time 转换失败 type=${ev.type} seq=${seq}，不发送`);
      return undefined;
    }
    const data = (ev.data ?? {}) as { source?: { kind?: unknown }; message?: { content?: unknown }; content?: unknown };
    switch (ev.type) {
      case "user/message": {
        // 来源必须精确匹配 'user'；缺失/其他来源绝不默认提升（doc2/03 §1）。
        if (data.source?.kind !== "user") {
          this.logger.warn(`UNSUPPORTED_SOURCE: user/message seq=${seq} source.kind=${JSON.stringify(data.source?.kind)}，未作为用户证据接收`);
          return undefined;
        }
        const text = extractText(data.content);
        if (text.length === 0) return undefined; // 纯非文本不臆造正文
        return {
          origin: { host_id: this.hostId, agent_id: sessionId, session_id: sessionId },
          event_seq: seq,
          role: "user",
          source_kind: "user",
          occurred_at: occurred.toISOString(),
          content: text,
        };
      }
      case "assistant/message":
      case "tool/result": {
        const text = extractText(data.message?.content);
        if (text.length === 0) return undefined;
        const role = ev.type === "assistant/message" ? "assistant" : "tool";
        return {
          origin: { host_id: this.hostId, agent_id: sessionId, session_id: sessionId },
          event_seq: seq,
          role,
          source_kind: role,
          occurred_at: occurred.toISOString(),
          content: text,
        };
      }
      default:
        return undefined;
    }
  }

  // ---------------------------------------------------------------- 内部：waiter

  private removeWaiter(opId: string, waiter: EvidenceWaiter): void {
    const list = this.waiters.get(opId);
    if (!list) return;
    const idx = list.indexOf(waiter);
    if (idx >= 0) list.splice(idx, 1);
    if (list.length === 0) this.waiters.delete(opId);
  }

  private resolveWaiters(opId: string, evidenceId: string): void {
    const list = this.waiters.get(opId);
    if (!list) return;
    for (const w of list) {
      clearTimeout(w.timer);
      w.resolve(evidenceId);
    }
    this.waiters.delete(opId);
  }

  private rejectAllWaiters(): void {
    for (const [opId, list] of this.waiters) {
      for (const w of list) {
        clearTimeout(w.timer);
        w.resolve(undefined);
      }
      this.waiters.delete(opId);
    }
  }
}

/** 正文只取 content[] 中 type='text' 的 text，按原顺序换行连接（doc2/03 §1）。 */
function extractText(content: unknown): string {
  if (content === undefined || content === null) return "";
  if (typeof content === "string") return content.trim();
  if (Array.isArray(content)) {
    return content
      .filter((b): b is { type?: unknown; text?: unknown } => typeof b === "object" && b !== null)
      .filter((b) => b.type === "text" && typeof b.text === "string")
      .map((b) => b.text as string)
      .join("\n")
      .trim();
  }
  return "";
}
