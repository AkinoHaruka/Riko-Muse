/**
 * Rust 内核 HTTP 客户端（doc/12 协议 v1）。
 * 令牌只在内存中持有；compose/write/Soul 超时来自插件配置，bundle 跟随调用方取消且无固定延迟预算；
 * 日志不输出令牌与正文。
 * 错误分类（doc2/03 §3）：unauthorized(401) / permanent(400,409) / offline(网络、超时、429、5xx)。
 */

export type FailureClass = "ok" | "unauthorized" | "permanent" | "offline";

export interface ClientConfig {
  baseUrl: string;
  token: string;
  writeTimeoutMs: number;
  composeTimeoutMs: number;
  soulTimeoutMs: number;
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

  private async post(path: string, body: unknown, timeoutMs?: number, signal?: AbortSignal): Promise<ApiResult> {
    const ctrl = timeoutMs === undefined ? undefined : new AbortController();
    const timer = ctrl ? setTimeout(() => ctrl.abort(), timeoutMs) : undefined;
    const requestSignal = signal && ctrl
      ? AbortSignal.any([signal, ctrl.signal])
      : signal ?? ctrl?.signal;
    try {
      const res = await fetch(`${this.cfg.baseUrl}${path}`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: `Bearer ${this.cfg.token}`,
        },
        body: JSON.stringify(body),
        signal: requestSignal,
      });
      const json = (await res.json().catch(() => ({}))) as Record<string, unknown>;
      return { status: res.status, body: json, requestId: json.request_id as string | undefined, failure: classify(res.status) };
    } catch {
      return { status: 0, body: undefined, failure: "offline" };
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }
  }

  private async get(path: string, timeoutMs: number, signal?: AbortSignal): Promise<ApiResult> {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), timeoutMs);
    try {
      const res = await fetch(`${this.cfg.baseUrl}${path}`, {
        headers: { authorization: `Bearer ${this.cfg.token}` },
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

  /** 启动握手（doc6/06 §1）：读取 capabilities 数组。 */
  version(): Promise<ApiResult> {
    return this.get("/v1/version", this.cfg.writeTimeoutMs);
  }

  recordEvent(request: Record<string, unknown>): Promise<ApiResult> {
    return this.post("/v1/evidence/events", request, this.cfg.writeTimeoutMs);
  }

  flush(request: Record<string, unknown>): Promise<ApiResult> {
    return this.post("/v1/extraction/flush", request, this.cfg.writeTimeoutMs);
  }

  dreamTrigger(request: Record<string, unknown>): Promise<ApiResult> {
    return this.post("/v1/dream/triggers", request, this.cfg.writeTimeoutMs);
  }

  dreamRunnerHeartbeat(request: Record<string, unknown>, signal?: AbortSignal): Promise<ApiResult> {
    return this.post("/v1/dream/runner/heartbeat", request, this.cfg.writeTimeoutMs, signal);
  }

  dreamRunnerClaim(request: Record<string, unknown>, signal?: AbortSignal): Promise<ApiResult> {
    return this.post("/v1/dream/runner/claim", request, this.cfg.writeTimeoutMs, signal);
  }

  dreamRunnerLease(request: Record<string, unknown>, signal?: AbortSignal): Promise<ApiResult> {
    return this.post("/v1/dream/runner/lease", request, this.cfg.writeTimeoutMs, signal);
  }

  dreamSubmitCandidates(jobId: string, request: Record<string, unknown>, signal?: AbortSignal): Promise<ApiResult> {
    return this.post(`/v1/dream/jobs/${encodeURIComponent(jobId)}/candidates`, request, this.cfg.writeTimeoutMs, signal);
  }

  dreamSubmitAdjudication(jobId: string, request: Record<string, unknown>, signal?: AbortSignal): Promise<ApiResult> {
    return this.post(`/v1/dream/adjudications/${encodeURIComponent(jobId)}/submit`, request, this.cfg.writeTimeoutMs, signal);
  }

  dreamSubmitConsolidation(jobId: string, request: Record<string, unknown>, signal?: AbortSignal): Promise<ApiResult> {
    return this.post(`/v1/consolidation/jobs/${encodeURIComponent(jobId)}/publish`, request, this.cfg.writeTimeoutMs, signal);
  }

  dreamRunnerFailure(request: Record<string, unknown>, signal?: AbortSignal): Promise<ApiResult> {
    return this.post("/v1/dream/runner/failure", request, this.cfg.writeTimeoutMs, signal);
  }

  dreamRead(jobId: string, request: Record<string, unknown>, signal?: AbortSignal): Promise<ApiResult> {
    return this.post(`/v1/dream/jobs/${encodeURIComponent(jobId)}/read`, request, this.cfg.writeTimeoutMs, signal);
  }

  compose(request: Record<string, unknown>, signal?: AbortSignal): Promise<ApiResult> {
    return this.post("/v1/context/compose", request, this.cfg.composeTimeoutMs, signal);
  }

  /** GET /v1/soul（doc6/06 §2）：当前 agent 的用户编辑人格；不存在返回 version 0。 */
  soul(agentId: string, signal?: AbortSignal): Promise<ApiResult> {
    return this.get(`/v1/soul?agent_id=${encodeURIComponent(agentId)}`, this.cfg.soulTimeoutMs, signal);
  }

  /** POST /v1/context/bundle（doc6/06 §2）：resident + retrieved 分段与诊断。 */
  contextBundle(request: Record<string, unknown>, signal?: AbortSignal): Promise<ApiResult> {
    // Query embedding 已受 memoryd 的 provider timeout 保护；这里不另设 bundle 延迟预算。
    return this.post("/v1/context/bundle", request, undefined, signal);
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

  retire(memoryId: string, request: Record<string, unknown>): Promise<ApiResult> {
    return this.post(`/v1/memories/${encodeURIComponent(memoryId)}/retire`, request, this.cfg.writeTimeoutMs);
  }

  restore(memoryId: string, request: Record<string, unknown>): Promise<ApiResult> {
    return this.post(`/v1/memories/${encodeURIComponent(memoryId)}/restore`, request, this.cfg.writeTimeoutMs);
  }
}
