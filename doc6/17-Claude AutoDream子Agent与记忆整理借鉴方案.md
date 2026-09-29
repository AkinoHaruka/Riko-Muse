# 17｜Claude Auto Dream 子 Agent 与记忆整理借鉴方案

状态：**D6-11—D6-15 已实施；本文件保留设计依据、边界和验收要求。** 代码与官方 DSH 固定响应证据见 [doc-handoff/20](../doc-handoff/20-D6-11-15交付记录.md)。此次未调用真实模型；语义质量、全面验收、性能和部署不因此视为通过。日期：2026-09-28。

本文将 Claude Code Auto Dream 源码中可借鉴的整理流程，改写为适用于本项目 Rust/memoryd 权威模型、DSH 常驻 runner 和持久 Dream job 的设计与实施契约。实施前的源码核对已记录于 [doc-handoff/13](../doc-handoff/13-D6源码核对.md)；本轮实施基线和验证档位见 [doc-handoff/20](../doc-handoff/20-D6-11-15交付记录.md)。

## 1. 目标与方案摘要

目标是让 Dream 子 Agent 能够在一个持久、冻结、可追溯的任务范围内，先了解已有记忆，再搜索相关 L1 和派生知识，最后提出合并、更新或新建建议。D6-11—D6-15 已为其接入 job-scoped 只读工具和按 phase 的有限探索；本段描述的是提出该扩展前的差距。

建议采用以下融合方式：

1. 借鉴 Claude Auto Dream 的分阶段反思流程：**了解现状 → 收集新证据 → 对照已有记忆 → 提出整理方案 → 生成或更新派生知识 → 清理过期派生索引**。
2. Dream 子 Agent 通过**任务范围受限的只读能力**查看本 job 冻结的 L0、同一认证 scope 下合格的现有 L1，以及来源仍有效的派生页。
3. 所有读取和最终写入仍由 memoryd 依 scope、job、generation、状态和来源版本校验。子 Agent 只返回结构化提案，不能直接写数据库、改 Markdown 权威文件或执行生命周期动作。
4. 复用现有 Hindsight 主体流程与 TencentDB query-embedding/候选召回路径。不要额外增加一个无边界的 LLM 文件选择器；子 Agent 的读取工具调用现有检索能力并返回有界结果。
5. 延续 D6 已确认的角色边界：Soul 仍由用户编辑；Resident、原子记忆和派生知识保持 user-role 数据；Dream 产物不得成为 Soul 或覆盖 L0/L1。主题页只按需召回，用户 pin 后才进入 Resident。
6. 保留用户已经选择的 Auto Dream 触发规则：默认定时启用，每 scope 最多 24 小时一次、至少空闲 15 分钟且有新 user evidence；普通 turn/end/flush 不启动 Dream LLM。不得照搬 Claude 的 24 小时 + 5 个 session 门槛。

建议的数据流：

持久 Dream trigger/job
  → Rust 冻结 L0 输入、job 状态和可读版本快照
  → Dream child 查看 job manifest
  → 只读搜索同 scope 的现有 L1/有效派生页
  → child 提交候选与合并/整理提案
  → Rust 校验证据 span、target version、准入与 generation 并提交
  → 派生页从已提交 L1 生成或更新
  → Rust 复核来源并发布，旧投影失效
  → 返回 Rust receipt 与 job 结果

## 2. 上游源码核对与报告勘误

以下结论来自只读检查 Claude 源码，而非只根据两份分析报告推断。参考 checkout：C:\TRAE\Claude - 副本\claude-code-source-code。

| Claude 源码位置 | 直接观察到的实现 | 对本项目的借鉴范围 |
|---|---|---|
| src/services/autoDream/autoDream.ts:59-90、133-168、218-227 | Auto Dream 有可配置的时间门槛、会话数门槛、扫描节流和锁；当前默认值为 24 小时及 5 个 session，通过后才构造 consolidation prompt 并调用 runForkedAgent。 | 借鉴“触发门控与整理执行分开”“避免并发整理”的做法；不复制其 session 门槛、配置存储或 stop-hook 触发时机。 |
| src/services/autoDream/consolidationPrompt.ts:10-64 | Prompt 把工作分成 Orient、Gather recent signal、Consolidate、Prune and index；要求先阅读已有记忆，再将新信号合并进合适主题，最后重建索引。 | 借鉴显式多阶段任务和先读后改的工作顺序；把文件读写替换为受限读取、结构化提案和 Rust 原子提交。 |
| src/services/autoDream/autoDream.ts:210-232 | Auto Dream 用 runForkedAgent 执行；工具授权由 createAutoMemCanUseTool(memoryRoot) 限定到记忆目录，Bash 限制为只读。 | 借鉴能力最小化与任务专用 Agent；不照搬允许 Agent 直接写文件的授权。 |
| src/services/extractMemories/extractMemories.ts:167-213、340-415 | 另有 Extract Memories 流程，复用 forked agent 和记忆目录权限；受 feature/config、是否已有主 Agent 写入及 throttle 等门控。 | 说明 Auto Dream 与 Extract Memories 是不同路径。本项目明确不采用每轮 stop-hook 抽取，不能把 Extract Memories 当作 Auto Dream 的必要前置步骤。 |
| src/memdir/memoryScan.ts:21-73 | 递归扫描 Markdown 文件，排除 MEMORY.md，把候选按 mtime 排序后限制在最多 200 个。 | 借鉴有界目录/候选概览，不把“所有记忆必须平铺”误当成能力约束；本项目用数据库中的有界清单替代文件扫描。 |
| src/memdir/memoryTypes.ts:14-30 | user、feedback、project、reference 是一组记忆类型；parseMemoryType 对缺失或无效值返回 undefined，兼容旧文件。 | 类型适合作为内容领域提示，不是可靠性或事实准入规则，也不是所有记录都必须具备的硬约束。 |
| src/memdir/findRelevantMemories.ts:39-74 | 先扫描文件头，再由模型按 query 选择相关文件，最多返回 5 个。 | 借鉴描述字段用于快速理解内容、限制读取量；不复制额外的 Sonnet 选择器，避免与本项目已有向量/词法召回重复。 |
| src/memdir/memdir.ts:196-233、272-317 | 提示词要求先检查现有记忆、避免重复，按主题组织；MEMORY.md 作为索引而非全文存储。 | 借鉴“索引与内容分离”和“优先更新已有主题”；本项目的索引和页均为 SQLite 权威状态上的可重建投影，不以本地 Markdown 作为第二写入真相。 |

两份分析报告需要按源码修正的表述：

- “记忆目录扁平、没有子目录”不准确。memoryScan.ts 使用递归 readdir；实际提示词偏向把普通主题文件写在顶层，但扫描器没有强制扁平结构。
- “四种 type 必填”不准确。解析器允许 type 缺失或无效并返回 undefined，至少保留了旧文件兼容路径。
- “每轮必然运行 Extract Memories”不准确。该路径存在 feature/config、主 Agent 已写入跳过、节流等门控；Auto Dream 自身又有单独的时间/会话门控。
- “检索按类型过滤”不准确。findRelevantMemories 的选择主要基于扫描出的文件描述及查询相关性，最多选 5 个；type 是上下文分类，不是该函数的硬过滤条件。

因此本项目借鉴的是 Auto Dream 的**反思顺序、受控读取、主题组织和索引维护方法**，不是其文件权限模型、分类约束或触发配置的整套复制。

## 3. 与本项目现状的差距

### 3.1 已有能力与明确限制

当前实现的 schema 常量在 `agent-memory/crates/memory-contract/src/lib.rs` 定义为 13。D6-11—D6-15 的实施与验收现状以 [doc-handoff/20](../doc-handoff/20-D6-11-15交付记录.md) 为准；此前 schema 11 是 D6-11 开工时基线。

DSH 适配器 `agent-memory/adapters/dsh/src/dream-runner.ts` 通过 `ctx.subagents.start("spawn", request)` 启动 child，设置 `maxDepth=1`、结构化 `outputSchema` 和 `toolFilter: {allow: []}`。child 默认没有继承的普通工具；只在其 scoped context 上注册当前 phase 允许的 Dream 只读工具，再由 Rust 复核并提交结构化结果。官方 DSH 固定响应运行证据见交付记录。

适配器 agent-memory/adapters/dsh/src/index.ts 已在主 Agent 的 pre-step 启动常驻 DreamRunner；会话事件捕获通过 isCapturableSessionHeader 排除 DSH 子 Agent，避免内部 Dream 指令进入 L0。这些现有保护应在扩展读取能力时继续成立。

适配器仍注册普通 Agent 的 system-prompt/assemble 与 agent/pre-step hook；Dream child 入口在两处识别 subagent/fork session 并跳过 Soul、bundle/compose。L0 捕获也排除 child session，避免内部 prompt/tool 内容进入用户证据。D6-11 的官方 DSH 固定响应运行检查覆盖了该隔离，源码锚点见 [doc-handoff/13 §12](../doc-handoff/13-D6源码核对.md)。

### 3.2 需要解决的问题

1. child 在读取已有记忆前不能确认候选是新事实、已有 claim 的补充，还是旧值更新。
2. 每阶段一次结构化输出适合严格裁决，但缺少有限的“查找—阅读—比较”循环，长远会把所有判断压力推给一次 prompt。
3. 页面和画像可被生成，但主题组织需要知道已有页面与来源，才能选择更新已有主题或保留不同主题。
4. 增加 Agent 读取能力会扩大数据暴露面；必须限定 scope、job、读集合和记录数，且不能以工具输入提供的 user_id/tenant_id 作为身份来源。

## 4. 目标权限模型：只读探索，Rust 写入

### 4.1 Dream child 可以执行的逻辑操作

以下能力契约已由 D6-12—D6-14 落入 HTTP、Rust 存储和 DSH scoped tools；字段与实现路径见 [doc6/06](06-协议与DSH接线.md) 和 [doc-handoff/20](../doc-handoff/20-D6-11-15交付记录.md)。

| 逻辑操作 | 读取范围 | Rust 必须校验 |
|---|---|---|
| 查看 job manifest | 当前领取的 Dream job、phase、冻结输入摘要、策略版本、预算及已读/已提案状态 | Bearer scope、job 属于该 scope、job 为 running、generation 与 lease 当前有效 |
| 读取 job evidence | 仅该 job 的冻结 evidence IDs；可读 user evidence 原文以提取连续 span，邻近 assistant/tool 只作为非引用语境 | evidence ID 必须在 job input 列表；来源角色、顺序和 byte span 依据服务端记录 |
| 搜索相关 L1 | 认证 scope 内当前 active、未过期且未退休/forgotten/purge_pending 的候选 | scope 从 Bearer 派生；生命周期过滤先于排名；返回 ID、version、kind、claim 和受限来源摘要 |
| 读取 L1 详情/来源 | 仅同一 job 的搜索结果或已冻结 target set | target 在该 scope 可见，版本/状态有效；输出必须保留 source IDs 与版本 |
| 搜索/读取派生页 | 仅 published 且所有 source versions 仍有效的页面 | scope、page status、source validity、page version；过期或 stale 页不得返回正文 |

子 Agent 不获得以下能力：普通 memory_remember/correct/forget/retire/restore/purge 工具、Soul/resident 管理、任意数据库查询、shell/文件系统读写、跨 scope 查询或自定义 HTTP。即使 DSH 工具过滤配置出错，memoryd 对 job、scope、generation 和状态的服务端校验仍必须拒绝越权请求。

### 4.2 读取快照与并发

- Dream trigger 建立的 L0 evidence ID 顺序和内容指纹仍是本 job 的冻结输入；重试始终读取相同输入。
- 每次搜索现有 L1/page 时，Rust 返回本次可见 target 的 ID、version 和 content fingerprint。模型只能针对实际返回的 target 提出 update/merge。
- 在一个 job 多次读取时，建议同一 scope 使用该 job 首次取得的 read snapshot；如现有存储无法提供跨调用一致快照，至少把已读的 target version/fingerprint 持久化并在重复读取时固定返回。
- 最终应用仍执行 doc6/09 的 source、scope、byte span、准入、target CAS 和 generation 复核。读取成功不锁定数据，也不代表最后能提交。
- 若 target 在处理中被 correct、retire、forget、purge 或到期，Rust 标记该动作 stale/conflict；child 不得把已失效内容重新写回。
- 搜索结果和读取回执可以记录 ID、version、耗时、数量和错误码；不得把私密正文写入普通日志或诊断输出。

## 5. 后续 Dream 工作流

Claude 的四阶段可以映射到本项目 job 状态机，但 Rust 保留每个阶段的持久化边界。

| 阶段 | Dream child 工作 | Rust/memoryd 工作 | 阶段完成条件 |
|---|---|---|---|
| Orient｜了解现状 | 读取 manifest：job 目标、冻结事件数量/类型、已有工作状态；按需查看 scope 内现有主题目录摘要。 | 只返回同 scope、有界、不过期的元数据；不把完整 L1 全库塞进 prompt。 | child 明确知道本 job 可引用的证据、当前已有候选和预算。 |
| Gather｜收集信号 | 从冻结 user events 抽取可能长期有效的独立命题；必要时读相邻非用户消息作语境，不将其引用成用户证据。 | 返回冻结 L0 原文与 event IDs；任何引用都必须能映射到原文连续 UTF-8 byte span。 | 每个提案可回指至少一条允许的 user evidence；无价值时允许返回空提案。 |
| Compare｜对照已有知识 | 对候选调用 scope 内的相关 L1/page 搜索；读取有限 top candidates；判断 duplicate/update/contradiction/complement/keep-separate/defer。 | 召回使用 doc6/09 的 embedding 候选路径及既有词法候选；向量仅找候选，不直接裁决。 | 每个更新/合并提案都包含现有 target ID/version 和证据关系；没有近邻不代表模型可捏造 target。 |
| Propose｜形成裁决提案 | 输出版本化 schema：create、update、attach evidence、keep separate、conflict、defer、not memory，以及原因和引用。 | 校验枚举、数量、输入范围、quote/span 与目标版本；按 doc6/09 语义裁决。 | 提案可被 Rust 接受或被明确拒绝；不能以自然语言摘要替代 receipt。 |
| Apply｜原子提交 | child 结束提案，不自行写入。 | Rust 在事务内执行 source/lifecycle/version/generation 复核并写 L1、evidence、adjudication 和 receipt。 | 只以 Rust receipt 列出的 ID/status 宣称已保存；held/defer 和 0 Active 也如实返回。 |
| Organize｜整理派生知识 | 在对应 Dream job 仍有效时，为登记的问题生成 mental model；根据已提交/仍 active 的 L1 更新 topic page。 | 冻结来源 ID/version，校验严格 schema，CAS 发布页面；source 改变则拒绝或 stale。 | 页面可重建、可审阅、可追溯；没有足够来源时不生成空页面。 |
| Prune｜整理投影 | 提议旧派生页/索引是否需要重建、归档或合并。 | Rust 只允许操作派生知识自身的发布状态和索引；source 失效时立即使旧页不可见。 | 不删 L0/L1，不执行忘记/退休/purge，不把源事实“整理掉”。 |

DreamRunner 按 phase 启动受限 child。job ID、generation、冻结输入和 phase 版本均由 memoryd 持久化；读取工具记录 job-scoped receipt，网络/模型调用期间不持有 SQLite 写事务。provider wait/cancel 恢复依既有 frozen input 与 claim generation 契约处理。

### 5.1 工作提示词约束

Dream 专用提示词应固定并版本化，至少说明：

- 先看已有记忆，再决定 create/update/keep-separate；严禁按“多产出”为目标。
- 每个主张保留最短充分的逐字用户原文 span；不补主语、不拼接不连续原文。
- 不同事实不能为了文件/页面整齐而强行合并；状态变化、时间性主张与稳定偏好按 doc6/09 已确认语义处理。
- embedding 相似度是候选线索，不是真实性、重复关系或准入结论。
- 页面摘要必须把事实、用户明确表达、模型归纳和不确定处区分清楚；没有支持的关系不补全。
- 提案不得自行创造 scope、evidence ID、memory ID、page ID、问题 key、策略版本、状态或 receipt。
- 失败、工具无结果、引用不确定时返回 defer/empty proposal，而不是猜测。

提示词模板、输出 Schema、工具契约和语义裁决版本分别固定。未来修改采用新版本；历史 Dream job 继续按 job 行记录的版本恢复。

## 6. 记忆类型、主题与 Markdown 的边界

Claude 的四类 user/feedback/project/reference 和本项目 fact/preference/instruction/episode **回答不同问题**：

| 维度 | Claude 示例 | 本项目现有示例 | 代表的问题 |
|---|---|---|---|
| 内容领域/来源语境 | user、feedback、project、reference | 当前未建立对应的必填维度 | “这条知识属于哪类上下文？” |
| 记忆语义类型 | 不由上述四类表达 | fact、preference、instruction、episode | “这条记忆表达什么性质的主张？” |

不得把两组枚举互相映射或用 Claude 类型取代现有 L1 kind。建议后续按以下次序演进：

1. 第一轮只在 topic page 上使用稳定主题 key、title、description、source IDs/versions；这已足以组织项目/个人/工作方式等主题。
2. description 用一句可检索的具体说明，帮助 Agent 与检索器判断页面覆盖范围；不能把它当成事实来源。
3. 如果真实样本证明领域过滤有价值，再提出独立、可选的 domain/tags 维度与迁移；不要求每条 L1 必须分类，也不让分类值参与 evidence/admission 决策。
4. 不将 Claude 的文件名前缀或目录结构引入 SQLite 语义；本地 Markdown 只用于 Soul 编辑/导入、导出或审阅，不成为第二事实源。
5. memory.md 仍是 Resident 当前选择的可重建导出；主题页不自动变 Resident。问题画像问题目录默认空，只有用户登记问题才可生成画像；topic page 独立于问题目录。

检索侧不复制 Claude 的“按文件描述再调用一个模型选最多五个文件”路径。本项目优先通过当前 FTS/embedding/RRF/rerank 管线检索 L1 与有效派生页，避免新增第二次选择调用、结果竞态和另一套召回质量指标。派生页的 title/description 可以进入相应索引；页面正文仍须受 source validity 和上下文预算约束。

## 7. 运行、安全与降级规则

- 触发契约不变：只处理已持久化 compact/scheduled/custom/manual Dream job。普通 turn/end、flush、工具查询本身不触发 Dream 模型。
- 自动调度参数延续 doc6/10 已确认值；Claude 的 5-session 条件不能成为本项目新门槛。
- child 只接受 memoryd 给该 scope/job 的工具能力。所有请求身份从 Bearer token 派生，客户端/模型不提交 tenant_id/user_id。
- 每次工具请求核 job status、lease、generation；lease 失效后 child 读取或提交均被拒绝。
- 固定每 job 最大输入/输出和只读工具调用预算。受限读取由 job 字段持久记录，默认 32 次、schema 允许范围 1—64；本版没有另设用户配置接口。job 持久化消费数与已读 target snapshot，doctor/job 状态供运维查看。模型阶段的输入/输出仍受各 phase schema 与既有 provider 限额约束。
- 外部模型暂时不可用、429 或超时：保留 job 的冻结输入并按既有 provider_wait/退避处理；不把“没读到”或“工具失败”转成 not_memory、Held 或成功。
- child crash/cancel：恢复同一 job generation 规则与 frozen input；任何旧 child 的迟到输出不得提交。
- query/read 只有只读副作用；不得借读工具更新 Resident、审计记忆正文、创建记忆或删除页面。
- Dream child 的内部 prompt/tool messages 不进入 L0；官方 session header 过滤继续覆盖所有 spawn/fork/continuable 运行模式。
- 用户可见回答必须区分 job queued/running、提案、Rust committed、派生页 published。不能因 child 说“整理好了”就声称记忆已更新。

## 8. D6-11—D6-15 实施卡与验收边界

以下卡片曾作为实施计划，现已按 [08](08-施工任务卡.md) 实施。卡片条目继续作为设计和验收契约；结果只以代码、检查输出和 [doc-handoff/20](../doc-handoff/20-D6-11-15交付记录.md) 的分档证据为准。

### D6-11｜现状复核与 child hook 隔离

**范围**：只读核官方 DSH 当前 HEAD、DreamRunner 及 Hook 生命周期；确认 spawn child 能否拿到指定工具 allowlist、普通 Soul assemble/bundle hooks 是否在 child 上执行、child 的 session header 是否被 L0 排除、取消与续作的可观测边界。

**交付**：更新源码核对交接记录；确认 Dream child context mode 的标识方式；写出只读 tool DTO、服务端认证/lease 检查和预算字段的精确契约。若 DSH 无法将 Dream 专用只读能力限制在 child，先选服务端预构建上下文 packet 的替代方案，不开放普通 memory tools。

**验收**：证据来自当前源码及官方 DSH mock session；不得只凭类型声明。验证主 Agent 正常 Soul/Resident 注入不回归、Dream child 不触发普通上下文污染，或将未解决行为列为阻塞子卡。

### D6-12｜Dream job-scoped 只读数据能力

**范围**：新增/扩展 memoryd job context/read API 与合约 DTO；Rust 端执行认证 scope、job/generation、冻结 evidence IDs、当前 L1/page lifecycle 和版本过滤；返回有限 manifest、证据与搜索结果。不得提供写入或生命周期 tool。

**交付**：协议、存储查询和 adapter client；如需持久化 read snapshot、已读 target 版本或预算账本，新增迁移，不修改既有迁移文件，并同步更新 doc6/02 数据模型、doc6/06 协议、doc6/07 验收矩阵和当前交接状态。公开 HTTP 若不适合给 child 直接调用，改由 DSH adapter 使用同 scope 内部 client，但 Rust 仍是校验端。

**验收**：跨 scope 404、伪 job/generation 拒绝、冻结 evidence 外 ID 拒绝、inactive/retired/stale page 不返回、并发版本变化被标记、分页与输出上限、日志不含正文、工具全只读。

### D6-13｜Dream 多阶段探索与结构化提案

**范围**：版本化 Dream prompt、DSH 子 Agent 的受限只读工具 allowlist、有限的 Orient/Gather/Compare/Propose 轮次、取消和结果提交。保留现有 Rust adjudication/application，不把 LLM 输出改成直接 commit。

**验收**：child 查到已存在同义/互补记忆并提出正确动作；无近邻时可以 create；工具失败时不伪造 no-match；严格引用当前 job user evidence；child 输出的伪 memory/evidence IDs 被 Rust 拒绝；每个提案最终 receipt 可追溯。

### D6-14｜有来源主题整理与索引改进

**范围**：仅在 D6-13 可靠后，将当前主题页生成/更新接入 Compare/Organize 流程；为页面维护 title/description/source versions/generator version；索引以描述和主题为检索入口。问题画像仍只由已登记问题驱动，目录默认空。

**边界**：第一版不改 L1 kind，不新增必填领域类型，不把 topic 页面提升为真相或 system prompt；不要求文件夹层级。是否新增 optional domain/tags 先用离线样本证明收益，再单独决策和迁移。

**验收**：已存在页面更新而非重复创建；不同主题保留分离；source correct/retire/forget/purge/expire 后页面即时不可召回/注入；重建索引不复活旧版本；Resident 只按用户 pin 变化。

### D6-15｜分档质量验收与小范围上线

**范围**：先固定响应与合成数据库验证权限、状态机、receipt 和失败恢复；再由用户配置的真实端点做少量针对性写入与召回质量验证。只允许使用 C:\TRAE\Agent-Memory\模型.txt 列出的模型；串行调用、有界次数，遇到 429/额度/凭据错误停止。

**最小对照集**：新主题、同义重复、对已有记忆补充、相互矛盾的更新、应保留为不同记忆、无可记忆内容、来源失效并发、主题页更新及正确沉默。分别统计 child 看到了哪些目标、提案是什么、Rust 接受/拒绝原因、最终 L1/page 状态、bundle 是否注入、回答是否利用；单次答案变化不等于记忆质量通过。

**交付状态分开报告**：确定性协议测试、官方 DSH child/tool 回路、模型真实连通、语义/整理质量、性能与部署分别记录；不把 mock、child dispatch 或模型 HTTP 200 说成质量验收。

## 9. 验收门槛总表

| 验收项 | 通过条件 |
|---|---|
| 触发 | 普通对话事件只进入 L0；只有持久 trigger/job 能使 child 开始整理。 |
| 受限读取 | child 能读取自己的 frozen evidence 与 Rust 返回的同 scope 候选；跨 scope、过期、退休、已忘记、purge_pending 与 stale 派生页不可见。 |
| 先读后写 | 同义旧记忆被发现时不会盲目重复创建；模型可选择补充、更新、冲突、分开或 defer。 |
| 证据 | 每个 L1 提案能由 Rust 定位到 job input 的 user evidence 和连续 UTF-8 byte span；模型不能自行指定不存在的来源。 |
| 权限 | child 无 DB/文件/任意 HTTP/普通记忆写入/retire/restore/purge/Soul 管理能力。 |
| 原子性 | target/source 版本并发变化后旧 child 输出被拒绝，不能复活被纠错或遗忘的内容。 |
| 派生 | 页面有 source IDs/version 和 generator version；source 失效时 read-time 与 index 路径均不可见。 |
| 注入 | 只有用户编辑 Soul 可作为人格 system context；Resident、L1 与派生知识为带来源的 user-role context。Dream 内部整理输入不能被记成用户新证据。 |
| 质量 | 分开报告提取准确、目标发现、重复/冲突裁决、页面组织、检索命中与正确沉默；允许合理的空输出，不追求生成条数。 |

## 10. 与现有 doc6 决策的关系

- [03](03-人格与常驻记忆.md)：Soul/Resident 权限、pin 语义和 user-role 注入继续有效。
- [04](04-召回与上下文.md)：普通用户 query 的向量召回仍走 TencentDB query-embedding 路径及现有融合；本提案不替换 query-time Recall。
- [05](05-后台整理.md)：来源版本、派生页状态、CAS 发布、即时 stale 和失败恢复继续有效；本提案补的是 Dream child 如何先查已有内容。
- [09](09-语义准入与记忆合并设计.md)：embedding 只召回待裁候选，语义动作仍由版本化模型提议、Rust 校验提交。
- [10](10-写入触发与Dream执行模型.md)：trigger、冻结输入、runner、lease、receipt 和普通 turn/end 不调用 Dream LLM 的边界继续有效。
- [11](11-Prompt注入方案决策.md)：system/user role 不改变。Dream 专用任务和检索结果不得把派生知识提升为人格 Prompt。
- [15](15-人格与派生知识融合方案决策.md)：问题目录保持默认空、topic page 独立于问题画像，Resident 继续由用户 pin 控制。

若本提案与这些已确认规则发生冲突，以用户明确决定和对应已确认设计文档为准；先修本文或通过新决策同步相关文档，再进入代码卡。
