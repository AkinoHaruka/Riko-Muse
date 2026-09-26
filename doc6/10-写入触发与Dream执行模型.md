# 10｜写入触发与 Dream 执行模型

状态：**用户已确认的产品方向，尚未实施**（2026-09-26）。本文件定义何时由主 Agent 或后台 Dream 生成长期记忆。它与 [09](09-语义准入与记忆合并设计.md) 配套：09 规定如何裁决候选，本文件规定何时创建候选和启动该裁决。

## 1. 已确认的写入策略

系统有两条长期记忆写入路径：

| 路径 | 触发者 | 触发时机 | 行为 |
|---|---|---|---|
| 主 Agent 即时写入 | 当前主 Agent 通过 `memory_remember` 等明确记忆接口主动提出 | 对话中发现值得立即保留的信息时 | 同步提交一条有来源的记忆；只有 Rust 返回成功回执后，宿主/模型才可声称已保存 |
| 后台 Dream 批量写入 | 由 compact 生命周期事件、定时 Auto Dream 或用户配置的自定义事件启动的 Dream runner/子 Agent | 触发后异步执行 | 从持久 L0 证据读取一个冻结批次，提取原子候选，召回近似记忆并批量裁决，最后请求 Rust 原子提交 |

**默认不在每轮、每个 turn/end 或每次普通 event flush 后调用 LLM 自动提取和裁决。** 对话事件仍持续写入 L0；“事件已捕获”与“长期记忆已整理”是两个不同状态。既有 v1 `/v1/extraction/flush` 是已实现的兼容接口；新 D6 默认路径不把普通 turn/end flush 当作 Dream 触发器，也不把它报告成记忆已就绪。

## 2. 全链路

```text
每条宿主对话事件
    └─▶ 适配器可靠 spool → memoryd 幂等写入 L0（不调模型）

主 Agent 判断应立即记住
    └─▶ memory_remember → Rust 校验证据/scope → 提交 L1 → 返回明确 receipt

compact / 定时 Auto Dream / 自定义触发
    └─▶ 持久化 Dream trigger → 冻结待处理 L0 输入
        → 后台 Dream 子 Agent 提交结构化提案
        → embedding/词法召回 + 批量语义裁决（见 09）
        → Rust 复核 scope、byte span、版本与 generation
        → 原子应用 L1/evidence 与裁决结果
        → 成功后 best-effort 写 `memory_audit`
        → 后续问题画像/主题页与向量索引异步刷新
```

主 Agent 路径和 Dream 路径的“提出者”不同，语义准入严格度也不同，但 SQLite/Rust 内核仍是唯一持久化写入者。两条路径共同遵守认证 scope、用户来源、原文 span、schema、幂等、[14](14-记忆修改审计方案决策.md) 的 `memory_audit` 规则和 forget/suppression/lifecycle 等硬门；主 Agent 显式直写不再叠加 Dream 的持久性分类与批量语义裁决，Dream 则必须执行 [09 §3.1](09-语义准入与记忆合并设计.md) 的额外语义门。Dream 子 Agent 不直接连接 SQLite，不拿无限制的管理工具；它只能读取 job 固化范围内的证据/候选，并返回版本化 JSON 提案。Rust 是最终提交闸门。

## 3. L0 采集不等于后台写记忆

1. 适配器继续将真实对话事件写入本 scope 的 `evidence_events`；使用当前 spool、顺序发送和 receipt 恢复保证事件至少一次送达、幂等入库。
2. 事件入库不触发 extraction LLM，也不改变 candidate/memory 状态。adapter 侧的阈值可以用于排空网络队列、落盘或传输批次，但不能偷偷创建提取作业。
3. Dream 输入从 memoryd 的规范 L0 读取，不把 compaction summary 当作证据源。summary 可作为定位/理解辅助，但每个被创建/更新的 L1 都必须引用原始用户 event ID 与连续 byte span。
4. job 可将 assistant/tool 等邻近消息作为理解语境，但这些消息不能成为用户事实的来源；只有 Rust 核准的用户事件 span 可以支持用户记忆。
5. event sequence 在当前协议里按 `(scope, host_id, session_id)` 计数，不能把单会话的 `event_seq` 当作全用户全局 cursor。后台 job 必须冻结实际 evidence ID 集合或使用经源码证明的全局稳定水位，不能仅按时间戳猜测“已处理到哪里”。

## 4. 触发器

触发器只负责**入队**，不在宿主 hook 中同步等待 Dream 完成。触发请求通过新增 `POST /v1/dream/triggers`；成功表示入队 receipt 已持久化，不表示记忆已经创建。状态查询用 `GET /v1/dream/jobs` 与 `GET /v1/dream/jobs/{id}`，具体 error envelope 沿用 [06](06-协议与DSH接线.md)。

### 4.1 Compact 触发

- 当宿主确实发出 compact/compaction 生命周期事件时，可提交 `trigger_kind=compact`，附宿主、会话和可验证的事件上界/关联 ID。
- trigger 到达 memoryd 前，适配器先确保该 session 的前序 spool event 已 ack；如果当前进程离线，trigger 与事件一样先持久落在本地 spool，恢复后按顺序重放。
- memoryd 从 L0 建立不可变 job 输入清单；compact summary 本身不取代 L0。
- 同一 compaction ID/trigger key 重放只返回原 trigger/job，不生成第二份 Dream。
- 若宿主没有真正发出的 compaction hook，适配器不能从 token 数或“快到上下文上限”自行声称收到 compact。可由 Auto Dream 定时器补足。

### 4.2 定时 Auto Dream

- **默认启用**定时整理。首版配置初值：每个 user scope 最多每 24 小时调度一次、最近一次该 scope 的 L0 事件距今至少 15 分钟、至少有 1 条尚未处理的真实 user event；单批最多 80 条事件、序列化输入最多 24 KiB、模型输入预算最多 8192 tokens、模型输出上限 2048 tokens。以上均可显式配置；达到事件数、字节或 token 边界时在完整事件前切窗，剩余事件保持 pending。token 计数器不可用时不得声称精确 token 受控：仍严格执行字节边界并记录 `token_budget_status=unverified`，部署者应配置更低字节上限。
- `dream.enabled=true` 是默认配置。未配置可用 chat 模型、D6-8 语义自动应用所需的 embedding provider，或经 D6-0 核实的后台执行器时，不启动批量模型链、不重复建立空作业；L0 与待处理账本保留，doctor 报 `dream_status=missing_model|missing_embedding|missing_runner`。用户可设 `dream.enabled=false`，此时 compact/custom/manual 的明确触发仍可入队，但无可用执行器时只停在可诊断的待处理状态。
- 同一个 scope 已有 queued/running job 时，新 timer tick 合并为 `pending_trigger` 或返回既有 job；不能并行启动多个 Agent 争抢同一批事件。
- 空闲时才跑可以减少模型竞争；即将超出输入预算时按稳定边界分批，不能截断 quote 或丢掉剩余事件。
- 单条 user event 自身超过配置的输入字节/token 上限时，不得让它永远占住队首：创建可诊断的确定性 `INPUT_TOO_LARGE` dead 结果并保留完整 L0，后续证据继续分批处理；管理员可在提高预算后对该 event 显式 requeue。不能截取其前半段冒充完整证据。

### 4.3 自定义触发

- DSH 其他可靠生命周期事件、用户/管理员 UI/API、CLI 或外部自动化都可请求 enqueue；所有触发均需经过 memoryd Bearer scope 认证。
- 可配置触发器只定义条件、scope 选择和批次策略，不允许配置任意 SQL、跨 scope 查询或 shell 命令。
- 暂不允许模型自己发出“立即启动 Dream”来形成无限递归或高频循环。若未来开放，须有频率/预算上限、去重 key 和明确审计。

## 5. Dream job 的持久化与水位

新增 `0007_dream_jobs.sql`，逻辑表和约束详见 [02](02-数据模型与迁移.md)：

- `dream_jobs`：scope、trigger kind/key、触发元数据、pipeline/prompt 版本（首版 `extract_version=dream_extract_v1`）、状态、attempts、`run_after`、lease/generation、输入 fingerprint、模型用量和 error code。
- `dream_job_inputs`：job 固化的 evidence ID、scope、host/session/event_seq、role、source hash 及输入顺序。job 重试时仍用同一列表；不得重试时重新查询“当前所有新事件”。
- `dream_evidence_state`（或实现等价的唯一处理账本）：每个用户 evidence 在该 Dream pipeline 中处于 pending/assigned/processed 状态，并关联唯一 active job。`processed` 表示该证据已被一个成功 job 明确判断过（包括 not_memory/defer 等结果），不等于一定产生了 Active 记忆。`defer` 候选另有 Held 状态与来源版本账本；原 L0 已处理不妨碍新证据到来后按新输入指纹重裁该候选。
- trigger/job/input 的同键请求幂等；一份输入不能同时被两个独立 job 分配。发生部分失败时，原 job 持有其冻结输入并走 retry；确定性 dead 后由显式 retry/requeue 决策恢复，不静默丢弃输入。

建立快照的事务边界需解决并发入库：触发事务选择输入集合并写 job/input/assigned 状态；在该快照之后入库的事件保持 pending，等待下一 job。不得只保存最大 `event_seq` 并据此跨 session 推断范围。跨多 session 的全用户 Dream 需要显式 job input IDs，或另有经验证的 scope 级单调摄取序号。

语义判断得到 Held/defer 后，只有新 Dream 批次提供同 scope、可验证且与该候选相关的新用户证据，或用户明确发起重裁，才自动建立新版本裁决；同一候选与相同证据/策略指纹只裁决一次。语义服务超时、429、不可用或向量索引临时失效属于**作业未完成**，不能伪装成成功的 `defer` 并推进 evidence 到 `processed`。有限重试仍失败时进入可诊断的 `provider_wait`，保留冻结输入与 `assigned`；定时器至多每 24 小时检查一次所配置端点及索引的就绪状态，恢复后重新领取原输入。坏 JSON、越权引用和未知版本仍按确定性 dead 处理，不自动循环。

建议 job 状态沿用可靠作业语义：`queued|running|succeeded|retryable_failed|dead|stale_input`；过期 running 恢复、claim generation、防旧执行者提交、短事务和网络调用不持 DB 锁沿用 doc4。每个 user scope 同一时刻最多一个 Dream job running；实现时若按 Agent 或 trigger family 分区并发，必须证明不会竞争同一 source state。

## 6. Dream 子 Agent 的职责与权限

Dream 子 Agent 一次只处理一个有预算上限的冻结 job：

1. 读取 job 输入中允许的 L0 消息；按 `extract_version` 提取独立候选并逐字引用 user evidence。
2. 由 Rust 校验 evidence ID、user role、窗口归属与 byte span；非法候选不进入后续裁决。
3. 对有效候选执行 [09](09-语义准入与记忆合并设计.md) 的 exact hash、混合召回、批量语义 adjudication；把候选与 target ID/version/action 一起返回。
4. Rust 再校验冻结输入、scope、target 当前状态、版本和 generation，在一个事务内应用结果。
5. 需要派生知识整理时，只在 L1 提交成功后由 Dream job 编排画像/主题页；二者均为派生资料，不是替代 L1 的长期真相。

Dream 子 Agent 默认**不得**：

- 调用主 Agent 的 `memory_remember` 工具绕开批量 job；直接写 SQLite；提供 scope/user/tenant 身份；引用输入集合以外的 evidence 或 target IDs。
- 自动 retire/restore/forget、物理删除/purge、改写 Soul、pin/unpin、改变用户配置或确认敏感设置。
- 把自己生成的摘要作为 evidence；把 `not_memory/defer/conflict` 静默删除；以模型口头回复代替 server receipt。

若 LLM 返回 `create/update/attach_evidence/keep_separate/conflict/defer/not_memory`，按 [09 §5](09-语义准入与记忆合并设计.md) 的版本化协议处理。Dream 必须先满足共同证据硬门，再满足语义准入映射；trigger 入队、模型提出候选或 embedding 找到近邻都不单独构成放行。明确、有证据且符合当前 admission policy 的记忆可以自动提交，不要求逐条人工批准；含糊或冲突结果进入 Held/待处理，且原始证据仍保留。成功回复必须列出 committed memory IDs、候选/held IDs 和未应用原因，避免“已记住”但没有回执。

## 7. 主 Agent 即时写入契约

- 主 Agent 可在日常对话中主动判断是否立即调用现有记忆写工具；这条路径不等待 compact 或下一次 Dream。
- **准入比 Dream 路径少一层语义分类，但不放松证据硬门**：工具调用表达主 Agent 的即时保存意图，因此不要求再运行 Dream 的 `durable/time_bound/uncertain/not_memory` 分类，也不跑 embedding + LLM 批量裁决。Rust 仍必须核验认证 scope、用户来源、当前接口允许的 evidence ID、精确连续 byte span、schema/长度、幂等及 forget/suppression；任一失败都不能 Active。成功更新已有 L1 后按 [14](14-记忆修改审计方案决策.md) 尝试写 audit metadata。
- 直写遇到规范化 claim 的精确重复时走确定性幂等/证据关联；只有近义、更新或冲突等非精确关系不在同步路径猜测，交给后续 Dream 裁决。这样保留即时写入，同时不把 embedding 相似度误当成自动合并证据。
- 每次成功都使用 Rust 返回的 memory ID/status/receipt；tool timeout 或错误不算已写入。重试使用原 idempotency key，避免二次创建。
- 只有当前接口实际提供且 Rust 核验过的证据可写入。若主 Agent 想记住多轮汇总、旧 session 内容或需要重组多个事实，应交给 Dream job，而不是伪造“最新用户事件”来直写。
- 同一内容随后进入 Dream 时，精确 hash 和语义 adjudication 应将它关联到已有 L1 或判为不同 claim，不能再复制一条。
- 主 Agent 直写与 Dream 使用同一个 scope 隔离、revision、`memory_audit`、forget 抑制与生命周期过滤机制；两条路径不得维护各自冲突的事实表。retired、forgotten、purge_pending、过期对象不得作为更新目标；Dream 不得调用 retire/forget/purge。
- receipt/job result 必须保留 `write_path=main_agent|dream` 与实际 admission/prompt/adjudication version；`main_agent` 路径不应伪填一个未运行的 Dream policy version。`memory_audit` 仅按 [14](14-记忆修改审计方案决策.md) 保存目标层、动作、认证 scope、可信宿主可提供的 Agent/task 关联、记录版本、时间和 request ID。

## 8. DSH 接线的源码前置条件

当前 DSH clone 中 `SessionStartSource` 类型包含 `'compact'`，但类型存在不能证明运行时真的发出该事件。实施 D6-0 时必须沿 driver 到实际 emitter 核实 compact 事件、定时任务和子 Agent 后台 dispatch 的接口形状；以当时源码为准，不按类型名猜测。

如果 compact 事件不存在，先保留该触发器为“宿主 capability 未支持”，通过定时/用户配置 API 触发，不得伪造事件。如果 DSH 子 Agent 仅支持当前回合内同步委派、不支持独立后台生命周期，则 D6-0 要记录限制并在编码前确定运行架构：可以由 DSH 的持久 jobs 驱动一次性 Dream 子 Agent；若没有可用后台 dispatch seam，则提请用户选择受控 Dream runner 部署方式，不能把普通 extraction worker 改名为“子 Agent”来宣称实现了该要求。

真实路径要求：trigger 被 durable ack → job 冻结输入 → 子 Agent 提案 → Rust receipt → 下一次 bundle 可读取已提交记忆。只有 mock/tool 单测不能证明真正 compact hook 或后台子 Agent 已连接。

## 9. 运行状态与用户可见结果

至少区分以下状态：

| 状态 | 含义 |
|---|---|
| `captured` | L0 event 已由服务端持久化 |
| `trigger_queued` | Dream trigger/job 已持久化，尚未开始处理 |
| `dream_running` | 后台 Agent 正在处理固定输入 |
| `dream_succeeded` | 本批完成裁决；可能创建 0 条 memory |
| `dream_retryable/dead` | 作业失败或需要人工 retry；L0 和输入账本仍保留 |
| `memory_committed` | Rust receipt 列出的 memory ID 已提交，可按 scope 检索 |
| `page_index_pending` | 派生页/向量等后续投影尚在更新；不影响 L0/已提交 L1 的权威状态 |

界面/Agent 回答不能把 `captured`、`trigger_queued`、`dream_succeeded` 说成 `memory_committed`。Dream 返回“本次没有长期记忆”是有效完成，不应为填数量强行创建。

## 10. 验收与故障场景

- 普通消息/turn end/adapter threshold flush 只确认事件持久化；没有配置触发时后台模型调用为 0。
- 主 Agent 主动直写可立即返回 receipt；超时、拒绝和相同 key 重放结果可区分。
- compact、定时、custom trigger 各自能持久入队；同 trigger 重放不产生双 job；同 scope job 合并/串行行为可观察。
- compact 触发时 spool 中前序事件未 ack，trigger 不越过缺口；内核重启后先补发 L0，再处理对应 Dream 输入。
- 快照后新到事件留给下一批；当前 job 重试始终使用原 evidence IDs；不同 session event_seq 不发生错误跨会话排序。
- 子 Agent 只能访问冻结 scope/输入，无法调用任意直写/forget/Soul mutation；模型输出伪造 ID 被 Rust 拒绝。
- 主 Agent 直写与随后 Dream 处理同一事件时，最终无重复 active memory，source 关系正确。
- Dream 同时遇到新增/更新/退休/遗忘/清理造成目标版本或生命周期变化时，整批 stale 或 CAS 冲突，不能部分提交或复活 retired/forgotten/purged source。
- 一个 Dream job 成功但 0 个 Active 输出时，诊断如实显示 0；模型超时/子 Agent 退出后 lease 恢复且证据不会丢。
- DSH 实际 compact event 与后台 dispatch 分别验证；未能证明某项宿主能力就写 `未验证/宿主不支持`，不以手动 API 调用代替。
