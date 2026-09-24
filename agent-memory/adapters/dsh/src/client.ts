/**
 * Rust 内核 HTTP 客户端（doc/12 协议 v1）。
 * 令牌只在内存中持有；compose/write 超时来自插件配置；日志不输出令牌与正文。
 * 错误分类（doc2/03 §3）：unauthorized(401) / permanent(400,409) / offline(网络、超时、429、5xx)。
 */

export type FailureClass = "ok" | "unauthorized" | "permanent" | "offline";

export interface ClientConfig {
  baseUrl: string;
  token: string;
  writeTimeoutMs: number;
  composeTimeoutMs: number;
}

export interface ApiResult {
  status: number;
  body: unknown;
  requestId?: string;
  failure: FailureClass;
}

function classify(status: number): FailureClass {
  if (status === 401) return "unauthorized";
  if (status === 400 || status === 409) return "permanent";
  if (status >= 500 || status === 429 || status === 408) return "offline";
  return "ok";
}

export class MemoryClient {
  constructor(private readonly cfg: ClientConfig) {}

  private async post(path: string, body: unknown, timeoutMs: number, signal?: AbortSignal): Promise<ApiResult> {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), timeoutMs);
    try {
      const res = await fetch(`${this.cfg.baseUrl}${path}`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: `Bearer ${this.cfg.token}`,
        },
        body: JSON.stringify(body),
        signal: signal ? AbortSignal.any([signal, ctrl.signal]) : ctrl.signal,
      });
      const json = (await res.json().catch(() => ({}))) as Record<string, unknown>;
      return { status: res.status, body: json, requestId: json.request_id as string | undefined, failure: classify(res.status) };
    } catch {
      return { status: 0, body: undefined, failure: "offline" };
    } finally {
      clearTimeout(timer);
    }
  }

  private async get(path: string, timeoutMs: number): Promise<ApiResult> {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), timeoutMs);
    try {
      const res = await fetch(`${this.cfg.baseUrl}${path}`, {
        headers: { authorization: `Bearer ${this.cfg.token}` },
        signal: ctrl.signal,
      });
      const json = (await res.json().catch(() => ({}))) as Record<string, unknown>;
      return { status: res.status, body: json, requestId: json.request_id as string | undefined, failure: classify(res.status) };
    } catch {
      return { status: 0, body: undefined, failure: "offline" };
    } finally {
      clearTimeout(timer);
    }
  }

  version(): Promise<ApiResult> {
    return this.get("/v1/version", this.cfg.writeTimeoutMs);
  }

  recordEvent(request: Record<string, unknown>): Promise<ApiResult> {
    return this.post("/v1/evidence/events", request, this.cfg.writeTimeoutMs);
  }

  flush(request: Record<string, unknown>): Promise<ApiResult> {
    return this.post("/v1/extraction/flush", request, this.cfg.writeTimeoutMs);
  }

  compose(request: Record<string, unknown>, signal?: AbortSignal): Promise<ApiResult> {
    return this.post("/v1/context/compose", request, this.cfg.composeTimeoutMs, signal);
  }

  search(request: Record<string, unknown>): Promise<ApiResult> {
    return this.post("/v1/memories/search", request, this.cfg.writeTimeoutMs);
  }

  getMemory(memoryId: string): Promise<ApiResult> {
    return this.get(`/v1/memories/${encodeURIComponent(memoryId)}`, this.cfg.writeTimeoutMs);
  }

  remember(request: Record<string, unknown>): Promise<ApiResult> {
    return this.post("/v1/memories/remember", request, this.cfg.writeTimeoutMs);
  }

  correct(memoryId: string, request: Record<string, unknown>): Promise<ApiResult> {
    return this.post(`/v1/memories/${encodeURIComponent(memoryId)}/correct`, request, this.cfg.writeTimeoutMs);
  }

  forget(memoryId: string, request: Record<string, unknown>): Promise<ApiResult> {
    return this.post(`/v1/memories/${encodeURIComponent(memoryId)}/forget`, request, this.cfg.writeTimeoutMs);
  }
}
