# Agent Memory（memoryd）

Rust 优先的通用 Agent 长期记忆内核：同一用户的多 Agent 共享同一份长期记忆，不同用户严格隔离。设计契约见 `../doc/`（v1 冻结：`../doc/10-开发冻结规范.md`；实现契约：`../doc/11`～`15`）。

**状态：卡 0～5 已交付并本机验证（Windows）。** 卡 6 为本文件。未验证项见文末。

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
curl http://127.0.0.1:8791/v1/version    # {"protocol_version":1,"schema_version":1,...}
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

TypeScript 薄适配器，已通过 `tsc --noEmit` 类型检查。宿主 Hook 事实已在目标 checkout 核对（`C:\TRAE\Riko-dsh\deepseek-harness` @ `riko-memory/src/index.ts`，只读）：

- `ctx.on('session/event', (session, event) => ...)`：`event.type`/`event.seq`/`event.time`/`event.data.source.kind` 可用；
- `ctx.on('agent/pre-step', ({agent,messages,signal}, next)`：`next()` 返回 `decision`，`decision.kind==='enter'` 时追加 `decision.messages`；
- `defineTool({name,description,parameters,output,execute})`，`execute(args, exec)` 中 `exec.agent.session` 可用；
- `createUserMessage({content, source:{kind:'plugin', ...}})` 标记注入来源。

装载方式：适配器不硬依赖 DSH 内部包，宿主 glue（Hook 注册、`createUserMessage`、工具包装）由目标 DSH 版本的装载层注入。**在独立 profile 试装**，不与旧 `riko-memory` 插件同时注入。运行时行为：事件先落本地 spool（`spoolDir/events.jsonl`，100 MiB 上限）再异步发送；内核离线时 DSH 对话不受影响，重连后按幂等键重放。

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
- `migrations/0001_init.sql`：规范库蓝图

## 已验证与未验证

**已验证（本机 Windows，23 个单测 + curl 实测）**：双用户令牌隔离与轮换；L0 幂等/冲突；Agent A→B 跨 Agent 记忆闭环（remember/search/compose）；中文 grams 与英文 FTS 双路检索；纠错/遗忘/版本锁/幂等/抑制复活；flush 幂等与作业状态；重启恢复；rebuild-index/backup/doctor；非 loopback 拒绝。

**未验证**：真实模型端点连通与提取质量（准入规则用固定响应验证）；DSH 宿主内端到端（需独立 profile 试装）；其他操作系统。

**不声称**：EverOS/Hindsight 的基准成绩、SOTA 准确率、生产多租户能力。首版只报告上述本机实测行为。
