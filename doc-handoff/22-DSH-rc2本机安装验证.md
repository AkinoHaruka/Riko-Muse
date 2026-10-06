# DSH 0.2.0-rc.2 本机安装验证

日期：2026-09-29（本机，Asia/Singapore）

## 结论

已按用户要求将原官方 DSH clone 整体保留到临时备份路径，并在原工作区路径重新克隆官方仓库。新 clone 为 `0.2.0-rc.2`，仓库干净；依赖安装与 DSH host 库构建通过。

在独立 DSH home/profile 中安装了公开的 Riko Memory 与 Riko-App Bridge bundle。官方 DSH rc.2 Web 服务实际启动；Bridge health 返回 HTTP 200；创建并列出了一个 Riko preset session。该 session 创建流程会 mount Riko preset，preset 中包含 Memory adapter，因此证明了 rc.2 运行时可以载入这两个 bundle。

本轮没有向 Agent 发送 prompt。因此没有验证记忆事件是否进入 L0、召回内容是否注入 prompt、Memory adapter 是否能完成真实 memoryd 读写，或任何模型行为。未调用 Chat、Embedding 或其他模型 API；没有操作服务器。

## Clone 更换与构建

- 官方仓库：`https://github.com/deepseek-ai/deepseek-harness.git`
- 新 clone：`C:\TRAE\Agent-Memory\deepseek-harness`
- 新 clone HEAD：`639ed015397290b3745d163aafe02ffee4aa3f84`（`master`，package version `0.2.0-rc.2`）
- 旧 clone 原有大量 tracked deletions；未丢弃或清理，完整移到：`%LOCALAPPDATA%\Temp\deepseek-harness-prior-20260929`
- 新 clone `git status --short`：干净
- `corepack pnpm install --ignore-scripts`：通过
- `corepack pnpm run build:lib:host`：通过
- 另一个干净的 rc.2 临时 clone 执行全量 `corepack pnpm run build`：host/client/native 构建阶段完成，Web Vite/esbuild 清理临时目录时遇到 Windows `Access is denied`。全量构建未通过，Web UI 未验证；这不影响本轮通过已构建 host/client/native 工件启动服务的 smoke 结果。

## Bundle 与隔离环境

使用独立临时 DSH home/profile，没有复用用户的日常 profile：

- DSH profile：`riko-web-smoke`
- profile/home：`%LOCALAPPDATA%\Temp\dsh-riko-rc2-profile-smoke-20260929`
- Memory bundle：`@agent-memory/dsh-adapter@0.5.2`，bundle patch `riko-preset.patch.yml`
- Bridge bundle：`@riko/riko-app-api@0.1.1`，bundle patch `riko-app-bridge.patch.yml`
- Profile 配置 dump 显示 `preset-riko` 和 `riko-app-api` 两个 bundle 项。

本轮使用的公开源码引用：

- Riko-Memory `main`：`74710df5382eb00cddd011fcd1d12a3f595ad3aa`
- Riko-App-Bridge `main`：`eb621f930e6e08240e2d5d657ea76f615caa0f66`

## 运行时实测

以 loopback 地址启动 rc.2 Web 服务，配合一个全新合成 schema 13 memoryd 数据库；Auto Dream 关闭，没有配置模型或 embedding provider。

| 检查 | 结果 | 证明范围 |
|---|---|---|
| `node apps/cli/lib/bin.js --version` | `0.2.0-rc.2` | 实际使用的 CLI 版本 |
| `--profile riko-web-smoke --dump-config` | 同时列出 `preset-riko` 与 `riko-app-api` | bundle 进入组合配置 |
| DSH Web 服务启动并保持运行 | 通过 | rc.2 Web/API 服务可启动 |
| `GET /riko-app-api/v1/health`（带临时 Bridge 凭据） | HTTP 200，`ok=true` | Bridge 路由已注册、鉴权配置可用 |
| `POST /riko-app-api/v1/sessions` | 成功，返回 `agentPreset=riko` | Riko preset session 可以创建 |
| `GET /riko-app-api/v1/sessions` | 新 session 可见 | Bridge session 列表可读 |

### 版本元数据差异

Bridge health JSON 当前仍报告 `dshVersion: 0.2.0-rc.1`，而正在运行的 DSH CLI 是 `0.2.0-rc.2`。这是 Bridge 返回的版本元数据落后一版；本轮 HTTP 路由和 session API 正常。尚未修改 Bridge 代码或发布新 bundle。

## 验证边界

### 已验证

- 官方 DSH 已在原目录重克隆到 rc.2；新 clone 干净。
- 依赖安装、`build:lib:host` 通过。
- 两个公开 bundle 可安装到隔离 profile 并出现在组合配置。
- rc.2 Web 服务启动、Bridge health、Riko session create/list 实际通过。
- 使用全新合成 schema 13 数据库；未打开 dana/realtest。

### 未验证

- 全量 DSH Web UI build（遇 Windows Vite/esbuild 临时目录 `Access is denied`）。
- Agent prompt 处理、Memory adapter 的事件捕获和 spool 写入。
- 后续 turn 的记忆召回、注入与模型利用。
- Dream 子 Agent、Chat/Embedding API、语义质量、性能。
- 服务器安装/升级和生产可用性。

## 清理与保留

- memoryd、DSH Web 进程均已停止；本轮端口已确认释放。
- 临时 Bridge/memory 凭据文件和可能包含临时认证信息的 stdout/stderr 日志已清理。
- 保留隔离 profile、合成数据库和临时 clone，便于复现；旧 clone 备份保留完整。
- 未更改或暂存 `deepseek-harness/` 的源码；该目录在根仓库仍是未跟踪参考目录。
- 未运行任何模型 API 请求；未进行服务器操作。
