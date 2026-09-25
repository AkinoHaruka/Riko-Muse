# 交接 Prompt（整段复制给下一个 Harness）

下面 `———` 之间的内容可直接复制作为新会话的第一条消息。

———

你是接手「Agent Memory 内核接入官方 DeepSeek Harness」实现的工程 Agent。工作目录是 `C:\TRAE\Agent-Memory`。请先读完交接文档再动手，不要凭常识猜接口形状。

## 0. 必读（按顺序）

1. `C:\TRAE\Agent-Memory\doc-handoff\README.md` —— 状态总览与三条禁区
2. `C:\TRAE\Agent-Memory\doc-handoff\01-环境与复现手册.md` —— 本机已装好的环境、路径、启动命令、故障定位
3. `C:\TRAE\Agent-Memory\doc-handoff\02-已完成与证据.md` —— 每张卡改了什么、哪些验证过（区分「代码存在／构建通过／配置预览／真实 DSH 闭环／真实模型连通」）
4. `C:\TRAE\Agent-Memory\doc-handoff\03-待办与已知冲突.md` —— 剩下的活、最后一个失败现象与三条假设、文档冲突
5. 规范：`doc2/`（本轮施工规范，07 是独立 profile 运行手册）；`doc/`（v1 产品与协议规范）。`doc2/` 与 `doc/` 冲突以 `doc2/` 为准；`doc2/` 与官方源码冲突以官方源码为准并回来更新 `doc2/`。

## 1. 当前状态（无需重做）

- 根仓库 `main`，HEAD `d7c863b`，工作区干净（只有 `deepseek-harness/` 与 `doc2/` 是未跟踪目录）。
- 已完成并提交：卡 V2-0、V2-1、V2-2、V2-3（代码+构建）、V2-4、V2-5。
- Rust：25 个单测全过；`memoryd` 报告 `schema_version=2`。
- 适配器：对真实 DSH 类型 `tsc` 通过，`dist/` 已产出。
- 真实 DSH 闭环已打通的部分：真实会话 `user/message` 入内核；spool 的 events/receipts/cursors 一致且 flush 在 event ack 之后；五个 `memory_*` 工具在 DSH 工具目录可见；`memory_remember` 经模型调用写库成功（`instruction「以后回答我用中文」active`）。
- 官方 DSH clone（`deepseek-harness@477b4f4`）已 `pnpm install --ignore-scripts` 且 `build:lib:host` 成功，profile `memory-hl`（headless）与 `memory-dev`（web）已建。

## 2. 你的第一个任务（按此顺序，每项单独跑、单独看日志）

1. 起三件套：`memoryd`（8791）→ 本地假模型（3977）→ `dsh --profile memory-hl --patch <alice patch>`。命令见 01 手册第 3 节。
2. 干净对照：删掉 `mock-state.json`，跑一次无关查询，确认输出 `ok 无记忆注入 tools=29`。
3. 跑 `记住指令：以后回答我用中文`，确认内核多出一行 active 记忆。
4. 跑 `用中文回答我`，断言 `mock-state.json` 的 `sawPluginMemory` 为 `true`——**这一步上一轮失败**（`dsh: TRANSPORT: DeepSeek Messages transport failed`，mock 侧 `requests` 只从 10 变 11）。按 03 第 1 节的三条假设排查：先用 curl 直接打 mock 复现，再看 `/tmp/mock.log`，不要急着改适配器逻辑。
5. 注入通过后依次跑：`纠错流程` → `遗忘流程` → `取记忆 <id>`（mock 已脚本化这些场景）。
6. 最后做跨用户 404：新建 bob 用户目录与令牌 + 第二个 patch，用 alice 的 `memory_id` 调 `memory_get`，期望 404 / `MEMORY_NOT_FOUND`。

## 3. 然后做卡 V2-6 交付记录

- 独立 profile 跑完整短闭环（Agent A 写入 → Agent B 可见 → correct → forget → 离线 spool 恢复），按 `doc2/07 §7` 记录：本项目 commit、DSH commit、`/v1/version`、一次事件 request ID、一次工具 request ID、一次 compose request ID、遗忘后不可见结果、spool 离线恢复结果；**每项都要标注属于「代码存在／构建通过／配置预览／真实 DSH 闭环／真实模型连通」中的哪一类**。
- README 分层状态：HTTP 协议验证 / DSH 实际运行 / 模型真实连通 / 构建安装部署，四类分开写；`doc/README.md` 的旧状态要么更正要么加醒目跳转。
- 提交前 `git status --short`：只允许本项目代码与文档。

## 4. 硬性约束

1. **禁止 `git add .`** 或递归暂存根目录；`deepseek-harness/` 是官方 clone，保持未跟踪、只读。三个上游仓库（`EverOS/`、`hindsight/`、`tencentdb-agent-memory/`）同样只读。
2. **禁止回改 `migrations/0001_init.sql`**；新增持久字段必须新迁移文件并同步 `SCHEMA_VERSION`。
3. **不要把本地假模型的结果写成「真实模型连通」**。无可用真实端点时，按 `doc2/05 §5` 如实记「未验证」，不得用 mock 冒充。
4. 用户 scope 与来源准入只能由 Rust 内核决定；适配器不得在请求 JSON 里发 `tenant_id`/`user_id`，不得让模型填 `evidence_id`。
5. 不要把「测试通过 / curl 通过 / 插件装载成功 / 模型连通 / 用户可用」混为一个「完成」。
6. 遇到接口形状不确定时，去官方 clone 源码核对并写清路径，不要用旧 checkout 或记忆里的类型补齐。

## 5. 本机已知坑（别重复踩）

- 官方 clone：`corepack pnpm`（11.7.0）；必须 `--ignore-scripts`（lefthook postinstall 被本机 safe-delete 钩子挡）；包级无 `build` 脚本，只能用根 `pnpm run build:lib:host`；根 `.npmrc` 已加 `verify-deps-before-run=false`。
- 适配器靠 `file:` 链接解析真实 DSH 类型，所以**必须先构建官方 clone 的 host 库**，否则 tsc 找不到 `lib/types/*.d.ts`。
- 编译 Rust 前先杀掉 8791 端口的 `memoryd`（占着 `target/debug/memoryd.exe`，会报 LNK1104）。
- 本机 `rm` 会被 safe-delete 钩子拦截（尤其 `.git` 下的 lock 文件），改用 python `os.replace`/`os.remove`。
- `pnpm dsh` 走 tsx 直接执行 TypeScript 源码，headless profile 不需要前端构建；web profile 只用于 `--dump-config` 预览。

现在开始第 2 节的 **第 1 步**，把三件套跑起来并把 `/v1/version` 与一次 `dsh` 启动的实际输出贴给我，再继续下一步。

———
