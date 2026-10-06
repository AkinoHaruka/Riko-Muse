# 07 独立 DSH profile 试装与状态记录

本手册是给卡 V2-6 的运行步骤，不是本次已执行记录。当前官方 clone `deepseek-harness/master@477b4f4` 尚未安装 `node_modules` 或构建 `lib`；当前适配器尚未导出可由 DSH profile 装载的完整 Cordis `apply`。执行前先完成卡 V2-0 至 V2-4，按命令实际结果调整记录，不能把本手册示例说成已运行。

## 1. 环境与目录

- 官方 DSH 要求 Node `^22.19.0 || >=24.0.0`，项目锁定 pnpm `11.7.0`；查看 [根 package.json](../deepseek-harness/package.json)。
- 新项目 Rust 命令在 `C:\TRAE\Agent-Memory\agent-memory` 内运行；DSH 命令在 `C:\TRAE\Agent-Memory\deepseek-harness` 内运行。
- 为测试用户建独立本机目录，例如 `%LOCALAPPDATA%\AgentMemory\v2\alice`，存放 DB、token、spool、非秘密 config 与 profile patch。不要把 token/DB/spool 放到 `doc2`、官方 DSH clone 或需要提交的源码目录。
- 当前根 `.gitignore` 排除了三个旧上游，尚未排除新 `deepseek-harness/`；提交前先检查 `git status --short`，只暂存明确要提交的本项目路径。

## 2. 先启动 Rust daemon

在 `agent-memory` 目录创建本机测试用户目录和 `config.toml`，配置只含非秘密项：`listen_addr=127.0.0.1:8791`、绝对 `db_path`、本项目 `migrations_dir`。用 `memoryd principal add` 创建测试用户令牌文件，随后用 `memoryd serve --config <本机配置>` 启动。先读取 `/v1/version` 与 `/v1/health`。这一步只证实内核启动；无需模型端点。完整 CLI 参数见 [项目 README](../agent-memory/README.md) 与 [实现附录](../doc/09-实现契约附录.md)；不要把令牌值贴在终端记录或文档里。

若此前测试已有同名 principal，`principal add` 会拒绝覆盖；使用新的隔离 DB 或经设计的 `rotate-token`，不要删除现有用户 DB 来省事。启用卡 V2-5 后，version 的 `schema_version` 应为 2，协议仍为 1。

## 3. 准备官方 DSH

官方 clone 当前无依赖与 build 产物。按其 [AGENTS.md](../deepseek-harness/AGENTS.md) 和 [CLI README](../deepseek-harness/apps/cli/README.md) 安装与构建必要 Host 包，然后从仓库根使用 `pnpm dsh` 入口。不要把 `apps/cli/src/bin.ts` 当独立产品启动器，也不要用旧 checkout 的 `riko-memory` Bundle 示例。

创建一个非默认、单用户的 profile，如 `memory-dev`，从官方 `web` 模板初始化。源代码支持的启动形式为：

```powershell
pnpm dsh --profile memory-dev --from-default-profile web --dump-config
```

该命令是依官方 CLI 参数和 profile 装载源码写的执行模板，本次未运行。若本机 profile 已存在，不要重复传 `--from-default-profile`；用 `--profile memory-dev --dump-config` 读取已有组合。`--dump-config` 是配置预览，不执行模型调用。

## 4. 本地 patch 模板

卡 V2-0 完成并生成实际 ESM 入口后，在用户本机目录保存一份 patch，示意如下。路径和配置名必须与最终插件入口实际导出核对，不能在现有 `createAdapter` 模块还没有 `apply` 时直接使用：

```yaml
- insert:
    - id: agent-memory
      name: 'C:/TRAE/Agent-Memory/agent-memory/adapters/dsh/dist/index.js'
      inject: [tools]
      config:
        memoryUrl: 'http://127.0.0.1:8791'
        userTokenFile: 'C:/Users/<USER>/AppData/Local/AgentMemory/v2/alice/alice.token'
        spoolDir: 'C:/Users/<USER>/AppData/Local/AgentMemory/v2/alice/spool'
        hostId: 'dsh-memory-dev-alice'
        composeTimeoutMs: 500
        writeTimeoutMs: 3000
        captureEnabled: true
        injectionEnabled: true
        toolsEnabled: true
```

`inject` 列表最终以插件实际使用的 DSH 服务为准；不能只照抄模板。官方 base bundle 已有 `session-query-sqlite` 行，预览时确认它在最终配置中仍存在，适配器的定点缺口对账才能使用 `ctx.sessionQuery`。profile patch 只写 token 文件路径，不写 token 值。将补丁放在用户目录便于单独删除/切换，不修改官方默认 profile。预览时运行 `pnpm dsh --profile memory-dev --patch <PATCH_PATH> --dump-config`，确认只有一个 `agent-memory` 行且未与其他记忆插件并列。

## 5. 启动与短闭环

运行 `pnpm dsh --profile memory-dev --patch <PATCH_PATH> --no-open --port <LOCAL_PORT>`，其中末尾 `--no-open/--port` 属于 Web 应用参数；实际运行以 `dsh --profile memory-dev --help` 与 Web 配置为准。只在本机回环地址访问。首轮重点观察：

1. 插件是否真正加载；五个工具注册且输出 schema 有效。
2. Agent A 的真实 `user/message` 是否落入 memoryd，并能用当前用户原文调用 remember。
3. 新 Agent B 是否使用同一 token 取得 search/compose，并在模型可见日志中有带 `source.kind=plugin` 的记忆块。
4. 对另一测试用户配置另一独立 profile/token，确认同一 memory ID 返回 404；不要在一个 DSH 进程内临时换 token 假装多用户隔离。
5. 用明确包含旧、新片段的用户消息做 correct；用明确包含遗忘动词与目标片段的消息做 forget；观察下一次上下文即时变化。
6. 暂停 daemon，输入一条用户消息，确认 spool 已写；恢复 daemon 后当前 DSH 进程完成重发，同键重复只返回既有 evidence ID，前序未 ack 时没有 flush 抢先完成。

这套短闭环只需一个可用的 DSH 模型配置和少量交互。若缺凭据，可完成装载、事件和工具的无模型检查，但不可宣称 Agent A→B 的真实宿主闭环或模型提取质量。

## 6. 故障定位顺序

| 观察到的现象 | 首先核对 |
|---|---|
| profile 找不到插件 | patch 是否真的插入、ESM 入口是否已构建、绝对路径是否正确、`apply` 是否导出 |
| 插件启动异常 | ESM `require` 是否已消除；配置/令牌文件/目录权限；DSH peer 版本兼容 |
| 没有 L0 | `event.time` 是否数字毫秒且已转换；`user/message` 正文路径；`source.kind` 是否为 user；spool 是否有 op |
| spool 有 op，内核无记录 | version 握手、401/400/409/5xx 分类、发送重试和 receipt 状态 |
| flush succeeded，事件缺失 | 检查前序 ack 栅栏；此问题应在卡 V2-1 阻断 |
| remember/correct/forget 失败 | 当前真实用户 `message.id/seq/evidence_id` 映射、必填 quote/版本、Rust 返回 `error.code` |
| Agent B 没有记忆 | 两 Agent 的用户令牌是否相同、记忆是否 active、`injectionEnabled`、pre-step 是否真实进入、compose 是否为空 |
| 真实模型作业失败 | 完整 endpoint URL、HTTPS/DNS 支持、密钥文件、作业 error_code；禁止打印密钥和原始对话 |

## 7. 交付记录

保存：本项目 commit、DSH commit、插件构建命令、profile/patch 路径、`memoryd /v1/version`、一次事件 request ID、一次工具 request ID、一次 compose request ID、一次遗忘后的不可见结果、spool 离线恢复结果。记录每项是“代码存在、编译通过、配置预览、插件加载、真实 DSH 闭环、真实模型连通、已安装/已部署”中的哪一种。没有执行的项写“未验证”，不把用户先前提供的 curl 汇报转换成这次的运行证据。
