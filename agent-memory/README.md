# Agent Memory（memoryd）

Rust 优先的通用 Agent 长期记忆内核：同一用户的多 Agent 共享同一份长期记忆，不同用户严格隔离。设计契约见 `../doc/`（v1 冻结：`../doc/10-开发冻结规范.md`；实现契约：`../doc/11`～`15`）。

**状态（分层，勿混为一类。更新：2026-09-25，详见 `../doc-handoff/04-V2-6交付记录.md`）：**

| 层 | 状态 |
|---|---|
| HTTP 协议验证（curl 实测 + 26 个单测） | ✅ 本机 Windows 实测 |
| DSH 实际运行（官方 clone 真实宿主闭环） | ✅ headless profile `memory-hl` 真实会话闭环（capture/五工具/注入/纠错/遗忘/跨用户 404/spool 离线恢复） |
| 模型真实连通 | ❌ **未验证**——全部宿主闭环使用本地假模型（mock，仅测试装置），无可用真实端点 |
| 构建安装部署 | ⚠️ 本机构建通过（cargo/tsc）；未做安装分发，其他操作系统未验证 |

## 快速开始（Windows 本机已验证）

```bash
# 1. 配置（非秘密项）
cp config.example.toml config.toml

# 2. 创建用户令牌（32 字节随机，base64url 写入新文件，数据库只存 SHA-256）
cargo run -p memory-server --bin memoryd -- principal add \
  --tenant local --user alice --token-out alice.token --db data/agent-memory.db

# 3. 启动内核（只监听 127.0.0.1:8791；迁移完成后才开放 HTTP）
cargo run -p memory-server --bin memoryd -- serve --config config.toml

# 4. 检查
curl http://127.0.0.1:8791/v1/version    # {"protocol_version":1,"schema_version":2,...}
curl http://127.0.0.1:8791/v1/health     # {"status":"ok","db":"ready","index":"ready|degraded"}
```

## CLI

| 命令 | 作用 |
|---|---|
| `memoryd serve --config <file>` | 启动内核（只监听 loopback；非 loopback 绑定拒绝启动） |
| `memoryd principal add --tenant <id> --user <id> --token-out <file> --db <path>` | 创建用户令牌（文件不能已存在） |
| `memoryd principal rotate-token ...` | 换令牌，原令牌立即失效 |
| `memoryd doctor --config <file>` | 只读诊断：schema 版本、principals 数、索引 generation/dirty |
| `memoryd rebuild-index --config <file>` | 从 active 规范表全量重建 FTS/grams（不复活 forgotten） |
| `memoryd backup --config <file> --out <path>` | SQLite 在线一致性备份（VACUUM INTO） |

恢复（停机维护）：停止服务 → 备份现库 → 校验恢复文件 schema 与用户列表（doctor）→ 替换 → 启动。

## HTTP 协议 v1（全部端点）

除 `/v1/health`、`/v1/version` 外均需 `Authorization: Bearer <token>`；服务端由令牌查 `(tenant_id,user_id)`，**请求正文不含用户 ID**；未知字段 400 `INVALID_FIELD`。

| 端点 | 说明 |
|---|---|
| `GET /v1/health` · `GET /v1/version` | 健康与版本（无需认证） |
| `POST /v1/evidence/events` | L0 事件幂等接收（同键同 hash 200，同键异 hash 409 `EVENT_CONFLICT`） |
| `POST /v1/extraction/flush` | 关窗排队提取作业（`window_key=v1:<seq>` 幂等） |
| `GET /v1/jobs/{id}` · `POST /v1/jobs/{id}/retry` | 作业状态 / dead 重排 |
| `POST /v1/memories/remember` | 用户显式记忆（quote 须为最新用户消息连续子串） |
| `GET /v1/memories/{id}` | 单条 active 记忆（跨用户 404） |
| `POST /v1/memories/search` | FTS5（拉丁）+ 二元字（中文）+ RRF(k=60)；`include_history` 需历史词 |
| `POST /v1/context/compose` | 有界注入（默认 5 条 / 2000 字符，指令类优先） |
| `POST /v1/memories/{id}/correct` | 纠错（supersede 旧 + 激活新，`supersedes` 关系） |
| `POST /v1/memories/{id}/forget` | 遗忘（即时不可见；写 `suppressed_sources` 防重放复活；原始 L0 保留） |

错误码全集（v1）：`INVALID_JSON` `INVALID_FIELD` `UNAUTHENTICATED` `FORBIDDEN` `NOT_FOUND` `BODY_TOO_LARGE` `EVENT_CONFLICT` `QUOTE_MISMATCH` `STALE_USER_EVIDENCE` `VERSION_CONFLICT` `STATE_CONFLICT` `AMBIGUOUS_TARGET` `MODEL_UNAVAILABLE` `INDEX_DEGRADED` `RATE_LIMITED` `INTERNAL`（定义：`crates/memory-contract`）。

## DSH 适配器（`adapters/dsh/`）

TypeScript 薄适配器（Cordis 插件），按官方 `deepseek-harness@477b4f4`（0.1.7-rc.2）源码事实重写并通过对真实 DSH lib 类型的 `tsc` 检查（宿主事实核对路径：`packages/core/agent-loop`、`packages/llm/llm-deepseek/src/serialize.ts`、`packages/session/session-format-v3-to-v4/src/message-sources.ts`）：

- Cordis 插件形状：导出 `name`/`inject`/`apply(ctx, config)`；`inject: [tools]`，sessionQuery 软探测；
- `ctx.on('session/event', ...)`：`event.time` 为 Unix 毫秒数；`user/message` 的 `data` 即 `UserMessage`；
- `ctx.on('agent/pre-step', ...)`：waterfall `await next()` 后按 compose 结果追加注入消息；
- 注入 source 用 producer 自有 kind `{kind:'agent-memory', form:'recall'}`——**v4 会话格式已退役 `kind:'plugin'`**（`doc2/04` 相应表述已过时，以官方源码为准）；
- `defineTool` 五个 `memory_*` 工具，输出固定 JSON 对象 `{ok,data?,error?}`。

运行时行为：事件先落本地 spool（`spoolDir/events.jsonl`，追加 + fsync，100 MiB 上限）再异步发送；内核离线时 DSH 对话不受影响，恢复后启动重放按幂等键重发（同键返回既有 evidence_id）。详见 `../doc2/03`、`../doc2/04`。

## 安全边界

- 请求正文不接受 `tenant_id/user_id`；scope 只由 Bearer 令牌在服务端解析。
- 令牌与模型密钥严禁写入仓库、配置或日志；模型密钥经 `model_key_file` 读取。
- 首版只监听 loopback；一个 DSH 进程服务一名配置用户（多用户进程需 DSH 可信身份接口，v1 不开放）。
- 自动 `active` 仅限用户明确直接陈述（`extract_v1` 确定性窄规则）；推断与敏感内容一律 `held`。

## 目录

- `crates/memory-contract`：协议常量、错误码、v1 版本化默认限额
- `crates/memory-domain`：ScopeKey、状态机、`normalize_v1`、claim 哈希（纯 Rust）
- `crates/memory-store-sqlite`：迁移、principals、evidence、memories、jobs、索引事务
- `crates/memory-extract`：`extract_v1` Prompt、模型协议、候选准入规则 1–7
- `crates/memory-recall`：词法（latin tokens / CJK bigrams）、RRF
- `crates/memory-server`：`memoryd` 二进制（CLI + HTTP + worker）
- `adapters/dsh`：DSH TypeScript 薄适配器
- `migrations/0001_init.sql`：规范库蓝图（schema 1，已发布、checksum 固定，禁止回改）
- `migrations/0002_prompt_version.sql`：schema 2——`extraction_jobs.prompt_version`（doc2/05 §3）；
  旧库启动时自动有序升级，旧作业回填 `extract_v1`。**升级前先用 `memoryd backup` 备份**。

## 已验证与未验证

**已验证（本机 Windows，26 个单测 + curl 实测 + 官方 DSH headless 真实闭环，2026-09-25）**：双用户令牌隔离与轮换；L0 幂等/冲突；Agent A→B 跨 Agent 记忆闭环（remember/search/compose，含 DSH 宿主内注入可见性）；中文 grams 与英文 FTS 双路检索；纠错/遗忘/版本锁/幂等/抑制复活（DSH 宿主内多步工具回路）；flush 幂等与作业状态；重启恢复；**spool 离线重放（停内核→落盘→恢复→同键幂等重放）**；rebuild-index/backup/doctor；非 loopback 拒绝。

**未验证**：真实模型端点连通与提取质量（宿主闭环均用本地假模型；自动提取 worker 用固定响应验证）；插件 HMR 卸载后工具消失；其他操作系统。one-shot headless 模式下 assistant/tool 事件捕获受退出竞态影响（用户事件不受影响），headless 无 sessionQuery、缺口对账不运行。

**不声称**：EverOS/Hindsight 的基准成绩、SOTA 准确率、生产多租户能力。首版只报告上述本机实测行为。
