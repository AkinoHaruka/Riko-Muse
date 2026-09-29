# Riko-Memory（memoryd）

Rust 优先的通用 Agent 长期记忆内核：同一用户的多个 Agent 默认共享长期记忆，不同用户由服务端身份令牌严格隔离。本仓库包含可运行实现；D6 产品与施工规范见 `../doc6/`，实施证据见 `../doc-handoff/`。

**验证状态（更新：2026-09-29；验证档位分开记录）：**

| 层 | 状态 |
|---|---|
| Rust 确定性检查 | ✅ 当前工作区 `cargo fmt --all -- --check`、`cargo test --workspace`、`cargo build --workspace` 通过；schema 13 |
| DSH 宿主 | ✅ D6-11—D6-15 在官方 DSH + 固定响应中验证 Dream child、Rust receipt、主题页创建/更新；DSH 0.2 bundle 安装与配置预览通过，插件完整激活和 memoryd 连接未验证 |
| 真实模型 | ⚠️ 有历史小样本，见 `../doc-handoff/16`—`19`；D6-11—D6-15 新 child 流程、总体写入/召回质量及回答利用率未验收 |
| Android 模型设置 | ⚠️ 本地 Bridge API、模拟器 debug APK 与确定性接口测试已完成；生产 DSH 主机是否部署新路由未验证 |
| 部署与用户数据 | ⚠️ dana/realtest 未升级；生产 DSH bridge 未因本地代码改动而更新 |

## 记忆行为

自动提取的新作业使用 `extract_v3` + `admit_v2`；历史作业按其持久化版本继续处理。未知版本会确定性失败。自动准入规则要求单命题、可定位的用户原文，并将不符合规则的候选留在 `held`，而非自动激活。

- `memory_remember` 是显式直写接口：内容按请求和证据原文创建为 active 记忆，不设凭据、健康、第三人、时间性或命题数量的内容类别限制。服务端仍验证用户 scope、最新用户证据、逐字 quote、幂等、审计和遗忘抑制。调用者应按应用自身的隐私策略决定哪些内容适合保存。
- 召回时 instruction 最多有 2 个独立名额；fact/preference 等其他记忆按词法相关性召回。所有注入受条数和字符预算限制。

作业队列支持租约恢复、代际隔离、按实际序列化字节分窗和显式跳过 dead 作业。

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
  curl http://127.0.0.1:8791/v1/version    # {"protocol_version":1,"schema_version":13,...}
curl http://127.0.0.1:8791/v1/health     # {"status":"ok","db":"ready","index":"ready|degraded"}
```

## CLI

| 命令 | 作用 |
|---|---|
| `memoryd serve --config <file>` | 启动内核（只监听 loopback；非 loopback 绑定拒绝启动） |
| `memoryd principal add --tenant <id> --user <id> --token-out <file> --db <path>` | 创建用户令牌（文件不能已存在） |
| `memoryd principal rotate-token ...` | 换令牌，原令牌立即失效 |
| `memoryd doctor --config <file>` | 只读诊断：schema 版本、principals 数、索引 generation/dirty + 作业聚合计数（各状态、lease 过期 running、未跳过/已跳过 dead、held 数、最老待办时长） |
| `memoryd job skip --config <f> --tenant <t> --user <u> --job-id <完整ID> --reason <文本>` | 显式跳过 dead/WINDOW_TOO_LARGE 作业的自动提取（写审计；不删 L0；重复幂等） |
| `memoryd candidates list/show --config <f> --tenant <t> --user <u> ...` | held 候选只读查看（list 不含正文；show 含 quote，仅本机终端） |
| `memoryd rebuild-index --config <file>` | 从 active 规范表全量重建 FTS/grams（不复活 forgotten） |
| `memoryd backup --config <file> --out <path>` | SQLite 在线一致性备份（VACUUM INTO） |

恢复（停机维护）：停止服务 → 备份现库 → 校验恢复文件 schema 与用户列表（doctor）→ 替换 → 启动。

## HTTP 协议 v1（全部端点）

除 `/v1/health`、`/v1/version` 外均需 `Authorization: Bearer <token>`；服务端由令牌查 `(tenant_id,user_id)`，**请求正文不含用户 ID**；未知字段 400 `INVALID_FIELD`。

| 端点 | 说明 |
|---|---|
| `GET /v1/health` · `GET /v1/version` | 健康与版本（无需认证） |
| `POST /v1/evidence/events` | L0 事件幂等接收（同键同 hash 200，同键异 hash 409 `EVENT_CONFLICT`） |
| `POST /v1/extraction/flush` | 关窗提取（**服务端权威分窗**：按实际序列化字节切窗，单事件超限建 dead/WINDOW_TOO_LARGE，空洞/无用户组建 succeeded checkpoint；同 through 幂等） |
| `GET /v1/jobs` | scope 内分页作业列表（默认 dead；keyset cursor；诊断字段，无正文） |
| `GET /v1/jobs/{id}` · `POST /v1/jobs/{id}/retry` | 作业详情（补诊断字段）/ dead 重排（`WINDOW_TOO_LARGE` 拒绝原样 retry，走 skip） |
| `POST /v1/memories/remember` | 用户显式记忆（quote 须为最新用户消息连续子串） |
| `GET /v1/memories/{id}` | 单条 active 记忆（跨用户 404） |
| `POST /v1/memories/search` | FTS5（拉丁）+ 二元字（中文）+ RRF(k=60)；`include_history` 需历史词 |
| `POST /v1/context/compose` | 有界注入（默认 5 条 / 2000 字符，指令类优先） |
| `POST /v1/memories/{id}/correct` | 纠错（supersede 旧 + 激活新，`supersedes` 关系） |
| `POST /v1/memories/{id}/forget` | 遗忘（即时不可见；写 `suppressed_sources` 防重放复活；原始 L0 保留） |

错误码全集（v1）：`INVALID_JSON` `INVALID_FIELD` `UNAUTHENTICATED` `FORBIDDEN` `NOT_FOUND` `BODY_TOO_LARGE` `EVENT_CONFLICT` `QUOTE_MISMATCH` `STALE_USER_EVIDENCE` `VERSION_CONFLICT` `STATE_CONFLICT` `AMBIGUOUS_TARGET` `MODEL_UNAVAILABLE` `INDEX_DEGRADED` `RATE_LIMITED` `INTERNAL`（定义：`crates/memory-contract`）。

## DSH 适配器（`adapters/dsh/`）

TypeScript 适配器（Cordis 插件与 DSH bundle）。D6 child/hook 闭环曾在官方 DSH `477b4f4` 固定响应环境中验证；bundle peer 与配置兼容已在 `0.2.0-rc.1` 验证。当前版本的完整插件激活和 memoryd 连接仍未验证：

- Cordis 插件形状：导出 `name`/`inject`/`apply(ctx, config)`；`inject: [tools]`，sessionQuery 软探测；
- `ctx.on('session/event', ...)`：`event.time` 为 Unix 毫秒数；`user/message` 的 `data` 即 `UserMessage`；
- `ctx.on('agent/pre-step', ...)`：waterfall `await next()` 后按 compose 结果追加注入消息；
- 注入 source 用 producer 自有 kind `{kind:'agent-memory', form:'recall'}`；
- `defineTool` 五个 `memory_*` 工具，输出固定 JSON 对象 `{ok,data?,error?}`。

运行时行为：事件先落本地 spool（`spoolDir/events.jsonl`，追加 + fsync，100 MiB 上限）再异步发送；内核离线时 DSH 对话不受影响，恢复后启动重放按幂等键重发（同键返回既有 evidence_id）。D6 还提供稳定 Agent Soul/Resident 注入、受限 Dream child 读取与 Rust 裁决、来源校验的主题页整理。

Riko-App Bridge 由同一 bundle 的 `riko-app-api` Host 插件提供。模型设置接口映射到 DSH `settings`、`credentials` 和 `llm.discoverModels`：密钥只写入 DSH credentials，不由 Bridge 回读。Android 设置页已接入提供商密钥写入/删除、自定义 OpenAI/Anthropic 兼容提供商和模型发现；部署前应先按 `../doc-handoff/21-DSH-0.2适配.md` 核对当前生产 Bridge 版本。

## 安全边界

- 请求正文不接受 `tenant_id/user_id`；scope 只由 Bearer 令牌在服务端解析。
- 令牌与模型密钥不得写入仓库或日志；模型密钥经 `model_key_file` 读取。
- 首版只监听 loopback；一个 DSH 进程服务一名配置用户（多用户进程需 DSH 可信身份接口，v1 不开放）。
- 自动提取只有通过当前版本的准入规则才会创建 active 记忆；直写 API 按上述语义处理，不替调用方做内容敏感度决策。

## 目录

- `crates/memory-contract`：协议常量、错误码、v1 版本化默认限额
- `crates/memory-domain`：ScopeKey、状态机、`normalize_v1`、claim 哈希（纯 Rust）
- `crates/memory-store-sqlite`：迁移、principals、evidence、memories、jobs、索引事务
- `crates/memory-extract`：版本化提取 Prompt、模型协议与候选准入策略
- `crates/memory-recall`：词法（latin tokens / CJK bigrams）、RRF
- `crates/memory-server`：`memoryd` 二进制（CLI + HTTP + worker）
- `adapters/dsh`：DSH TypeScript 薄适配器
- `migrations/0001_init.sql`—`0013_topic_page_description.sql`：当前 schema 13；`0001`—`0013` 均冻结，后续迁移从 `0014` 开始。**升级任何用户库前先做只读快照并按交接手册演练**。

## 已验证与未验证

**已验证（本机 Windows）**：核心 HTTP、隔离与生命周期路径有历史 curl/固定响应证据；D6-11—D6-15 当前检查为 `cargo fmt --all -- --check`、`cargo test --workspace`、`cargo build --workspace` 通过，官方 DSH 固定响应验证 Dream child 提取/裁决、页面创建/更新与索引回归。完整证据和验证边界见 `../doc-handoff/20-D6-11-15交付记录.md`。

**历史真实模型样本**：曾用 SiliconFlow、OpenRouter 与 Gemini 进行提取、召回和 Dream/embedding 探针；结果、失败和请求量分开记录在 `../doc-handoff/16`—`19`。这些小样本不代表新 D6 child 流程或整体记忆质量通过。

**未验证**：D6 新 Dream child 的真实模型质量、整体记忆写入/召回与回答利用率、全量 E01—E25、规模性能、当前 DSH 0.2 插件完整激活与生产 Bridge 部署、用户库升级及其他操作系统。one-shot headless 模式下 assistant/tool 事件捕获受退出竞态影响（用户事件不受影响），headless 无 sessionQuery、缺口对账不运行。

**不声称**：EverOS/Hindsight 的基准成绩、SOTA 准确率、生产多租户能力。首版只报告上述本机实测行为。
