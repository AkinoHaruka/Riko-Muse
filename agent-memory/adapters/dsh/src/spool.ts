/**
 * 本地待发 spool（doc/14 §3）。
 *
 * - 发送前把 event/flush envelope 追加到 spoolDir/events.jsonl 并落盘；session/event 回调在
 *   本机写入成功后才返回，网络发送异步进行。
 * - 收到 200/201（event）或 200/202（flush）后把 op ID 写入 acked.jsonl。
 * - 重启后按原顺序重放未 ack 操作，依赖内核幂等键去重。
 * - 总量限 100 MiB，超限抛错并停止记忆捕获（不影响 DSH 正常对话），不静默丢弃。
 * - spool 是敏感本地数据：目录权限限当前用户，不含令牌。
 */
import {
  appendFileSync,
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  statSync,
} from "node:fs";
import { join } from "node:path";

export type SpoolOpKind = "event" | "flush";

export interface SpooledOp {
  /** 操作唯一键；同一内核幂等键的来源不变。 */
  opId: string;
  op: SpoolOpKind;
  /** POST 请求正文（不含令牌）。 */
  request: Record<string, unknown>;
}

export class SpoolLimitError extends Error {}

export class Spool {
  private readonly eventsPath: string;
  private readonly ackedPath: string;
  private readonly acked = new Set<string>();
  private bytes = 0;
  private fd: number | undefined;

  constructor(
    dir: string,
    private readonly limitBytes = 100 * 1024 * 1024,
  ) {
    mkdirSync(dir, { recursive: true });
    this.eventsPath = join(dir, "events.jsonl");
    this.ackedPath = join(dir, "acked.jsonl");
    if (existsSync(this.eventsPath)) this.bytes = statSync(this.eventsPath).size;
    if (existsSync(this.ackedPath)) {
      for (const line of readFileSync(this.ackedPath, "utf8").split("\n")) {
        const t = line.trim();
        if (t) this.acked.add(t);
      }
    }
    this.fd = openSync(this.eventsPath, "a");
  }

  /** 追加并落盘。回调内同步调用；失败抛错，由调用方决定停止捕获。 */
  append(op: SpooledOp): void {
    const line = `${JSON.stringify(op)}\n`;
    if (this.bytes + Buffer.byteLength(line) > this.limitBytes) {
      throw new SpoolLimitError(
        `spool 超过 ${Math.floor(this.limitBytes / 1024 / 1024)} MiB 上限，停止记忆捕获（DSH 对话不受影响）`,
      );
    }
    appendFileSync(this.fd as number, line);
    this.bytes += Buffer.byteLength(line);
  }

  markAcked(opId: string): void {
    if (this.acked.has(opId)) return;
    this.acked.add(opId);
    appendFileSync(this.ackedPath, `${opId}\n`);
  }

  /** 启动重放：按原序返回未 ack 的操作。 */
  pending(): SpooledOp[] {
    if (!existsSync(this.eventsPath)) return [];
    const out: SpooledOp[] = [];
    for (const line of readFileSync(this.eventsPath, "utf8").split("\n")) {
      const t = line.trim();
      if (!t) continue;
      const op = JSON.parse(t) as SpooledOp;
      if (!this.acked.has(op.opId)) out.push(op);
    }
    return out;
  }

  /** 只压缩所有未 ack 操作都可恢复的情形（本实现不在发送中途删除唯一副本）。 */
  dispose(): void {
    if (this.fd !== undefined) {
      closeSync(this.fd);
      this.fd = undefined;
    }
  }
}
