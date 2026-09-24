/**
 * 事件线（doc/14 §2）。
 *
 * 已核对的宿主事实（C:\TRAE\Riko-dsh\deepseek-harness @ riko-memory/src/index.ts，只读）：
 * - ctx.on('session/event', (session, event) => ...)；event.type ∈ user/message | assistant/message |
 *   tool/result | turn/end；event.seq 为整数、event.time 为时间、session.id 为会话 ID。
 * - user/message 的 plugin 来源：event.data.source.kind === 'plugin' → 不作为用户证据发送。
 * - 正文在 content blocks：{ type:'text', text }[]。
 * - 每会话 Promise 顺序链：参考插件用 sessionWrites Map（本实现同思路）。
 * - turn/end 的 flush 边界 = 此前已追加到 spool 的最后一条正文证据序号（不是 turn/end 自身序号）。
 *
 * 缺失 seq/time/session.id 时记录 DSH_EVENT_SHAPE_UNSUPPORTED，不猜测、不自增序号。
 */
import type { MemoryClient } from "./client.js";
import { Spool, type SpooledOp } from "./spool.js";

export interface HostMessageSource {
  kind?: string;
}

export interface HostContentBlock {
  type?: string;
  text?: string;
}

export interface HostSessionEvent {
  type: string;
  seq?: unknown;
  time?: unknown;
  data?: unknown;
}

export interface Logger {
  warn(message: string): void;
  error?(message: string): void;
}

const PLUGIN_NAME = "@agent-memory/dsh-adapter";

export interface EventWireRequest extends Record<string, unknown> {
  origin: { host_id: string; agent_id: string; session_id: string };
  event_seq: number;
  role: string;
  source_kind: string;
  occurred_at: string;
  content: string;
}

export class EventPipeline {
  /** 每会话一条 Promise 顺序链。 */
  private readonly chains = new Map<string, Promise<void>>();
  /** 每会话已入 spool 的最后正文证据 seq（flush 上界）。 */
  private readonly lastEvidenceSeq = new Map<string, number>();
  private readonly knownEvidence = new Map<string, string>(); // "session/seq" -> evidence_id（工具回填用）
  stopped = false;

  constructor(
    private readonly spool: Spool,
    private readonly client: MemoryClient,
    private readonly logger: Logger,
    private readonly hostId: string,
  ) {}

  /** session/event 回调入口：同步落 spool 后返回；网络发送异步。 */
  observeSessionEvent(
    sessionIdRaw: unknown,
    event: HostSessionEvent,
    agentId: string,
  ): void {
    if (this.stopped) return;
    const sessionId = typeof sessionIdRaw === "string" ? sessionIdRaw : String(sessionIdRaw ?? "");
    if (sessionId.length === 0) {
      this.logger.warn("DSH_EVENT_SHAPE_UNSUPPORTED: session.id 缺失，事件不发送");
      return;
    }
    if (event.type === "user/message") {
      const sourceKind = (event.data as { source?: HostMessageSource } | undefined)?.source?.kind;
      if (sourceKind === "plugin") return; // 插件消息不作为用户证据（doc/05 §1）
    }
    if (event.type === "turn/end") {
      this.enqueue(sessionId, () => this.flushTurnEnd(sessionId, agentId));
      return;
    }
    const mapped = this.mapEvent(sessionId, event, agentId);
    if (mapped === undefined) return;
    const opId = `${sessionId}/${mapped.event_seq}`;
    this.enqueue(sessionId, async () => {
      this.spool.append({ opId, op: "event", request: mapped });
      this.lastEvidenceSeq.set(sessionId, mapped.event_seq);
      const r = await this.client.recordEvent(mapped);
      if (r.status === 200 || r.status === 201) {
        this.spool.markAcked(opId);
        const body = r.body as { evidence_id?: string };
        if (body.evidence_id) this.knownEvidence.set(opId, body.evidence_id);
      } else {
        this.logger.warn(`memoryd ingest 未确认 status=${r.status} op=${opId}；留在 spool 待重放`);
      }
    });
  }

  /** 把已提交的 event.seq → evidence_id 暴露给工具层（doc/14 §5：不能只靠文字相等猜 ID）。 */
  evidenceIdFor(sessionId: string, eventSeq: number): string | undefined {
    return this.knownEvidence.get(`${sessionId}/${eventSeq}`);
  }

  latestSeqFor(sessionId: string): number | undefined {
    return this.lastEvidenceSeq.get(sessionId);
  }

  /** 启动重放：按原序发送未 ack 操作。 */
  async replayPending(): Promise<void> {
    for (const op of this.spool.pending()) {
      await this.sendOp(op);
    }
  }

  dispose(): void {
    this.stopped = true;
    // doc/14 §3：dispose 最多等 5 秒，不删除 spool；未 ack 记录留给下次启动。
    const drain = Promise.all([...this.chains.values()]).catch(() => undefined);
    void Promise.race([drain, new Promise((r) => setTimeout(r, 5000))]);
    this.spool.dispose();
  }

  private enqueue(sessionId: string, task: () => Promise<void>): void {
    const prev = this.chains.get(sessionId) ?? Promise.resolve();
    const next = prev.then(task, task);
    this.chains.set(sessionId, next.then(() => undefined, () => undefined));
  }

  private async flushTurnEnd(sessionId: string, agentId: string): Promise<void> {
    const through = this.lastEvidenceSeq.get(sessionId);
    if (through === undefined) return; // 无正文证据，无需 flush
    const opId = `${sessionId}/flush:${through}`;
    const request = {
      host_id: this.hostId,
      session_id: sessionId,
      through_event_seq: through,
    };
    this.spool.append({ opId, op: "flush", request });
    await this.sendOp({ opId, op: "flush", request });
  }

  private async sendOp(op: SpooledOp): Promise<void> {
    const r =
      op.op === "event"
        ? await this.client.recordEvent(op.request)
        : await this.client.flush(op.request);
    if (r.status === 200 || r.status === 201 || r.status === 202) {
      this.spool.markAcked(op.opId);
    } else {
      this.logger.warn(`memoryd ${op.op} 未确认 status=${r.status} op=${op.opId}；留在 spool 待重放`);
    }
  }

  private mapEvent(
    sessionId: string,
    event: HostSessionEvent,
    agentId: string,
  ): EventWireRequest | undefined {
    const seq = event.seq;
    if (typeof seq !== "number" || !Number.isSafeInteger(seq) || seq < 0) {
      this.logger.warn(`DSH_EVENT_SHAPE_UNSUPPORTED: event.seq 缺失或非法 type=${event.type}，不发送`);
      return undefined;
    }
    const time = event.time;
    const occurred =
      typeof time === "string"
        ? time
        : time instanceof Date
          ? time.toISOString()
          : undefined;
    if (occurred === undefined) {
      this.logger.warn(`DSH_EVENT_SHAPE_UNSUPPORTED: event.time 缺失 type=${event.type}，不发送`);
      return undefined;
    }
    const data = (event.data ?? {}) as { source?: HostMessageSource; message?: unknown; content?: unknown };
    const sourceKind = typeof data.source?.kind === "string" ? data.source.kind : undefined;
    const text = extractText(data.message) || extractText(data.content);
    if (text.length === 0) return undefined; // 纯图片/非文本：v1 不拼想象文字（doc/14 §2）

    let role: string;
    let source_kind: string;
    switch (event.type) {
      case "user/message":
        role = "user";
        source_kind = sourceKind ?? "user";
        break;
      case "assistant/message":
        role = "assistant";
        source_kind = "assistant";
        break;
      case "tool/result":
        role = "tool";
        source_kind = "tool";
        break;
      default:
        return undefined;
    }
    if (role === "user" && source_kind !== "user") {
      // user 角色但 plugin 等来源：保留事件但不从它提取（source_kind=plugin 进 L0 供审计）。
      if (source_kind === "plugin") return undefined; // v1 与参考插件一致：直接不发
    }
    return {
      origin: { host_id: this.hostId, agent_id: agentId, session_id: sessionId },
      event_seq: seq,
      role,
      source_kind,
      occurred_at: occurred,
      content: text,
    };
  }
}

function extractText(value: unknown): string {
  if (value === undefined || value === null) return "";
  if (typeof value === "string") return value;
  if (Array.isArray(value)) {
    return value
      .filter((b): b is HostContentBlock => typeof b === "object" && b !== null)
      .filter((b) => b.type === "text" && typeof b.text === "string")
      .map((b) => b.text as string)
      .join("\n")
      .trim();
  }
  return "";
}
