# 11｜Prompt 注入方案决策：融合方案的角色分层与 DSH hook 映射

状态：**用户已确认的设计决策，尚未实现**。日期：2026-09-26。

## 1. 决策范围

用户选择将 TencentDB 的注入角色分层用于融合方案。该决定只控制“人格/记忆/派生知识在模型调用前怎样进入上下文”；整体知识结构以 [15](15-人格与派生知识融合方案决策.md) 为准：Hindsight 风格的证据约束画像与主题页作为派生数据，EverOS 风格 Markdown 作为可读/交换体验。Rust 继续负责 scope、证据、记忆状态和服务端裁决。

## 2. 采用的注入规则

借鉴 TencentDB `MemoryCore/src/core/hooks/auto-recall.ts` 的角色和时机划分：宿主构建模型 Prompt 前执行；变化的记忆/知识通过 `prependContext` 进入 user prompt；本项目只有**用户编辑的 Soul**可进入 system prompt。由 LLM 从 L1 派生的问题画像和主题页是数据，不是 TencentDB 所谓稳定 persona；不可自动生成 system Persona。Resident 是本项目查询无关的常驻能力，不是 TencentDB `prependContext` 的逐字照搬；Resident 选择规则仍由 doc6/03 决定。本项目映射为：

| 内容 | 作用域/变化频率 | 本项目的模型角色 | DSH 接线 |
|---|---|---|---|
| Soul | `(tenant,user,agent_id)`；用户编辑，稳定 | system prompt 中独立的 `agent-memory:soul` section | `system-prompt/assemble` |
| Resident | 同用户跨 Agent 共享；不依赖当前 query，每 step 重算 | 带来源边界的 user-role runtime context | `agent/pre-step` 调用 bundle |
| Derived knowledge | 同用户跨 Agent 共享；问题画像/主题页，随来源版本更新 | 不增加新的信任角色；作为带 `document_kind`、文档 ID 和来源边界的 user-role item | 由 bundle 放入 Resident 或 Retrieved 对应段；只在相关召回或用户 pin 时出现 |
| Retrieved | 同用户跨 Agent 共享；随本轮 query 变化，可为空；可含 L1 与派生知识 | 带来源边界的 user-role runtime context | 与 Resident 同一 bundle 请求、分段注入 |

Resident 与 Retrieved 在当前用户正文之前提供，语义上对应 TencentDB 的 user-prompt `prependContext`；派生文档只作为这两段中的数据 item，不另建一个可被误认为指令的新角色。DSH 实现必须使用独立的插件来源消息或宿主认可的 user-role runtime context；不得将内容拼进原始用户消息，不得把消息标记成 `source.kind='user'`。各段保持来源类别与 item IDs，便于预算、诊断、去重与撤销核对。

## 3. DSH 与 TencentDB 的接口映射

TencentDB 的 `before_prompt_build` 是其宿主 hook 名，不是 DeepSeek Harness 的 API。当前官方 DSH 源码呈现的职责映射为：

1. 官方 `deepseek-harness/packages/core/agent-loop/src/agent.ts` 的 `preStep` 先调用 `systemPrompt.assemble()`，再执行 `agent/pre-step`。因此 Soul 必须在 Prompt assembly 阶段贡献 system section；不能等到 pre-step 后再补 system 指令。
2. 官方 `deepseek-harness/packages/core/system-prompt/README.md` 与 `packages/core/system-prompt/src/index.ts` 定义 section 与 runtime context：前者渲染到 system prompt，后者以带来源的 user-role context 参与模型输入。`deepseek-harness/packages/core/agent/README.md` 说明 `agent/pre-step` 可替换进入该 step 的消息列表；D6-0 仍需核对当前类型与持久化边界，再确定如何在不改写原始输入的前提下安排顺序。只有 Soul 用 section；Resident/Retrieved（可含派生文档）用 user-role context。
3. DSH `agent/pre-step` 用于在每个模型 step 前请求 `/v1/context/bundle`，即使 query 为空也取 Resident。只从最近真实用户消息取 Retrieved query；工具续步不能把工具文本或上轮问题冒充成本轮用户 query。
4. DSH 消息来源字段只用于宿主内部来源/事件语义；DeepSeek provider 的 `deepseek-harness/packages/llm/llm-deepseek/src/serialize.ts::serialize` 构造 wire message 时传 role/content，不把 `source` 发送给模型。验收必须分别核实宿主侧来源元数据和模型线路上的角色、顺序、可见边界文本；不能把内部 source marker 声称为模型可见安全边界。

如果实际 checkout 的 DSH API 无法把独立 sourced user-role context 排在原始用户正文之前，D6-0 记录源码、类型和最小复现，并暂停 D6-4 注入实现；先修订本决策文档。不得为符合排序要求把记忆提升到 system role，也不得偷偷合并进用户正文。

## 4. 不随本决策引入的上游机制

- 不采用 TencentDB 的整套 L0-L3、MemoryProxy、场景导航或存储结构；不把任何模型生成 Persona 当作用户授权的 Soul。问题画像/主题页按 [15](15-人格与派生知识融合方案决策.md) 作为独立、可撤销的 user-role 派生知识处理。
- 不采用其 FTS/向量内部混合排序取代本项目既定的 Hindsight 主体召回与单次全局 RRF。
- 不把所有记忆都当 system 指令。只有用户编辑的 Soul 进入 system section；Resident、Derived knowledge、Retrieved 都是有来源的数据上下文。
- 不改变 Resident 选择、Retrieved 召回、语义准入、Dream 写入触发、scope 或 forget 语义；这些分别由 doc6/03、04、09、10 和现有 Rust 契约决定。
- 不宣称注入等于记忆被模型采用。日志可以证明某个 ID 与文本上了线路；模型是否正确利用要用有/无记忆可区分的验收样本判断。

## 5. 信任与失败边界

- 注入前仍由 Rust bundle 按已认证 scope、active/有效状态、pin、来源版本和预算裁决。DSH 适配器不自行挑记忆或放宽 scope。
- Resident/Derived knowledge/Retrieved 的正文必须作为数据明确包裹、转义 XML/Markdown 特殊字符。正文即使包含“忽略系统指令”等文本，也不能跳出数据边界或变成 Soul。
- 宿主内部保留 `agent-memory` 来源，用于防止注入消息重新作为用户证据进入 L0；模型线路的 user role 本身不证明内容由用户新说出。
- 服务失败、超时或空结果时，不复用未获服务端版本确认的旧 bundle；跳过记忆上下文并记录非正文诊断。Soul 改版/删除必须在后续 step 生效。
- 无 Soul/Resident/Derived knowledge/Retrieved 内容时不发送空占位消息；不得为填上下文预算而注入无关的最新记忆或全部画像。

## 6. D6-4 验收要求

使用官方 DSH 进程和 mock 端点验证以下协议行为；这些结果只证明宿主接线，不证明真实模型利用质量：

1. outbound system prompt 中出现当前 Agent 的 Soul，其他 Agent/user scope 的 Soul 不出现；空 Soul 不产生空 section。
2. 生成的问题画像/主题页从不进入 system section；相关时作为有来源 user-role data 出现，未召回且未 pin 的画像不注入。
3. query 为空的工具续步仍可获得 Resident；query 有值时 Retrieved 与本轮真实用户消息匹配，且已在 Resident 的 ID 不重复注入。
4. Resident/Derived knowledge/Retrieved 在原始用户正文之前作为 user-role context 出现；原始用户事件的文本和 source 未被改写。模型线路上的 role、先后顺序和显式上下文标签可从 mock 请求核对。
5. 宿主内部 source marker 能区分插件注入与用户证据；重复 hook 不产生重复段；注入消息不进入新的 L0 用户 evidence。
6. forget/correct、过期、scope 切换、超时、内核离线、空 bundle 后，旧内容不残留；跨用户内容不出现在请求中。
7. 包含 XML 标签或指令样式文本的记忆/派生文档仍处于 user-role 数据区，不改变 system section。

另做模型利用率/正确沉默评估时，按 doc6/07 的有记忆/无记忆可区分探针报告；仅 `saw_injection=true` 不等于回答采用了记忆。

## 7. 参考源码

- TencentDB：`tencentdb-agent-memory/MemoryCore/src/core/hooks/auto-recall.ts`，`RecallResult.prependContext`、`appendSystemContext` 及其动态 L1/稳定 persona 分流。
- DSH 时序：`deepseek-harness/packages/core/agent-loop/src/agent.ts` 的 `preStep`；`deepseek-harness/packages/core/agent/src/dispatch.ts` 的 `assembleContextFor`。
- DSH system/context 语义：`deepseek-harness/packages/core/system-prompt/README.md`、`deepseek-harness/packages/core/system-prompt/src/index.ts`。
- DSH 消息列表改写边界：`deepseek-harness/packages/core/agent/README.md` 的 `agent/pre-step` 说明；wire source 边界：`deepseek-harness/packages/llm/llm-deepseek/src/serialize.ts`。
- 当前 Agent-Memory 旧注入：`agent-memory/adapters/dsh/src/recall.ts`；它用于描述现状，不代表新 bundle/Soul 已实现。
