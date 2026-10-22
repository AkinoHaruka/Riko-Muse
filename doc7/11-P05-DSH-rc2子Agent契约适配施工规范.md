# P0.5 · DSH 0.2.1-rc.2 子 Agent 契约适配施工规范

> 2026-10-22 UTC；依据 P0 [交接 34](../doc-handoff/34-P0真实闭环验证记录.md)。现场官方 clone：`/home/hatch/workspace/src/deepseek-harness`，HEAD `687f282e8625c6b8d11ff58211a90524be2e4317`。本卡无迁移，不改内核。路径/行号均按该 HEAD 核对。

## 1. 不变量与当前结论

Dream child 必须在 DSH 子 Agent 的独立 scope 中运行。普通全局工具全部不可见、不可执行；只允许当前 phase 通过 child scope 注册的 Dream read tools。模型提示词、独立 preset 或服务端拒绝写入不能单独充当工具隔离。限制必须在首个 prompt 前生效，并覆盖恢复后的执行。Rust 仍是来源和提交的唯一裁决者。

**阻断结论**：rc.2 `SubagentStartRequest` 已移除 `toolFilter`，`startActivation` 的 spec 也没有等价的工具限制字段。现有 `toolFilter: { allow: [] }` 不能映射为单次 `startActivation` 调用。遵照 P0.5 明示停止条件，本卡在规范阶段暂停；不得仅删去 `toolFilter` 以求编译通过，也不得启动没有经验证隔离的真实 Dream child。

## 2. 新旧契约映射与源码证据

| 旧调用（本仓库） | rc.2 对应/差异 | 现场源码 |
|---|---|---|
| `ctx.subagents.start("spawn", request)` | `ctx.subagents.startActivation({ provider: "spawn", label, request, signal, delivery: "caller" })` 是候选形式；`delivery: caller` 只把结果交给 runner，避免向父模型发送完成通知。实际调用须在隔离方案确立后实施。 | `riko-dsh/src/dream-runner.ts:326-348`；上游 `packages/subagent/subagent/src/types.ts:30-59`、`src/index.ts:250-284` |
| `request.label`、`request.signal` | 升到 activation spec 顶层。request 类型变为 `Omit<SubagentStartRequest, 'label' | 'signal'>`。 | 上游 `types.ts:31-44,136-182` |
| `request.prompt/parent/maxDepth/outputSchema` | 仍在 `spec.request`；`spawn` 需支持相应 capability，服务端会核对。 | 上游 `types.ts:136-182`、`src/index.ts:269-283,630-649` |
| `request.toolFilter={allow:[]}` | **无映射**。新 request 不含此字段；activation spec 也不含。 | 上游 `types.ts:30-59,136-183`；本仓库 `dream-runner.ts:332` |
| 旧 run 的 `id/result/dispose` | 新 activation 为 `childId/messageId?/result/dispose`；`result` 含 `structured/stopReason`。不能继续读 `child.id`。 | 上游 `types.ts:46-59,249-277`；本仓库 `dream-runner.ts:346-372` |

全仓适配器源码搜索 `startActivation|toolFilter|subagents.start` 的结果：实际旧调用只在 `dream-runner.ts:332,346-348`；`index.ts:138` 有依赖旧限制的注释。`agent/created` 中 Dream scoped read tools 的注册位于 `index.ts:136-155`，需随隔离方案复核。锁文件/依赖声明仍为旧基线，此阻断阶段不切换。

上游 local activation 的启动在 `manager.ts:379-471`：先保留 parent ownership 和 child ID，经 `materialize` 创建 child，再接受首条 inbox 消息；返回包含 `childId/messageId`。`result` 在执行收尾后结算，`dispose()` 处理这一 activation；`delivery: caller` 不向父模型通知（上游 `README.md:59-73,117-130`）。因此未来 runner 必须继续在整个 `await activation.result` 期间续租，租约丢失时中止并清理 activation，提交前确认 generation/lease；`finally` 必须 dispose，不能把“已接受首条消息”误判为 job 完成。现有 `dream-runner.ts:290-374` 的 15 秒续租、失败上报、清理路径应保留语义。local child 可续接和冷恢复，但其 structured schema 不作为恢复配置持久保存（上游 `README.md:111-130`）；Dream job 不应依赖冷恢复来补交本次结构化结果。

## 3. 工具隔离缺口与可选路径

上游 `packages/core/tools/src/index.ts:697-705,1098-1131` 的 `agent.ctx.tools.restrict({allow:[]})` 能隐藏该 scope 继承的全局工具，且保留 scope 内注册工具。但它是 **agent 创建层** 的操作，不是 `startActivation` 请求参数。`agent/created` 在 child 发布后触发；官方创建流程说明 `setup` 先于 `agent/created` 和首条 prompt（上游 `packages/core/agent/src/index.ts:100-111,539-556`）。`manager.ts:880-922` 由 DSH 自身持有 local child 的 setup，仅从 `request.agentPreset` 取装配参数；新建 descriptor 在 `manager.ts:394-401` 未快照 `toolFilter`。历史 descriptor 可含 `toolFilter`，但冷恢复分支在 `manager.ts:1348-1376` 只能重放旧描述符中的限制，新请求不会写入这个字段。

可选设计入口，**均未获本卡实施结论**：

1. 等上游恢复受管理的、持久化的启动时工具限制参数；保留 `allow:[]` 的单次启动和恢复语义。
2. 在本项目接入有明确 child 身份绑定的创建时隔离层，调用 `agent.ctx.tools.restrict({allow:[]})`，并证明初次创建、冷恢复、并发 child、插件重载、失败回滚、首 prompt 前工具 schema/执行路径均受限；phase read tools 仍只在 child scope 注册。需先更新本规范与 D6/13 的 seam 证据。
3. 考察独立 child preset 或其他受支持 seam，并逐项证明它对**所有全局工具**也有清空语义；`agentPreset` 只选择插件组合（上游 `child-agent.ts:186-217`、`README.md:63-65`），不能未经证明替代 `allow:[]`。

不得在此卡通过类型断言、跳过 TS 检查、沿用旧 dist、仅靠模型提示词或 Rust 写接口拒绝来绕过阻断。

## 4. 解阻后的验收门槛

先更新此规范并实施经证实的隔离方案，然后分开提交代码与交付记录。基线切到 rc.2 时，`package.json` 的 DSH/Cordis peer 与本地 `file:` 链接、lockfile 和 preset 文件头必须同一基线。`tsc --noEmit`、构建、`npm test` 在 rc.2 下通过；契约测试要覆盖 child 实际可见/可执行工具、首 prompt、并发与恢复，以及失败时 claim/lease/dispose。

真实验收只在临时新库运行：官方 DSH riko preset 与真实模型多轮事实、偏好、指令、纠正；核候选、active、supersede、四段注入、system Soul、真实 Dream child spawn 及工具隔离；新会话问仅靠记忆能回答的问题。端点凭据只从用户指定的本机配置读取，不进仓库；按代码、构建、适配器测试、真实 DSH 闭环、经 DSH 的真实模型、部署分档记录。未达项写具体根因。

## 5. P0.6 路径 2 实施决定（2026-10-22）

用户已选择 §3 路径 2。Dream child 在 `agent/created` 事件中，先在 `agent.ctx.tools` 调用 `restrict({allow:[]})` 清空继承的全局工具，再在同一 child scope 注册当前 phase 的 Dream read tools。只对带有效 Dream scope 绑定的 child 执行；若 restrict 或注册失败，创建失败，不能继续运行未隔离 child。限制的 disposer 随 agent scope 清理。

现场上游 `packages/core/tools/src/index.ts:697-705,1098-1131` 说明限制只作用于全局工具，scoped 注册保留。`packages/core/agent/src/index.ts:100-111,539-556` 说明 setup 在创建事件前完成，`agent/created` 串行 awaited；`packages/subagent/subagent/src/manager.ts:437-471` 在 materialize 后才提交首条消息。因此上述监听完成在首 prompt 前。必须用实际 child 的 schema 和执行路径复验，不能只凭源码推论。

Dream job **不依赖冷恢复补交结果**。runner 在 activation 存活期继续每 15 秒续租；租约丢失即中止；提交前再次确认 generation/lease；`finally` dispose。activation 死亡或进程中断时，job 失败并由服务端租约/重试机制回队列，不在恢复出的 child 里续跑。新 descriptor 不快照限制（`manager.ts:394-401`），冷恢复的旧字段重放（`:1348-1376`）不能用作新 job 的隔离依据。

验收门槛：rc.2 同基线的类型检查、构建及至少原有 47 例适配器测试全绿；新增契约测试证明首 prompt 前限制、实际工具 schema/执行、两个并发 child 隔离、activation 死亡回队列及 dispose。真实 DSH + 真实模型验收按 §4 执行，并在交接记录中逐项区分档位。若现场证明 `agent/created` 晚于首 prompt，立即停止实施，记录证据，不替换隔离方案。
