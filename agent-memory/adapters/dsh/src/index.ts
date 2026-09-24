/**
 * DSH 薄适配器装配（doc/14）。
 *
 * 职责边界：从宿主取得可信身份/会话事件/Agent 生命周期 → 协议请求交 Rust 内核 →
 * 把上下文接到 DSH。不直接打开 SQLite；不复用旧 riko-memory 的 ownerNamespace+preset 作用域。
 *
 * 宿主 glue（Hook 注册、createUserMessage、defineTool 包装）由目标 DSH 版本的装载层注入，
 * 本包只依赖公开协议，便于独立 typecheck 与单元测试。
 */
import { MemoryClient } from "./client.js";
import type { Config } from "./config.js";
import { EventPipeline, type HostSessionEvent, type Logger } from "./events.js";
import { makeComposeHook } from "./recall.js";
import { Spool } from "./spool.js";
import { registerMemoryTools, type ToolHost } from "./tools.js";

export interface HostGlue {
  logger: Logger;
  /** ctx.on('session/event', cb) */
  onSessionEvent(cb: (session: unknown, event: HostSessionEvent) => void): void;
  /** ctx.on('agent/pre-step', cb) */
  onPreStep(cb: (args: { agent: { id: unknown }; messages: readonly unknown[]; signal: { aborted: boolean } }, next: () => Promise<never>) => Promise<unknown>): void;
  createUserMessage(opts: {
    content: { type: "text"; text: string }[];
    source: { kind: string; plugin: string; form: string };
  }): unknown;
  toolHost: ToolHost;
}

export interface Adapter {
  dispose(): void;
}

export async function createAdapter(config: Config, glue: HostGlue): Promise<Adapter> {
  // 启动校验（doc/14 §1）：令牌文件存在、URL 为 loopback、协议版本兼容、spool 可写。
  const token = readTokenFile(config.userTokenFile);
  const url = new URL(config.memoryUrl);
  if (url.hostname !== "127.0.0.1" && url.hostname !== "localhost" && url.hostname !== "::1") {
    throw new Error(`memoryUrl=${config.memoryUrl} 不是 loopback；首版不允许远端内核`);
  }
  const client = new MemoryClient({
    baseUrl: config.memoryUrl,
    token,
    writeTimeoutMs: 3000,
    composeTimeoutMs: config.requestTimeoutMs,
  });
  const version = await client.version();
  if (version.status !== 200) {
    throw new Error(`memoryd /v1/version 返回 ${version.status}；请检查内核是否运行`);
  }
  const protocol = (version.body as { protocol_version?: number }).protocol_version;
  if (protocol !== 1) {
    throw new Error(`协议版本不兼容：内核=${protocol ?? "unknown"}，适配器=1`);
  }

  const spool = new Spool(config.spoolDir);
  const pipeline = new EventPipeline(spool, client, glue.logger, "dsh");
  await pipeline.replayPending(); // 重启重放未 ack 操作（doc/14 §3）

  let agentId = "unknown-agent";
  glue.onSessionEvent((session, event) => {
    const sessionId = glue.toolHost.sessionId(session);
    glue.toolHost.latestUserText(session); // 保持宿主消息通道活跃（部分宿主惰性构造）
    pipeline.observeSessionEvent(sessionId, event, agentId);
  });
  glue.onPreStep(makeComposeHook(client, glue, glue.logger, config.requestTimeoutMs) as never);
  if (config.captureEnabled) {
    registerMemoryTools(glue.toolHost, client, pipeline, glue.logger);
  }

  return {
    dispose() {
      pipeline.dispose();
    },
  };
}

function readTokenFile(path: string): string {
  const { readFileSync } = require("node:fs") as typeof import("node:fs");
  const token = readFileSync(path, "utf8").trim();
  if (!token) throw new Error(`令牌文件为空: ${path}`);
  return token;
}
