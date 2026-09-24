/**
 * DSH 薄适配器骨架（卡 0/卡 2 交付范围见 doc/15）。
 *
 * 职责边界（doc/03、doc/14）：
 * - 从宿主取得可信身份、会话事件与 Agent 生命周期；
 * - 将协议请求交给 Rust 内核；把返回的上下文接到 DSH；
 * - 不直接打开 SQLite；不改变用户作用域；旧 riko-memory 插件的作用域逻辑不复用。
 *
 * 接线项（session/event、agent/pre-step、defineTool ×5、spool）在卡 2/卡 3
 * 按目标 DSH 版本的实际 Hook 类型填充；本文件只固定配置加载与协议版本握手。
 */
import { loadConfig } from "./config.js";

export interface AdapterContext {
  config: ReturnType<typeof loadConfig>;
  /** 从令牌文件读取的 Bearer token；只在内存中短暂持有。 */
  token: string;
}

export async function handshake(baseUrl: string, token: string): Promise<void> {
  const res = await fetch(`${baseUrl}/v1/version`, {
    headers: { authorization: `Bearer ${token}` },
  });
  if (!res.ok) {
    throw new Error(`memoryd /v1/version 返回 ${res.status}；请检查令牌与内核状态`);
  }
  const body = (await res.json()) as { protocol_version?: number };
  if (body.protocol_version !== 1) {
    throw new Error(`协议版本不兼容：内核=${body.protocol_version ?? "unknown"}，适配器=1`);
  }
}

export function createAdapter(rawConfig: Parameters<typeof loadConfig>[0]): AdapterContext {
  const config = loadConfig(rawConfig);
  // TODO(卡 2)：读取 userTokenFile → token；校验 loopback；核对 /v1/version；装载 Hook。
  return { config, token: "" };
}
