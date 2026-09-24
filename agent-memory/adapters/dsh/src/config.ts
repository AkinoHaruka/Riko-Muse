/**
 * DSH 适配器固定配置（doc/14 §1）。
 * token 绝不直接写配置：userTokenFile 指向 `memoryd principal add` 生成的文件。
 */
export interface Config {
  /** Rust 内核地址。默认 http://127.0.0.1:8791；默认不允许远端。 */
  memoryUrl: string;
  /** memoryd principal add 生成的令牌文件路径。 */
  userTokenFile: string;
  /** 本机持久 spool 目录（不能位于参考仓库源码内）。 */
  spoolDir: string;
  /** compose 默认 500ms，写默认 3000ms。 */
  requestTimeoutMs: number;
  /** 是否捕获会话事件（默认 true）。 */
  captureEnabled: boolean;
  /** 是否注入自动上下文（默认 true）。 */
  injectionEnabled: boolean;
}

export function loadConfig(raw: Partial<Config> | undefined): Config {
  return {
    memoryUrl: raw?.memoryUrl ?? "http://127.0.0.1:8791",
    userTokenFile: raw?.userTokenFile ?? "",
    spoolDir: raw?.spoolDir ?? "spool",
    requestTimeoutMs: raw?.requestTimeoutMs ?? 500,
    captureEnabled: raw?.captureEnabled ?? true,
    injectionEnabled: raw?.injectionEnabled ?? true,
  };
}
