# Android Bridge 生产部署与连通验证

日期：2026-09-29（Asia/Singapore）

## 结论

已把 Android 设置页依赖的 Riko-App Bridge 新路由部署到生产 DSH，并用已安装的 Android debug APK 完成认证连接与模型设置读取。原先设置页报 `GET model-settings: HTTP 404`，原因是生产 Bridge 版本过旧；更新 bundle 后又因 profile 缺少两个启用条件而未注册路由。补齐 profile 配置并重启 DSH 后，Android 显示「已连接到 DSH · Riko preset」，在线状态正常，provider/model settings 可读取。

这是 **Bridge 已部署 + Android 到 Bridge 的认证读取已验证**。没有向模型发送对话，也没有通过 App 提交/修改模型提供商凭据；不能据此声称模型 API 已连通。

## 影响范围与保护

- 目标：生产 Docker 容器 `riko-dsh-runtime` 内 DSH 的 `web` profile，以及 `https://riko.asia/riko-app-api/v1` 对应的 Bridge 路由。
- 部署 Bridge：`@riko/riko-app-api@0.1.1`，来源为公开 Riko-App-Bridge bundle。
- DSH runtime image 仍为 `riko-dsh-runtime:0.2.0-rc.1`；本次只更新 profile 内的 bundle，不升级 DSH 镜像。
- 变更前在服务器创建 profile 备份：`/opt/riko-stack/backups/riko-app-api-update-20260929T114823Z`。备份包含 profile `package.json`、`pnpm-lock.yaml`、`cordis.patch.yml` 和旧 `riko-app-api` 目录。
- 没有读取、复制或记录 token/key 内容；只确认已有 token 文件可读、路径存在。没有打开或升级 dana/realtest 数据库。
- 本机只安装更新后的 debug APK 到 `emulator-5554`；未发布 APK，也未改动 Riko-App Git 状态（Riko-App 根目录不是 Git 仓库）。

## 根因与修复

1. 更新前 App health 可达，但 Android 设置页的 `GET model-settings` 返回 `HTTP 404: Route not found`。这与生产 profile 中旧 `@riko/riko-app-api@0.1.0` 相符：旧版没有 Android 模型设置路由。
2. 将 profile bundle 更新至 `@riko/riko-app-api@0.1.1` 后，发现新版 Bridge 的 patch 在未设置 `RIKO_APP_API_TOKEN_FILE` 和 `RIKO_APP_SESSION_REGISTRY_FILE` 环境变量时会禁用插件。生产容器没有设置这两个变量，所以升级后公开 health 仍为 404。
3. 沿用服务器已有文件，不改 token 内容；在 `web` profile 的 `cordis.patch.yml` 中为 Bridge 显式配置：
   - `apiTokenFile: /root/.dsh/riko-app-api/api-token`
   - `sessionRegistryFile: /root/.dsh/riko-app-api/sessions.json`
4. DSH `--profile web --dump-config` 显示 Bridge patch 已启用并加载上述配置。随后重启 `riko-dsh-runtime`，容器状态为 running。

Android 侧在 `riko-compose/app/src/main/java/com/riko/app/core/DshRepository.kt` 为普通请求和 SSE 错误补上操作/路由上下文，例如显示 `GET model-settings`，不只显示裸 HTTP 状态。这样已实际定位到旧 Bridge 返回的 404。该改动随当前 debug APK 构建并安装；没有改变 token 存储方式或网络信任策略。

## 验证证据

| 验证 | 结果 | 证明范围 |
|---|---|---|
| `riko-app-api` package | 生产 profile 安装 `@riko/riko-app-api@0.1.1` | 新版 Bridge bundle 已写入 DSH profile |
| DSH `--profile web --dump-config` | Bridge 项启用，配置指向已有 token/registry 文件 | profile patch 已生效 |
| 未认证 GET `https://riko.asia/riko-app-api/v1/health` | JSON `401 UNAUTHORIZED` | 反代与 Bridge 路由已存在；未认证被正确拒绝，非 404 |
| Android `:app:assembleDebug --offline` | 成功 | 当前 Riko-App 源码可构建 debug APK |
| `adb install -r` 到 `emulator-5554` | 成功 | 模拟器安装并启动当前 debug APK |
| App「保存并测试连接」 | 显示「已连接到 DSH · Riko preset」及在线状态 | App 中已保存的 Bridge token 可经 Rust JNI HTTPS 鉴权并连接 preset |
| App provider/model settings | 能读取 DeepSeek、DeepSeek Account 等 provider 状态及模型设置 | `/model-settings` 认证读取、Bridge 到 DSH settings/credential 状态查询可用；不回读密钥 |
| Bridge 本机测试 | `npm test`：构建成功，1 项测试通过 | 当前 Bridge 包的确定性测试通过，不代替生产写入验收 |

## 未验证边界

- 未在 Android 界面中提交新的 provider API key，也未执行密钥移除。
- 未在 Android 界面保存/编辑自定义模型 API provider；当前只确认线上 provider/model settings 列表可读。
- 未验证在线模型发现操作，也未通过 App 发送 chat 请求。
- 没有调用 DeepSeek、Gemini、OpenRouter 或其他模型 API；没有模型 key 被输入或传输。
- 本次没有把 DSH runtime 升级到 rc.2；当前生产容器仍是 rc.1。
- 公开 health 的未认证 401 是预期鉴权结果；实际 App 认证读取成功。它不是匿名访问放行。

## 回滚

回滚材料位于服务器目录 `/opt/riko-stack/backups/riko-app-api-update-20260929T114823Z`。如需回滚，应先停止对该 profile 的并发编辑，再恢复其中的 profile manifest、lockfile、patch 和旧 bundle 目录，随后重启 `riko-dsh-runtime` 并用 Android 连接页与健康路由复验。不要删除该备份。

## 后续状态

Android 已能连接生产 Riko preset 并读取模型配置，是继续验证设置页操作链的基础。后续应单独完成：Android provider key 写入/删除、custom provider 保存/更新/删除、模型发现，以及在明确指定模型后发送一条低成本测试对话。每项都应分开记录 Bridge 配置持久化、DSH provider 状态与真实模型 HTTP 调用结果。
