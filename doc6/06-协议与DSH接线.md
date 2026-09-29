# 06｜HTTP 增量与 DSH 接线

## 1. 现行接口事实

当前路由在 `agent-memory/crates/memory-server/src/main.rs:261-275`：`/v1/evidence/events`、`/v1/extraction/flush`、`/v1/memories/*`、`/v1/context/compose` 等。现行 `ComposeRequest` 对未知字段拒绝，`max_items=1..5`、`max_chars=100..2000`；`memory-store-sqlite/src/memories.rs:877-1005` 实现两条指令先占位。DSH `adapters/dsh/src/recall.ts:63-99` 只在存在最新原始用户消息时调用旧 compose，并在 500 ms 超时后跳过。

本阶段新增路由，不给旧请求偷偷加字段或改变旧响应。`protocol_version=1` 保留供老适配器握手；`/v1/version` 增加 `capabilities` 数组（老客户端忽略未知响应字段），如 `soul_v1`、`context_bundle_v1`、`derived_knowledge_v1`、`semantic_v1`、`dream_v1`。新版 DSH 适配器配置 `require_context_bundle=true` 时，启动握手若缺 capability 必须拒绝启用 v6 注入并报诊断，不静默落回旧“两条指令”模式。默认切换时机由部署卡控制。

## 2. 新 HTTP 端点（全部 Bearer scope）

| 方法/路径 | 请求主体/参数 | 成功 | 主要失败 |
|---|---|---|---|
| `GET /v1/soul?agent_id=...` | 稳定 agent ID | `{version,body_md,sha256}`；不存在时 `version:0,body_md:""` | 400 参数、401 token |
| `PUT /v1/soul` | `{agent_id,body_md,expected_version,idempotency_key}` | 创建 201 / 更新或同键重放 200，回版本 | 409 `VERSION_CONFLICT`，400 超长 |
| `GET /v1/soul/revisions?agent_id=...` | 分页 cursor | 当前 scope 的版本元数据；正文需单独 show | 400/401 |
| `POST /v1/resident/pins` | `{memory_id,position?,expected_pin_version?,idempotency_key}` | pin/重放 200 或 201 | 404 跨 scope，409 版本/状态 |
| `DELETE /v1/resident/pins/{memory_id}` | `Idempotency-Key` 请求头必填；可选 `{expected_pin_version}` 正文 | 已解除或同键重放 200 | 404 跨 scope ID，409 版本/幂等键冲突 |
| `POST /v1/resident/page-pins` / `DELETE /v1/resident/page-pins/{page_id}` | page ID、CAS/idempotency | 已发布有效页的固定/解除回执 | 404 跨 scope，409 页面失效/版本 |
| `GET /v1/resident` | `agent_id` 可选，仅为诊断 | 当前选择结果、included/omitted/conflict IDs | 401、400 限额 |
| `GET /v1/resident/suggestions` | 分页 cursor | 可供 pin 的 active fact/preference 与来源 ID；不改变注入 | 401、400 |
| `POST /v1/context/bundle` | [04 §4](04-召回与上下文.md) 的请求 | resident/retrieved 分段与诊断 | 401、400、内部错误 |
| `POST /v1/dream/triggers` | `{trigger_kind:"compact|scheduled|custom|manual",trigger_key,agent_id?,host_id?,session_id?,through_event_seq?}`；无 scope 字段 | 202，返回持久 trigger/job receipt 与当前状态；重复 key 返回原 receipt | 400 参数、401 token、404 不存在的宿主对象、409 冻结边界冲突 |
| `POST /v1/dream/runner/heartbeat` | `{runner_id,host_id,agent_id,capabilities}`；`capabilities` 至少区分 `chat`、`dream_v1`、`adjudicate_v1`、`consolidate_v1` | 刷新 DSH 常驻 runner lease；doctor 按 live heartbeat 分别报告 chat/runner 缺项 | 400 非法能力、401 token |
| `POST /v1/dream/runner/claim` | `{runner_id}`；只允许当前 scope 的 live runner，且要求 `chat`、`dream_v1` 和服务端 embedding 均可用 | 领取 memoryd 持久 Dream job 及其冻结输入；缺 chat/embedding 返回明确状态，不推进 L0 | 409 lease 无效；缺项以结构化状态返回 |
| `POST /v1/dream/runner/lease`、`POST /v1/dream/runner/failure` | runner ID、Dream generation 与当前阶段对应的作业 ID/generation | 续租或持久记录 provider/runner 失败；lease 过期会按冻结输入恢复 | 409 旧 generation 不得写入 |
| `POST /v1/dream/jobs/{id}/read` | `{runner_id,dream_generation,operation,ids?,query?,limit?,candidate_id?}`；scope 不在正文中 | 当前 job/generation 的 manifest、冻结 evidence、有效 L1/派生页搜索及其已冻结详情；metadata receipt 不保存原始 query | scope、runner lease、job generation/状态或读取预算失效时拒绝；跨 scope 不泄露对象 |
| `POST /v1/dream/jobs/{id}/candidates`、`POST /v1/dream/adjudications/{id}/submit`、`POST /v1/consolidation/jobs/{id}/publish` | 当前 runner ID、阶段 generation 和结构化候选/裁决/页面结果 | Rust 复核 scope、generation、原始 byte span、来源版本后写入；裁决结果与 processed 回执原子提交 | 409 旧 claim/来源漂移；422 Schema/span 非法 |
| `GET /v1/dream/jobs` / `GET /v1/dream/jobs/{id}` | 分页/状态过滤/完整 job ID | 当前用户 scope 的 trigger、输入计数、阶段状态、attempts、结果 ID 摘要和 error code | 401、400；跨用户/不存在 ID 均 404 |
| `GET /v1/pages` / `GET /v1/pages/{id}` | 分页/ID | 当前可见问题画像/主题页，含 `document_kind`、版本与 source IDs | 404 跨 scope/失效页 |
| `GET /v1/mental-model/questions` | 分页；可筛 `active|archived` | 当前 scope 的问题键、正文、版本与状态；不触发模型 | 401、400 |
| `POST /v1/pages/{id}/archive` | `{expected_version}` | 归档回执 | 404/409 |
| `GET /v1/consolidation/jobs` | 分页/状态 | 作业状态、错误码、输入 ID 元数据 | 401/400 |
| `POST /v1/memories/{id}/retire` | `{expected_version,reason_code,idempotency_key,user_evidence_id,instruction_quote,instruction_start_byte,instruction_end_byte}`；Agent 工具只可在用户明确要求停用时调用 | 200/201，回 `retired=true`、version、request ID | 404 跨 scope/不存在，409 版本或状态冲突，400/409 证据无效 |
| `POST /v1/memories/{id}/restore` | `{expected_version,idempotency_key,user_evidence_id,instruction_quote,instruction_start_byte,instruction_end_byte}`；Agent 工具只响应用户明确恢复要求 | 200/201，回当前状态与 ID | 404/409；证据失效或已过期不得恢复 |
| `POST /v1/memories/{id}/purge/preview` | `{expected_version}`；管理界面/CLI 使用，不注册为 Agent 工具 | 200，依赖 fingerprint、计数、一次性短期 confirmation token | 404/409；不修改记忆/索引，仅持久化短期 confirmation token 哈希 |
| `POST /v1/memories/{id}/purge/confirm` | `{expected_version,dependency_fingerprint,confirmation_token,idempotency_key}`；显式二次确认，不注册为 Agent 工具 | 202/200，持久 purge operation/job ID 与计数 | 404/409 fingerprint/version/token 冲突；token 过期拒绝 |
| `GET /v1/lifecycle/operations/{id}` | 完整 operation ID | 当前 scope 的 purge/retention 操作状态和无正文计数 | 404 跨 scope/不存在 |

所有 JSON 请求 `deny_unknown_fields`；`request_id` 是服务端链路诊断号，`idempotency_key` 是客户端提供的重复操作键（长度/字符集固定并持久记录），不要混为同一个字段。DELETE 的键从 `Idempotency-Key` 请求头读取，规范化请求哈希还包括路径 ID 与可选正文；不得同时在头和正文传两个不同的键。错误仍用现有 envelope `{request_id,error:{code,message}}`；新码需在 `memory-contract` 显式定义且适配器按端点解释，不能把所有 409 全局当用户可修复。

### Dream child 的 job-scoped 读取

`POST /v1/dream/jobs/{id}/read` 是 D6-12 增加的唯一 child 读取入口。操作为 `manifest|evidence|search_memories|get_memory|search_pages|get_page`。服务器从 Bearer token 派生 scope；请求必须带当前 runner ID 与 claim generation，任一不匹配即拒绝。`evidence` 每次最多 20 个 ID；memory/page 搜索 query 最多 512 个 Unicode 标量，结果上限由服务端钳制到 1—10；`dream_jobs.read_budget` 默认 32，schema 允许范围 1—64。搜索只返回当前 active、未过期、未 retired/purge_pending 且来源有效的对象；详情只能读取本 generation 先前搜索并冻结版本的目标。搜索回执只保存 query hash、结果数和 semantic 完整性，不存原始 query。

适配器通过 `ctx.subagents.start("spawn", request)` 建 Dream child，`maxDepth=1`、严格 `outputSchema`，并在 child request 设置 `toolFilter: { allow: [] }`，先拒绝全部继承的普通工具。仅在已识别且已绑定的 Dream child scoped context 上注册本 phase 允许的 `dream_read_*` 工具；每个工具再次验证 child agent/session 绑定，再携当前 runner/job/generation 调用 Rust。extract/redecision 只读 manifest/evidence；adjudicate 可搜索/读 L1；consolidation 另可搜索/读有效页面。没有普通 memory 写入、Soul/resident、retire/restore、purge、shell 或任意 HTTP 工具。最终候选、裁决和页面仍由 Rust endpoint 复核并返回 receipt。

写 Soul 的 HTTP 接口是给可信 UI/本机用户客户端使用；DSH 插件默认**不注册 soul 写工具**。pin/unpin 可以是用户操作工具，但任何工具成功声明都必须以服务器 receipt 为准，不能信任模型口头“已固定”。现有 `memory_remember` 保持用户已决定的开放直写语义。普通事件 ingest/turn end 不自动触发 extraction LLM；只有 `dream_v1` trigger 创建的持久 job 才能启动后台写入路径。旧 `/v1/extraction/flush` 对旧适配器继续可用，但不作为 D6 新默认触发。

遗忘边界遵守 [12](12-遗忘、过期与时间衰减方案决策.md)：旧 `/forget` 原样兼容且不承诺物理删除；新增 retire/restore 为可逆操作，主 Agent 可在明确用户请求下调用相应工具，Dream 无权调用；任何模型都不能直接调用 purge。Rust 对 Agent 请求核当前真实用户事件的 scope、role、最新性及 `instruction_quote` 的精确 UTF-8 byte span，并核目标 ID/version/状态；逐字证据不能证明自然语言与目标 ID 的语义匹配，Agent 的明确请求判断仍受工具策略约束。Purge 预览与确认只供明确的管理/用户客户端，执行前校验 scope、版本和依赖 fingerprint；预览仅写短期确认元数据。自动 retention 默认关闭，可信用户/管理员设正值后无须逐批二次确认，仍走同样的删除闭包和可恢复 job。响应、审计和 job 查询不得回显被删除的正文。

首版 retention 配置入口为本机管理 CLI：`memoryd retention show/set --config ... --tenant ... --user ... --raw-evidence-days N --expired-memory-days N --expected-policy-version V`；`0` 关闭对应自动清理。`set` 用 CAS 持久化 scope、策略版本和生效时间；不向 DSH 注册配置工具，不让模型通过普通 Bearer token 修改清理策略。doctor 显示当前策略版本、截止规则、待处理/失败 job 数，不输出正文。手工 purge 的 HTTP preview/confirm 与 retention job 的持续授权须在 API/日志中分开标识。

问题目录首版管理入口为可信用户/管理员 CLI：`memoryd mental-model questions list/add/update/archive/reactivate --config ... --tenant ... --user ... --key ... --expected-version ...`；add 要求该 scope 下 key 不存在，update/archive/reactivate 使用 CAS，重复同内容幂等。归档问题会同步使对应已发布画像 stale 并退出检索/Resident；问题版本改动后只能由后续 Dream 用当前 L1 重建。HTTP 首版只提供上述 GET 供审阅，不注册问题目录写工具给 DSH。

## 3. DSH 注入时序

本项目采用 TencentDB 的角色分层，但按融合决策只允许用户编辑 Soul 进入 system；Resident、Retrieved 和问题画像/主题页作为 sourced user context。`before_prompt_build` 是 TencentDB/其宿主的 hook 名，**不是 DSH API 名称**。官方 DSH 源码 `deepseek-harness/packages/core/agent-loop/src/agent.ts` 的 `preStep` 实际先 `systemPrompt.assemble()`，再执行 `agent/pre-step`。`packages/core/agent/src/dispatch.ts` 的 `assembleContextFor` 返回 `{agent,scope:agent,signal}`；`packages/core/system-prompt/src/index.ts` 声明 scope/signal，agent 字段由模块扩展提供。`packages/core/system-prompt/README.md` 说明 `system-prompt/assemble` waterfall 可异步改 assembly；section 进入 system prompt，context 进入带来源的 user-role runtime context。官方当前 checkout 的精确 revision、行号、类型和消息排列必须在 D6-0 再核对并记录，不能把本段路径描述当成未来 checkout 永久不变的事实。

建议接线：

1. `system-prompt/assemble`：按本安装的 user token 和实际 agent ID 请求 `GET /v1/soul`；添加单一 `agent-memory:soul` system section（有正文时），带 version；不使用 `complete:true`。每 step 读取或做经服务端版本确认的缓存；服务不可达时不使用旧正文，避免用户修改/删除后继续注入。
2. `agent/pre-step`：调用 `POST /v1/context/bundle`，即使 query 为空也取得 resident；将 `agent-memory:resident` 与 `agent-memory:retrieved` 作为独立、有来源的 user-role runtime context 放在本轮原始用户内容之前，匹配 TencentDB `prependContext` 的语义。保留插件来源和证据 ID；绝不将注入内容改标为 `source.kind='user'` 或合并进用户原文。新文本需 XML/Markdown 安全转义，不回流 L0。
3. 不在同一步同时调用旧 compose。对同一 `decision` 的重复 hook 要按 source marker 去重；切换配置期间若旧/新插件并存，应检测重复插件实例并拒绝装载或消重。
4. 超时/离线不注入旧缓存，记录 `request_id`、agent ID 哈希、失败层，不打印正文。魂与 resident 的失败单独计数；服务恢复后下一 step 自然重试。

如果 DSH 当前 API 无法在不破坏来源标记的前提下把这两条 user-role context 放在用户正文之前，保留分层意图并停止 D6-4 该接线：先把源码限制与可选排列写入 D6-0 交付记录，再更新 [11](11-Prompt注入方案决策.md) 和本节；不可通过把记忆提升到 system prompt 来绕过宿主限制。

`system-prompt/assemble` 发请求可能加重每 step 延迟；`GET soul` 保留默认 300 ms 失败保护，`POST /v1/context/bundle` 不设专用延迟预算，由 embedding provider 的 `embedding_timeout_secs`（默认 30 秒）约束外部请求，并跟随 DSH step 的取消信号。记录实际延迟用于后续诊断，但不得因超过某个 bundle 延迟目标而提前降级。为减少长期成本，可在拿到服务端可验证的版本后缓存**同版 soul**，但任何删除/改版必须在下一 step 可见；没有版本确认时不能继续用缓存。resident 不用无确认跨 step 缓存。

Dream child 是另一种 hook scope：`recall.ts` 在 Soul assemble 与 bundle pre-step 入口都先识别 subagent/fork session 并跳过普通人格和上下文；`events.ts` 同样排除 child session 的 L0 捕获。读取能力只通过上表列出的 job-scoped tools 暴露，child 不继承父 Agent 的普通 DSH 工具权限。

## 4. 一次交互的证据链

DSH 输入用户消息 → 既有 spool/ingest → 记忆 remember 或提取作业完成 → `GET soul`（system）和 `bundle`（resident/retrieved）返回 item IDs → 模型请求日志可判定两层是否实际在 wire 上 → 用户 correct/forget → 下一 step 的 bundle 不含旧 ID/page。模型回答是否遵守人格或利用记忆，要另做有/无对照；日志 `saw_injection` 只证明传输了文本。

保存回执问题仍单列：doc-handoff/10 观察到模型未调工具却自称“已记住”。v6 注入改善不自动解决这一点。已有 `memory_remember` 201/200 的 memory ID 才是同步保存凭据；异步提取未完成时只能称“已捕获，待处理”。若要宿主级用户可见保存状态，先核 DSH 回复/事件 API，另立实现卡，不在本规范中用 prompt 文案冒充回执。
