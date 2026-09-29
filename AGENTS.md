# AGENTS.md —— 本仓库的 Agent 操作手册

面向在本仓库执行任务的 AI Agent。先读本文件，再读对应阶段的规范目录。**不确定接口形状时去源码核对并写下路径，不要凭记忆或旧 checkout 猜。**

## 1. 这是什么

通用 Agent 记忆内核：Rust 服务端 `memoryd`（SQLite + HTTP v1）是唯一裁决者；DeepSeek Harness（DSH）是第一个宿主，适配器是 TypeScript 薄层。

- 同一用户的不同 Agent 默认共享记忆；不同用户按服务端 Bearer token 的 scope 隔离。
- 记忆可信度由「可定位的用户证据 + Rust 准入规则」决定，模型输出只是候选。
- 首版从空库开始，不迁移 Riko／Companion 旧数据；不做模型海试或大规模基准。

## 2. 当前状态（2026-09-29）

```
当前基线：main（每次开工现场核对 branch、HEAD、status；不要依赖文档中的旧 SHA）
DSH 适配：官方本地 clone 已更新到 `0.2.0-rc.1 / 4878cdabd87d4041bdaff61d04c966883b9fd07a`；Riko bundle `0.5.1` 已发布，隔离 profile 安装与配置组合通过，完整插件激活/内核连接未验证，证据见 `doc-handoff/21-DSH-0.2适配.md`。Android 模型设置 Bridge 是本地后续改动，生产主机尚未更新
v1（doc/ 卡 0–6）、v2（doc2/ 卡 V2-0…V2-6）、doc4（D4-0…D4-7）、doc5（D5-0…D5-6）：已交付；已验证档位见 doc-handoff/README.md
现有内核 schema 13（迁移 0001—0013）；`0001`—`0013` 均冻结，新增迁移须从 `0014` 顺序递增；D5 新作业 extract_v3/admit_v2，历史版本按作业行冻结
remember 直写内容护栏按用户决定全部解除；保留 scope、最新用户证据、逐字 span、幂等与审计（doc-handoff/12）
D6-0—D6-15 已有实现；D6-11—D6-15 的 Rust/TypeScript 检查与官方 DSH 固定响应证据见 doc-handoff/20。真实模型质量、全量 E01—E25、性能、用户库升级和部署仍未验证/未执行；新开发以 doc6 当前规范和最新交接记录为准
用户库 dana/realtest 未升级；没有单独部署指令不得触碰，升级前先只读快照并按交接记录处理 index_dirty
最新实施入口：doc-handoff/README.md + doc-handoff/20-D6-11-15交付记录.md + doc-handoff/21-DSH-0.2适配.md + doc6/README.md + doc6/08-施工任务卡.md
```

## 3. 目录与只读边界

| 路径 | 性质 |
|---|---|
| `agent-memory/` | **唯一实现目录**：`crates/`（Rust）、`migrations/`、`adapters/dsh/`（TS 适配器）、`config.example.toml`、`README.md` |
| `doc/` | v1 规范：目标、数据模型、HTTP v1、算法契约、任务卡 |
| `doc2/` | v2 施工规范（官方 DSH 源码事实 + 修复任务卡 + 运行手册） |
| `doc5/` | doc5 记忆质量规则（产品决定与施工规范，已实施；未跟踪） |
| `doc6/` | D6 产品与施工规范（D6-0—D6-15 已实施；人格/Soul、Resident、语义召回与合并、Dream、生命周期治理及受限 child 整理） |
| `doc-handoff/` | 交接文档：环境复现、已完成证据、待办与冲突、可复制交接 Prompt |
| `deepseek-harness/` | 官方 DSH clone（`4878cdabd87d4041bdaff61d04c966883b9fd07a`，0.2.0-rc.1）——**未跟踪、只读参考**，不提交、不改源码 |
| `EverOS/`、`hindsight/`、`tencentdb-agent-memory/` | 上游参考仓库，**只读**，算法来源见 `doc/02` |
| `.workbuddy/` | 项目数据，**不要删除** |

**提交纪律**：禁止 `git add .` 或递归暂存根目录。`deepseek-harness/` 保持未跟踪；只暂存明确的本项目路径。提交前必看 `git status --short`。

## 4. 规范优先级

1. 用户明确决定
2. `doc6/`（D6 新能力的已批准产品与施工规范；含逐卡顺序和冻结边界）
3. `doc2/` 与现场核对的官方 DSH 源码事实（宿主接线以当前源码为准）
4. `doc/10` 与 `doc/11`–`doc/15`（v1 冻结契约；D6 只通过新增表/接口扩展，不回改旧语义）
5. `doc/01`–`doc/09`（背景解释）
6. 横评报告建议（调研入口，不能代替源码）

`doc6/` 与官方 DSH 当前源码冲突时：按 D6-0 记录路径、行号、冲突和影响，先更新规范，再实现依赖部分。官方源码事实不能靠类型声明、旧 clone 或旧报告推断；每次开工现场核对 clone HEAD。`doc2/` 与官方 DSH 源码冲突时同样以源码为准，更新文档并记录影响（历史案例见 `doc-handoff/03`）。

## 5. 硬性规则

- **scope 与来源准入只由 Rust 决定**。适配器不得在请求 JSON 里发 `tenant_id`/`user_id`；不得让模型填 `evidence_id`；不得用「最后一条正文事件」或文本相等猜证据。
- **D6 迁移**：禁止回改 `migrations/0001_init.sql`—`0013`；后续新增迁移从 `0014` 顺序递增，并同步 `SCHEMA_VERSION`、版本端点、CLI/doctor。开工先核 migration checksum 与 `Store::open` 行为；`Store::open` 会自动迁移，因此不得用新二进制对 dana/realtest 原库跑 serve、backup、doctor 或其他会打开 Store 的命令。先对原文件做只读一致性快照，再只在副本演练；未获独立部署指令不得升级原库。
- **DSH 源码核对**：D6-0 与 D6-11 的官方 DSH seam 已记录在 `doc-handoff/13-D6源码核对.md`。今后变更 DSH hook/subagent 接线时，仍须现场核当前 clone HEAD 与接口，并记录路径/行号；如果没有受控后台 dispatch seam，停止依赖该 seam 的实施并报告 runner 选择点；不得把普通 worker 改名为 subagent。
- **D6 产品边界**：Soul 由用户/管理员编辑，按用户+Agent 隔离且只有 Soul 进入 system；Resident 是可重建视图，问题目录默认空；Resident、原子记忆与有来源派生知识进入 user-role context。普通 turn/end/flush 不触发 LLM；定时 Auto Dream 默认启用，保留用户可配置关闭的能力，参数和上限依 doc6/10。Agent retire/restore 必须提交最新用户事件与精确 quote/span，由 Rust 校验；Agent/Dream 不得 purge。retention 默认关闭，可信用户/管理员明确设正值后，按该策略自动清理无需逐批手动确认。
- **保持版本冻结**：不得静默改变 `extract_v1/v2/v3`、`admit_v1/v2` 的历史作业行为；D6 新 Prompt/准入按新版本独立实现。不得恢复已由用户撤销的 remember 内容类别、保存指令或单命题护栏。
- **所有新内容带清晰来源**：派生问题画像/主题页不能覆盖 L0/L1 或 Soul；失效来源的派生文档不可继续注入。purge 要按 doc6 闭包清理正文、索引、审计/候选等可反查记录，不能把逻辑隐藏说成永久擦除。
- **禁止把本地假模型（mock）的结果写成「真实模型连通」**。无可用端点时按 `doc2/05 §5` 记「未验证」。
- **状态用语分开写**：代码存在 / 构建通过 / 配置预览 / 真实 DSH 闭环 / 真实模型连通 / 已安装部署。不得把「测试通过、curl 通过、插件装载成功、模型连通、用户可用」混成一个「完成」。
- 不改上游仓库与官方 clone；不写凭据、令牌、完整私密正文进文档或提交。

## 6. 常用命令

```bash
# Rust 内核
cd agent-memory && cargo build --workspace && cargo test --workspace
./target/debug/memoryd serve --config <本机 config.toml>     # 版本/健康：curl /v1/version /v1/health
./target/debug/memoryd principal add --tenant <t> --user <u> --token-out <file> --db <db>
./target/debug/memoryd doctor|rebuild-index|backup --config <file>

# DSH 适配器（依赖官方 clone 先构建）
cd agent-memory/adapters/dsh && npm install
./node_modules/.bin/tsc --noEmit -p tsconfig.json && ./node_modules/.bin/tsc -p tsconfig.json

# 官方 DSH clone（本机已装好，一般无需重跑）
corepack pnpm install --ignore-scripts && corepack pnpm run build:lib:host
DEEPSEEK_BASE_URL=http://127.0.0.1:3977/anthropic DSH_HOME=<home> \
  corepack pnpm dsh --profile memory-hl --patch <patch.yml> "<prompt>"
```

## 7. 本机已知坑

- 编译 Rust 前先杀掉 8791 端口的 `memoryd`（占着 `target/debug/memoryd.exe` → LNK1104）。
- 官方 clone 用 `corepack pnpm`（11.7.0，系统 pnpm 10.33 不可用）；必须 `--ignore-scripts`（lefthook postinstall 被本机 safe-delete 钩子挡）；包级无 `build` 脚本，只能用根 `build:lib:host`；根 `.npmrc` 需 `verify-deps-before-run=false`。
- 适配器靠 `file:` 链接解析真实 DSH 类型 → **先构建官方 clone 的 host 库**，否则 tsc 找不到 `lib/types/*.d.ts`。
- 本机 `rm` 会被 safe-delete 钩子拦截（尤其 `.git` 下文件），改用 `python -c "os.replace(...)"`。
- Node ESM 下 `new URL(...).pathname` 在 Windows 带前导斜杠，写文件要用 `fileURLToPath`。
- crates.io 曾出现拉取缓慢/失败；新增 Rust 依赖失败时如实记录阻碍，不要用手写半成品替代（模型客户端已改为 reqwest）。

## 8. 开工顺序

- 继续 D6 工作 → 先读本文件、`doc-handoff/README.md`、`doc-handoff/20-D6-11-15交付记录.md`、`doc6/README.md`、`doc6/01`—`doc6/17` 与 `doc6/08` 中相关卡；检查 `git status --short`、branch、HEAD、迁移版本、官方 DSH clone HEAD。保留用户已有 `agent-memory/README.md` 修改和所有未跟踪目录。
- D6-0、D6-1—D6-15 已完成；不得把历史施工卡重新当作未实施。若后续规范与 runtime/API 不一致，先把精确源码证据写入 `doc-handoff/13-D6源码核对.md` 并更新规范，再实施依赖改动。D6 新迁移从 `0014` 开始，禁止改写 `0001`—`0013`。
- 既有 v2 修复 → 读 `doc-handoff/README.md` + `doc2/06` 对应卡与 `doc2/07` 运行手册；不要将历史交付状态当成当前工作区验证结果。
- 每项交付只运行与该项相称、且用户/任务要求的构建或验证；保留真实命令和输出，注明验证档位。代码改动按本仓库/卡片要求运行 Rust/TS 检查；文档改动无需运行代码测试。mock、离线测试、真实 DSH、真实模型和用户库部署分别报告。
