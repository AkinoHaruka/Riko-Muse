# 14 DSH 适配器实施手册

本文件是 DSH 侧实现的施工说明。它不是对当前 DSH 类型的编译保证：开始动代码时，先在目标 checkout 核对接口导出及插件装载配置；只为类型与生命周期差异调整胶水代码，不改变 [10-开发冻结规范](10-开发冻结规范.md) 的用户作用域。参考代码是 `C:\TRAE\Riko-dsh\deepseek-harness\packages\bundle\riko-memory\src\index.ts`：当前看到 `session/event`、`agent/pre-step`、`agent/created`、`ctx.tools.register`、`Service.init`、`ctx.effect` 的实际使用；该旧插件有未提交工作，不应在这里修改它。

## 1. 新包与配置

新包放在新项目的 `adapters/dsh/`，包名暂定 `@agent-memory/dsh-adapter`。它可以以 DSH 外部插件／Bundle layer 装入独立 profile；具体 manifest 语法从目标 DSH 已有插件复制最小可用模板。不要把新代码写进旧 `riko-memory` 目录。试运行只启用新适配器一个记忆插件，以免两套记忆同时注入。

固定配置：

```ts
interface Config {
  memoryUrl: string;        // 默认 http://127.0.0.1:8791
  userTokenFile: string;    // memoryd principal add 生成，绝不直接写 token
  spoolDir: string;         // DSH 本机持久目录，不能位于参考仓库源码里
  requestTimeoutMs: number; // 默认 500ms for compose, 3000ms for writes
  captureEnabled: boolean;  // 默认 true
  injectionEnabled: boolean;// 默认 true
}
```

启动时读取令牌文件、限制内存中令牌曝光；向 `/v1/version` 核对协议版本，再读取 health。token file 不存在、URL 非 loopback、协议不兼容或 spoolDir 不可写时，插件启动失败并报明确配置错误。默认不允许远端 memoryUrl。

## 2. 事件线

`ctx.on('session/event', (session,event) => ...)` 中只提取允许的事件类型：`user/message`、`assistant/message`、`tool/result`、`turn/end`。对用户／助手／工具消息，按 DSH 真实结构提取文本；纯图片、非文本片段在 v1 标成 unsupported 并记录非敏感错误，不拼出想象文字。`event.seq`、`event.time`、`session.id` 必须从宿主对象取；找不到则不发送，并记录 `DSH_EVENT_SHAPE_UNSUPPORTED`，不能自增造序号。

`user/message` 若 `event.data.source.kind === 'plugin'`，不作为用户证据发送。由本适配器注入的召回消息也必须带 `{kind:'plugin',plugin:'agent-memory'}`；不能让它在下一轮被当成新用户陈述。保留 `origin.agent_id`：若事件回调没有 Agent 实例，需查 DSH 对 session 的当前 Agent 映射；若仍找不到，用稳定 `unknown-agent` 只作为来源标记，**不影响用户 scope**。`user_id` 从令牌间接决定，插件不填写。

旧插件当前在 `index.ts` 的 `latestUserText(session)` 中通过 `session.deriveMessages()` 从后向前寻找 `role=user` 且 `source.kind=user` 的消息；`userQueryFromMessages(messages)` 只汇总 `source.kind=user` 的文本块；`createRecallMessage` 用 `createUserMessage` 设置 `source.kind=plugin`。新适配器可以复用这三个**接口用法**，但不能复制旧插件的 `ownerNamespace + preset` 作用域逻辑。工具还需要把该用户消息对应回已提交的 `event.seq → evidence_id`，不能只靠文字相等猜 ID。

每个会话建立一个 Promise 顺序链：同一 session 的 `event_seq` 按序追加本地 spool、按序 POST。多个会话可并行。`turn/end` 在 spool 中追加一条 flush 操作，`through_event_seq` 取**此前已追加到 spool 的最后一条正文证据序号**，不取没有送入内核的 `turn/end` 序号。发送队列严格按 spool 顺序先确认前面的 event，再发送 flush；内核离线时二者都保留在 spool，重连后依次重放。重复事件和 flush 由内核唯一键去重。

## 3. 本地待发 spool

为避免内核短暂不可用导致 L0 消失，适配器在发送前把 event 或 flush envelope 追加到 `spoolDir/events.jsonl`，并使该追加落盘；`session/event` 回调在本机写入成功后才返回，网络发送异步进行。每条记录包含操作唯一键与请求正文，不含令牌。收到 event 的 200/201 或 flush 的 200/202 后，将操作 ID 写到 `spoolDir/acked.jsonl`。重启后按原顺序重放未 ack 操作，依赖内核幂等键消除重复。只在所有未 ack 操作仍可恢复时压缩 spool；不要边发边删除唯一副本。限制总量 100 MiB，超限记录可见错误并停止记忆捕获，不影响 DSH 正常对话；不能静默丢弃。spool 是敏感本地数据，目录权限限当前用户，备份和删除遵循会话数据策略。

内核离线时适配器继续让 DSH 对话运行，自动注入跳过并记录一次有节流的错误；写入走 spool 重试。`dispose` 时停止接新事件，等待当前会话顺序链最多 5 秒，留下未 ack 记录供下次启动重放。不能在 dispose 时删除 spool。`turn/end` 的 flush 也要进入同一 spool 流，且在先前事件 ack 后执行。

## 4. 自动上下文 Hook

基于已看到的 DSH 用法：

```ts
ctx.on('agent/pre-step', async ({agent,messages,signal}, next) => {
  const decision = await next();
  if (decision.kind !== 'enter' || signal.aborted) return decision;
  const query = latestOriginalUserText(messages);
  if (!query) return decision;
  const result = await composeWithTimeout(query, String(agent.id), signal);
  if (!result.text) return decision;
  const memoryMessage = makePluginSourceMessage(result.text);
  return {...decision, messages:[...decision.messages,memoryMessage]};
});
```

这段是结构示例，`makePluginSourceMessage` 必须用目标 DSH 版本的 `createUserMessage` 或等价 API，确保 `source.kind='plugin'`。`latestOriginalUserText` 只选最后一条真实用户消息，不选历史注入或助手消息。同一个 pre-step 如果重入，检查 `decision.messages` 中的稳定 `agent-memory` 标签并跳过重复追加。compose 超时或服务错误时返回原 decision，日志写 request ID 与错误码，不写记忆正文。

## 5. 工具注册

按目标 DSH 的 `defineTool` 形式注册 `memory_search`, `memory_get`, `memory_remember`, `memory_correct`, `memory_forget`。工具回调从 `exec.agent.session` 取得当前 session，再从 DSH session 事件流找到**最新真实用户消息**及其 `event_seq`；在 Rust 返回的 `evidence_id` 与本地事件映射中取得 `user_evidence_id`。找不到该证据时 remember/correct/forget 直接返回“当前用户消息尚未入库，请重试”，不能用助手参数补一个 ID。

工具参数不能包含 `tenant_id/user_id/token`。search/get 只传查询和 ID；写类工具还传当前 `origin`、最新 `user_evidence_id`、quote 和期望版本。forget 与 correct 的目标 ID 若缺失或含糊，先调用 search 返回候选供用户选择。Rust 端仍执行最终的原文子串和最新事件检查，不能仅靠 TypeScript 守门。

## 6. 交付时的宿主核查

开始实施前在目标 DSH checkout 中记录：

1. `session/event` 实际事件类型、`event.seq/time`、消息正文路径与 `source.kind`；保存文件路径和 commit。
2. `agent/pre-step` 的 `next()` 返回类型及消息 source 能否正确设置；核对同一回合执行次数。
3. 工具 `exec.agent` 是否存在、如何获得最新原始用户事件。
4. 外部插件包／Bundle layer 的安装方式；只在独立 profile 试装。
5. 当前 DSH 是否提供经过认证的多用户 ID。若没有，保持单用户 DSH 进程配置；不能悄悄映射 preset 为用户。

核查不需要调用模型。若某个 Hook 的必要字段不存在，写清缺项并只暂停该接线项，Rust 数据库和协议等独立任务可以继续。
