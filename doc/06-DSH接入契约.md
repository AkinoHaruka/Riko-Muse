# 06 DSH 接入范围与接口事实

DSH 适配器的最终施工步骤见 [14-DSH适配器实施](14-DSH适配器实施.md)，精确的请求／响应见 [12-HTTP协议v1](12-HTTP协议v1.md)。本文件只记录已核对的宿主事实与由此形成的接入边界，避免把旧插件的作用域当成新项目的身份来源。

## 已核对的本地源码

在 `C:\TRAE\Riko-dsh\deepseek-harness\packages\bundle\riko-memory\src\index.ts` 中，当前可见：

- `ctx.on('session/event', ...)` 取得 session 与 event；参考代码按 session 顺序写 L0。
- `ctx.on('agent/pre-step', ...)` 在 `await next()` 后，对 `decision.kind === 'enter'` 的回合附加召回消息。
- `ctx.on('agent/created', ...)` 注册 prompt context。
- `ctx.tools.register(defineTool(...))` 注册显式记忆工具。
- 旧插件用 `ownerNamespace + stableAgentPresetId` 分域。这**不能**实现本项目同用户跨 Agent 默认共享。

该 DSH checkout 存在用户未提交改动，只作只读参考。目标 DSH 版本若更新，实施者须核对 Hook 类型和插件装载方式，并在 [14](14-DSH适配器实施.md) 记录实际源码路径。

## 新适配器身份规则

每个 DSH 适配器实例配置一份当前用户的令牌文件。Rust 服务由 Bearer token 查 `principals`，从服务端得到 `(tenant_id,user_id)`；HTTP 正文不含 scope。一个用户的不同 Agent 使用相同令牌并共享记忆；另一个用户使用不同令牌。

当前没有看到足以证明 DSH Hook 提供多用户认证 ID 的接口。因此 v1 支持“一个 DSH 进程服务一名配置用户”；可以有多个这样的进程连接同一内核。若一个 DSH 进程同时服务多人，必须等可信用户身份接口明确之后才开放，不能用 Agent preset、session、目录、Prompt 或模型输出充当用户身份。

## 接入纪律

1. 新适配器位于新项目 `adapters/dsh/`，使用独立 DSH profile 试装。
2. 旧记忆插件不和新适配器在同一 profile 同时注入。
3. `session/event` 的 plugin 来源消息不做用户证据；新注入消息也标记 plugin 来源。
4. `turn/end` flush 以前面已被内核确认的最后一条正文证据 seq 为边界。
5. 工具写操作必须携带最新真实用户消息的 evidence ID，Rust 服务二次检查。
6. 内核短暂不可用时，适配器按 [14](14-DSH适配器实施.md) 的本地 spool 规则恢复事件，不让对话整体因记忆故障中断。
