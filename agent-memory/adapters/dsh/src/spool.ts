/**
 * 本地持久 spool v2（doc2/03 §2）。
 *
 * - events.jsonl：不可变 envelope（opId, op, request），追加 + fsync 后才算落盘。
 * - receipts.jsonl：v2 receipt {opId, kind, evidence_id?, at}；evidence_id 只来自内核 200/201。
 * - acked.jsonl（v1 旧格式，纯 opId 行）：兼容读取，视为"已确认但无 evidence_id"，
 *   需要 evidence_id 时必须重发原请求取内核幂等响应，绝不伪造 ID。
 * - cursors.json：每 session 已落盘最大 seq（临时文件 + 原子替换）。
 * - 上限 = events + receipts 总字节（默认 100 MiB）；超限抛 SpoolLimitError，
 *   由调用方停止捕获并记缺口，不静默删除唯一副本。
 * - spool 是敏感本地数据（事件正文），目录只给当前 OS 用户；不含令牌。
 */
import {
  appendFileSync,
  closeSync,
  existsSync,
  fsyncSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";

export type SpoolOpKind = "event" | "flush" | "dream";

export interface SpooledOp {
  /** 操作唯一键 = <host>/<session>/<seq 或 flush:through>；内核幂等键来源不变。 */
  opId: string;
  op: SpoolOpKind;
  /** POST 请求正文（不含令牌）。 */
  request: Record<string, unknown>;
}

export interface Receipt {
  opId: string;
  kind: SpoolOpKind;
  /** 仅来自内核响应；旧 acked.jsonl 行没有。 */
  evidenceId?: string;
  at: string;
}

export class SpoolLimitError extends Error {}

export class Spool {
  private readonly eventsPath: string;
  private readonly receiptsPath: string;
  private readonly legacyAckedPath: string;
  private readonly cursorsPath: string;
  private readonly acked = new Map<string, Receipt>();
  private bytes = 0;
  private fd: number | undefined;
  /** 每 session 已落盘（spool 已写）的最大正文证据 seq。 */
  private cursors = new Map<string, number>();

  constructor(
    dir: string,
    private readonly limitBytes = 100 * 1024 * 1024,
  ) {
    mkdirSync(dir, { recursive: true });
    this.eventsPath = join(dir, "events.jsonl");
    this.receiptsPath = join(dir, "receipts.jsonl");
    this.legacyAckedPath = join(dir, "acked.jsonl");
    this.cursorsPath = join(dir, "cursors.json");
    if (existsSync(this.eventsPath)) this.bytes += statSync(this.eventsPath).size;
    if (existsSync(this.receiptsPath)) this.bytes += statSync(this.receiptsPath).size;
    this.loadReceipts();
    this.loadLegacyAcked();
    this.loadCursors();
    this.fd = openSync(this.eventsPath, "a");
  }

  private loadReceipts(): void {
    if (!existsSync(this.receiptsPath)) return;
    for (const line of readFileSync(this.receiptsPath, "utf8").split("\n")) {
      const t = line.trim();
      if (!t) continue;
      try {
        const r = JSON.parse(t) as Receipt;
        this.acked.set(r.opId, r);
      } catch {
        // 跳过损坏行：不据此声称已确认。
      }
    }
  }

  /** v1 旧 acked.jsonl：仅"已确认过"语义，无 evidence_id（doc2/03 §2）。 */
  private loadLegacyAcked(): void {
    if (!existsSync(this.legacyAckedPath)) return;
    for (const line of readFileSync(this.legacyAckedPath, "utf8").split("\n")) {
      const opId = line.trim();
      if (!opId || this.acked.has(opId)) continue;
      this.acked.set(opId, { opId, kind: "event", at: "legacy" });
    }
  }

  private loadCursors(): void {
    if (!existsSync(this.cursorsPath)) return;
    try {
      const raw = JSON.parse(readFileSync(this.cursorsPath, "utf8")) as Record<string, number>;
      for (const [sessionId, seq] of Object.entries(raw)) {
        if (Number.isSafeInteger(seq) && seq >= 0) this.cursors.set(sessionId, seq);
      }
    } catch {
      // 光标损坏：按无光标处理，启动对账会重新建立（可能重复入队同 opId，靠幂等去重）。
    }
  }

  /** 追加并刷新（fsync）。落盘成功后才更新 session 光标。失败抛错，由调用方停止捕获。 */
  append(op: SpooledOp, sessionId: string, bodySeq: number | undefined): void {
    const line = `${JSON.stringify(op)}\n`;
    const size = Buffer.byteLength(line);
    if (this.bytes + size > this.limitBytes) {
      throw new SpoolLimitError(
        `spool 超过 ${Math.floor(this.limitBytes / 1024 / 1024)} MiB 上限，停止记忆捕获（DSH 对话不受影响）`,
      );
    }
    appendFileSync(this.fd as number, line);
    fsyncSync(this.fd as number);
    this.bytes += size;
    if (bodySeq !== undefined) {
      const prev = this.cursors.get(sessionId) ?? -1;
      if (bodySeq > prev) {
        this.cursors.set(sessionId, bodySeq);
        this.persistCursors();
      }
    }
  }

  /** 内核 200/201/202 确认后持久化 receipt（含 evidence_id 时回填映射）。 */
  markAcked(opId: string, kind: SpoolOpKind, evidenceId: string | undefined): void {
    if (this.acked.has(opId)) return;
    const receipt: Receipt = { opId, kind, at: new Date().toISOString() };
    if (evidenceId) receipt.evidenceId = evidenceId;
    this.acked.set(opId, receipt);
    const line = `${JSON.stringify(receipt)}\n`;
    appendFileSync(this.receiptsPath, line);
    fsyncSync(openSync(this.receiptsPath, "r+"));
    this.bytes += Buffer.byteLength(line);
  }

  isAcked(opId: string): boolean {
    return this.acked.has(opId);
  }

  receiptOf(opId: string): Receipt | undefined {
    return this.acked.get(opId);
  }

  /** 启动重放：按原序返回未 ack 的操作。 */
  pending(): SpooledOp[] {
    if (!existsSync(this.eventsPath)) return [];
    const out: SpooledOp[] = [];
    for (const line of readFileSync(this.eventsPath, "utf8").split("\n")) {
      const t = line.trim();
      if (!t) continue;
      try {
        const op = JSON.parse(t) as SpooledOp;
        if (!this.acked.has(op.opId)) out.push(op);
      } catch {
        // 损坏行不重放（记缺口由调用方记录 CAPTURE_GAP）。
      }
    }
    return out;
  }

  cursorOf(sessionId: string): number {
    return this.cursors.get(sessionId) ?? -1;
  }

  knownSessions(): string[] {
    return [...this.cursors.keys()];
  }

  /** 原子写光标（临时文件 + 替换）。 */
  private persistCursors(): void {
    const tmp = join(dirname(this.cursorsPath), `.cursors-${process.pid}.tmp`);
    writeFileSync(tmp, JSON.stringify(Object.fromEntries(this.cursors)));
    renameSync(tmp, this.cursorsPath);
  }

  dispose(): void {
    if (this.fd !== undefined) {
      fsyncSync(this.fd);
      closeSync(this.fd);
      this.fd = undefined;
    }
  }
}
