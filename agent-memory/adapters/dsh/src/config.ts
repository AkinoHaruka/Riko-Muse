/**
 * 插件配置（doc2/02 §3）。全部字段在装载时校验，非法即拒绝加载。
 * token 绝不直接写配置：userTokenFile 指向 `memoryd principal add` 生成的文件。
 */

export interface AdapterConfig {
  /** Rust 内核基地址。只允许 http://127.0.0.1 / localhost / [::1]，禁止用户信息、查询串、路径片段。 */
  memoryUrl: string;
  /** memoryd principal add 生成的令牌文件（必须存在、可读）。 */
  userTokenFile: string;
  /** 本机持久 spool 目录（必填绝对路径，不在上游仓库源码树内）。 */
  spoolDir: string;
  /** 当前 DSH 安装的稳定来源 ID：非空、最长 256 字符；由安装持久提供。 */
  hostId: string;
  /** 上下文 compose 调用时限。默认 500ms。 */
  composeTimeoutMs: number;
  /** 单次 HTTP 写时限。默认 3000ms。 */
  writeTimeoutMs: number;
  /** L0 事件捕获与 flush。默认 true。只控制本行为，不影响工具与注入。 */
  captureEnabled: boolean;
  /** pre-step 自动注入。默认 true。 */
  injectionEnabled: boolean;
  /** 注册五个记忆工具。默认 true。 */
  toolsEnabled: boolean;
  /** 本地 spool 上限（events+receipts 总保留量）。默认 100 MiB。 */
  spoolLimitBytes: number;
}

const DEFAULT_MEMORY_URL = "http://127.0.0.1:8791";
const DEFAULT_COMPOSE_TIMEOUT_MS = 500;
const DEFAULT_WRITE_TIMEOUT_MS = 3000;
const DEFAULT_SPOOL_LIMIT_BYTES = 100 * 1024 * 1024;
const HOST_ID_MAX_CHARS = 256;

export function loadConfig(raw: unknown): AdapterConfig {
  const r = (raw ?? {}) as Record<string, unknown>;
  const memoryUrl = strOr(r.memoryUrl, DEFAULT_MEMORY_URL);
  assertLoopbackBaseUrl(memoryUrl);
  const userTokenFile = strOr(r.userTokenFile, "");
  if (userTokenFile.length === 0) throw new Error("userTokenFile 必填：指向 memoryd principal add 生成的令牌文件");
  const spoolDir = strOr(r.spoolDir, "");
  if (spoolDir.length === 0) throw new Error("spoolDir 必填：本机持久 spool 目录");
  if (!isAbsolutePath(spoolDir)) throw new Error(`spoolDir 必须是绝对路径: ${spoolDir}`);
  const hostId = strOr(r.hostId, "");
  if (hostId.length === 0) throw new Error("hostId 必填：当前 DSH 安装的稳定来源 ID（非空、非秘密、重启不变）");
  if (hostId.length > HOST_ID_MAX_CHARS) throw new Error(`hostId 最长 ${HOST_ID_MAX_CHARS} 字符`);
  return {
    memoryUrl,
    userTokenFile,
    spoolDir,
    hostId,
    composeTimeoutMs: positiveIntOr(r.composeTimeoutMs, DEFAULT_COMPOSE_TIMEOUT_MS, "composeTimeoutMs"),
    writeTimeoutMs: positiveIntOr(r.writeTimeoutMs, DEFAULT_WRITE_TIMEOUT_MS, "writeTimeoutMs"),
    captureEnabled: boolOr(r.captureEnabled, true),
    injectionEnabled: boolOr(r.injectionEnabled, true),
    toolsEnabled: boolOr(r.toolsEnabled, true),
    spoolLimitBytes: positiveIntOr(r.spoolLimitBytes, DEFAULT_SPOOL_LIMIT_BYTES, "spoolLimitBytes"),
  };
}

/** 只接受 loopback 基地址：无用户信息、无查询串、无路径片段（doc2/02 §1）。 */
function assertLoopbackBaseUrl(value: string): void {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new Error(`memoryUrl 不是合法 URL: ${value}`);
  }
  if (url.protocol !== "http:") throw new Error(`memoryUrl 只允许 http://，实际 ${url.protocol}`);
  const host = url.hostname;
  if (host !== "127.0.0.1" && host !== "localhost" && host !== "::1" && host !== "[::1]") {
    throw new Error(`memoryUrl=${value} 不是 loopback；首版不允许远端内核`);
  }
  if (url.username || url.password) throw new Error("memoryUrl 禁止包含用户信息");
  if (url.search) throw new Error("memoryUrl 禁止包含查询串");
  if (url.pathname !== "" && url.pathname !== "/") throw new Error("memoryUrl 禁止包含路径片段");
}

function isAbsolutePath(p: string): boolean {
  return /^([a-zA-Z]:[\\/]|\\\\|\/)/.test(p);
}

function strOr(v: unknown, dflt: string): string {
  return typeof v === "string" ? v : dflt;
}

function boolOr(v: unknown, dflt: boolean): boolean {
  return typeof v === "boolean" ? v : dflt;
}

function positiveIntOr(v: unknown, dflt: number, name: string): number {
  if (v === undefined || v === null) return dflt;
  if (typeof v !== "number" || !Number.isInteger(v) || v <= 0) {
    throw new Error(`${name} 必须是正整数，实际 ${JSON.stringify(v)}`);
  }
  return v;
}
