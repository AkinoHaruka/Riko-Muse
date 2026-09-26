/**
 * DSH 宿主插件入口（doc2/02，官方 Cordis 形状已核对 @477b4f4）。
 *
 * - 导出 name / inject / apply(ctx, config)：由官方 profile + --patch 装载。
 * - token 用 ESM import 读取，只保存在进程内；日志不输出 token/hash/正文/密钥。
 * - captureEnabled 只控 L0 捕获；injectionEnabled 只控注入；toolsEnabled 只控工具注册。
 * - 卸载经 ctx.effect：停止接收 → 真实等待在途链 ≤5s → 释放句柄；未 ack spool 保留。
 * - daemon 离线仍捕获落 spool；恢复后先核 protocol_version=1 再按序发送。
 */
import { readFileSync } from "node:fs";
import type { Context } from "@deepseek-ai/cordis";
import type { Session, SessionEvent, SessionId, SessionSeq } from "@deepseek-ai/dsh-session";
import type { PreStepPayload } from "./recall.js";
import { makeBundleHook, makePreStepHook, makeSoulAssembleHook } from "./recall.js";
import { MemoryClient } from "./client.js";
import { loadConfig, type AdapterConfig } from "./config.js";
import { EventPipeline, type GapChecker, type Logger } from "./events.js";
import { Spool } from "./spool.js";
import { buildMemoryTools, type AgentLike, type ToolExecLike } from "./tools.js";

export const name = "agent-memory";
export const inject = ["tools"];

/** D6 capability（doc6/06 §1）：v6 注入（Soul section + bundle 前插）所需。 */
const REQUIRED_CAPABILITY = "context_bundle_v1";

// Cordis 插件配置（doc2/02 §3 字段见 config.ts；装载时非法即抛错拒绝加载）。
export type Config = Record<string, unknown>;

interface SessionQueryLike {
  readEvent(request: { sessionId: SessionId; seq: SessionSeq }): Promise<{ target: SessionEvent }>;
}

function makeLogger(ctx: Context): Logger {
  const anyCtx = ctx as { logger?: { warn(m: string): void; error(m: string): void; info(m: string): void } };
  const l = anyCtx.logger;
  return {
    warn: (m) => l?.warn(m),
    error: (m) => l?.error(m),
    info: (m) => l?.info(m),
  };
}

function readToken(path: string): string {
  // ESM import（G-02 修复）：禁止裸 require。
  const token = readFileSync(path, "utf8").trim();
  if (!token) throw new Error(`agent-memory: 令牌文件为空: ${path}`);
  return token;
}

class AgentMemoryPlugin {
  private readonly logger: Logger;
  private readonly client: MemoryClient;
  private readonly spool: Spool;
  private readonly pipeline: EventPipeline;
  private disposed = false;
  /** 装载期可降级：握手缺 capability 时停用 v6（doc6/06 §1 不静默落回）。 */
  private cfg: AdapterConfig;

  constructor(
    private readonly ctx: Context,
    rawConfig: Config,
  ) {
    this.cfg = loadConfig(rawConfig);
    this.logger = makeLogger(ctx);
    const token = readToken(this.cfg.userTokenFile);
    this.client = new MemoryClient({
      baseUrl: this.cfg.memoryUrl,
      token,
      writeTimeoutMs: this.cfg.writeTimeoutMs,
      composeTimeoutMs: this.cfg.composeTimeoutMs,
      soulTimeoutMs: this.cfg.soulTimeoutMs,
      bundleTimeoutMs: this.cfg.bundleTimeoutMs,
    });
    this.spool = new Spool(this.cfg.spoolDir, this.cfg.spoolLimitBytes);
    this.pipeline = new EventPipeline(
      this.spool,
      this.client,
      this.logger,
      this.cfg.hostId,
      this.cfg.captureEnabled,
    );
  }

  async start(): Promise<void> {
    const gapChecker = this.resolveGapChecker();
    await this.pipeline.start(gapChecker);

    if (this.cfg.contextBundleEnabled) await this.negotiateContextBundle();

    if (this.cfg.captureEnabled) {
      // session/event 是提交后的同步受保护通知；回调只做有界入队（doc2/03 §2）。
      this.ctx.on("session/event" as never, ((session: Session, event: SessionEvent) => {
        this.pipeline.observeSessionEvent(String(session.id), event);
      }) as never);
    }

    if (this.cfg.contextBundleEnabled) {
      // D6 v6 注入（doc6/06 §3）：Soul 走 assemble waterfall；resident/retrieved
      // 走 pre-step 前插。同一步不再调用旧 compose（不双调）。
      this.registerSoulAssemble();
      this.registerBundlePreStep();
    } else if (this.cfg.injectionEnabled) {
      const hook = makePreStepHook(this.client, this.logger, this.cfg.composeTimeoutMs);
      this.ctx.on("agent/pre-step" as never, (async (payload: PreStepPayload, next: () => Promise<never>) => {
        return hook(payload, next);
      }) as never);
    }

    if (this.cfg.toolsEnabled) {
      const tools = buildMemoryTools({
        client: this.client,
        pipeline: this.pipeline,
        logger: this.logger,
        hostId: this.cfg.hostId,
        writeTimeoutMs: this.cfg.writeTimeoutMs,
      });
      const registry = (this.ctx as { tools: { register(def: unknown): () => void } }).tools;
      for (const tool of tools) {
        registry.register(tool);
      }
    }

    this.logger.info?.(
      `agent-memory: 已加载 capture=${this.cfg.captureEnabled} injection=${this.cfg.injectionEnabled} `
      + `tools=${this.cfg.toolsEnabled} contextBundle=${this.cfg.contextBundleEnabled}`,
    );
  }

  /**
   * capability 握手（doc6/06 §1）：contextBundleEnabled 时核 `context_bundle_v1`。
   * 缺失时：require=true → throw 拒绝装载（fail loud）；否则停用 v6 并**连同旧
   * 注入一并停用**（不静默落回旧"两条指令"模式），报 error 诊断。
   */
  private async negotiateContextBundle(): Promise<void> {
    const r = await this.client.version();
    const caps = (r.body as { capabilities?: unknown } | undefined)?.capabilities;
    const has = Array.isArray(caps) && caps.includes(REQUIRED_CAPABILITY);
    if (has) return;
    const detail = `memoryd 缺少 capability ${REQUIRED_CAPABILITY}（status=${r.status}）；v6 注入不可用`;
    if (this.cfg.requireContextBundle) {
      throw new Error(`${detail}；requireContextBundle=true，拒绝装载（不落回旧注入模式）`);
    }
    this.cfg = { ...this.cfg, contextBundleEnabled: false, injectionEnabled: false };
    this.logger.error?.(`agent-memory: ${detail}；已停用全部自动注入（capture 与工具不受影响）`);
  }

  /** 注册 system-prompt/assemble waterfall：唯一 Soul system section。 */
  private registerSoulAssemble(): void {
    if (!this.cfg.agentName) {
      // DSH agent.id 是随机会话 ID（session-*），跨会话不稳；Soul 按
      // (tenant,user,agent_id) 隔离会退化为按会话空人格。doc6/03 §1 要求
      // agent 身份来自本进程配置——部署未提供时明确告警并回退。
      this.logger.error?.(
        "agent-memory: contextBundleEnabled 未配置 agentName；Soul 将回退到随机会话 ID，人格无法跨会话生效",
      );
    }
    const hook = makeSoulAssembleHook(
      this.client,
      this.logger,
      this.cfg.soulTimeoutMs,
      () => this.disposed,
      this.cfg.agentName,
    );
    // system-prompt/assemble 的分发 subject 是 context.scope（agent scope，doc6/03 §1
    // 预核）。每个 agent 创建时在其 scope ctx 上注册 scoped listener——官方
    // preset/persona 测试的同款模式；全局注册实测不触发（根因见交付记录）。
    (this.ctx as { on: (name: string, cb: unknown) => unknown }).on(
      "agent/created",
      async (payload: { agent: { ctx: unknown } }) => {
        const agentCtx = payload.agent.ctx as { on: (name: string, cb: unknown) => unknown };
        agentCtx.on("system-prompt/assemble", async (assembly: unknown, context: unknown, next: () => Promise<unknown>) => {
          return (hook as unknown as (
            a: unknown,
            c: unknown,
            n: () => Promise<unknown>,
          ) => Promise<unknown>)(assembly, context, next);
        });
      },
    );
  }

  /** 注册 agent/pre-step：bundle 的 resident/retrieved 前插（doc6/06 §3）。 */
  private registerBundlePreStep(): void {
    const hook = makeBundleHook(this.client, this.logger, this.cfg.bundleTimeoutMs, this.cfg.agentName);
    this.ctx.on("agent/pre-step" as never, (async (payload: PreStepPayload, next: () => Promise<never>) => {
      return hook(payload, next);
    }) as never);
  }

  /** 官方异步定点读取：已知 session 的缺口对账用（doc2/03 §2 恢复边界）。 */
  private resolveGapChecker(): GapChecker | undefined {
    const anyCtx = this.ctx as { get?: (key: string) => unknown };
    const sessionQuery = anyCtx.get?.("sessionQuery") as SessionQueryLike | undefined;
    if (!sessionQuery || typeof sessionQuery.readEvent !== "function") return undefined;
    return {
      readEvent: async (sessionId: string, seq: number) => {
        try {
          const window = await sessionQuery.readEvent({ sessionId: sessionId as SessionId, seq: seq as SessionSeq });
          return window.target;
        } catch {
          return undefined;
        }
      },
    };
  }

  /** 真实收尾：等待在途发送链最多 5 秒，保留未 ack spool，释放文件句柄。 */
  async dispose(): Promise<void> {
    if (this.disposed) return;
    this.disposed = true;
    await this.pipeline.dispose();
    this.spool.dispose();
  }
}

/** Cordis 插件入口：apply 必须同步完成装载契约，异步启动错误要可见。 */
export function apply(ctx: Context, config: Config = {}): void {
  const plugin = new AgentMemoryPlugin(ctx, config);
  // ctx.effect：卸载时由宿主回收；dispose 返回 Promise，由 Cordis 等待真实收尾。
  (ctx as { effect: (register: () => (() => void) | Promise<void>) => void }).effect(() => () => {
    void plugin.dispose().catch((e: unknown) => {
      // 收尾失败只记录：未 ack spool 已保留，不影响 DSH。
      const anyCtx = ctx as { logger?: { error(m: string): void } };
      anyCtx.logger?.error(`agent-memory: 卸载收尾异常: ${String(e)}`);
    });
  });
  plugin.start().catch((e: unknown) => {
    const anyCtx = ctx as { logger?: { error(m: string): void } };
    anyCtx.logger?.error(`agent-memory: 启动失败: ${e instanceof Error ? e.message : String(e)}`);
    throw e;
  });
}

export type { AgentLike, ToolExecLike };
