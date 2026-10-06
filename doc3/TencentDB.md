# 审计报告：tencentdb-agent-memory @ feat/server_team (828ac89)

仓库实际结构：无 `MemoryServer` 目录；“服务端”是 `MemoryCore/src/gateway/server.ts`（gateway 模式）。核心链路：MemoryProxy（协议代理/注入）→ MemoryCore gateway（L0 写入 + L1/L2/L3 异步管线）→ 存储后端（TCVDB/Mongo/SQLite + COS/JSONL）。

---

## 1. L0 原始对话持久化

**两条写入路径：**

- **服务模式（HTTP）**：`POST /v2|/v3 conversation/add` → `handleConversationAdd`（`MemoryCore/src/gateway/v2-router.ts:664-822`）。每条消息生成新 UUID `msg-<32hex>`（v2-router.ts:722），带 team/user/agent/task 隔离字段（727-740），先 `store.insertL0Batch / upsertL0` 写入 VDB（749-758），**之后**才 `deps.notifyPipeline` 触发异步 L1（761-774），且 notify 失败仅 warn：“L0 is already persisted, pipeline will catch up later”（769-771）。standalone 下再镜像一份 JSONL（781-811，失败非致命）。
- **插件/钩子模式**：`performAutoCapture`（`MemoryCore/src/core/hooks/auto-capture.ts:46-347`）→ `recordConversation`（`MemoryCore/src/core/conversation/l0-recorder.ts:93-314`）写每日分片 JSONL `conversations/YYYY-MM-DD.jsonl`（293-306；COS 走 `storage.appendFile`），随后 L0 向量化（auto-capture.ts:173-298，sqlite 支持后台补嵌），最后 `scheduler.notifyConversation`（305-307）。

**分窗/分批**：L1 读取时按 checkpoint 游标批量拉取，`L1_BATCH_PROCESS=10`、`L1_BATCH_QUERY=20`（`MemoryCore/src/utils/pipeline-factory.ts:89-90`），同毫秒边界对齐防丢（523-529），批满即再入队 drain（server.ts:2943-2960）。

**证据先于提取？** 是——上面两条路径都是先写 L0 再通知管线。但有两个证据丢失/重复窗口：

1. **L0 写失败仍推进游标（丢证据）**：`recordConversation` 写 JSONL 失败时只打 error 并 `return filtered`（l0-recorder.ts:308-311，注释“Return filtered messages anyway so L1 can still process them”）；`performAutoCapture` 随后仍返回 `maxTimestamp` 让 `captureAtomically` 推进游标（auto-capture.ts:139-140）。该批消息此后不会再被重捕。
2. **重试会重复写 L0**：MemoryProxy 侧 `pending-writes.ts` 头注释明确：L0 重试 `withL0Retry` 在 kernel 已收到但客户端超时时会写两条，“tdai `/v3/conversation/add` 目前没有 idempotency-key……宁可重复也不要丢”（`MemoryProxy/src/tdai/pending-writes.ts:1-21`）。SIGTERM flush 见同文件 `flushPendingWrites`。

**L1 不重复提取靠游标**：checkpoint 记 `last_l1_cursor`，L1 完成后 `checkpoint.markL1ExtractionComplete(sessionKey, totalStored, maxRecordedAtMs, …)`（pipeline-factory.ts:655-658）；跨节点用分布式锁保护 checkpoint（`server.ts:2841-2886`，注释指出锁丢失会“L1 游标丢失并永久重复抽取”）。崩溃发生在“写入成功但游标未推进”之间时，重试会重新抽取同一批 L0——依赖后续 batchDedup 收敛（见第 4 节）。

---

## 2. 模型如何抽取事实（l1-extraction.ts, chat 模式）

- **类别**：chat 模式仅三类 persona/episodic/instruction（`MemoryCore/src/core/prompts/l1-extraction.ts:15-16`），code/team 模式为 work_fact/work_task/work_method/work_artifact（:105, :184-186）。代码侧 `VALID_TYPES` 兼容两套并折叠 legacy 类型（`MemoryCore/src/core/record/l1-extractor.ts:808-820`）。
- **原子化要求——方向相反**：chat prompt 明确“归纳合并：强关联或因果关系的多条消息，必须合并为一条完整记忆，不可碎片化”（l1-extraction.ts:35）；team 模式“不要把同一个工作结论拆成多个碎片。但不同工作对象、不同任务、不同方法论应分开提取”（:167-171）。即拆分粒度由模型按“工作对象”判断，**没有“一条消息多个事实必须拆成多条”的指令**；输出结构上每情境 `memories` 数组可含多条，每条带 `source_message_ids`（:76-85, :344-351），后处理无任何拆分逻辑——解析只是扁平化（l1-extractor.ts:223-240）。
- **逐字引用要求——没有**：`source_message_ids` 只要求填消息 ID（l1-extraction.ts:81, :347）；`content` 要求按“提取句式”改写（如“用户（[姓名]）喜欢/是/擅长...” :42，episodic 句式 ：48），全仓库 prompts 目录 grep `quote|原文|逐字` 无逐字引用要求（仅 offload 的 L1 summary prompt 有“对原文的可替代性”打分，`offload_server/prompts/l1-prompt.ts:27`）。
- **防漏提/防补全**：防漏——无 few-shot（prompt 无示例对话，仅类型示例句）；“宁缺毋滥：过滤琐碎闲聊……剔除不可靠的边缘信息”（:33）。“只从新消息提取”+“严禁从背景消息中新增提取记忆；source_message_ids 必须只包含【待提取的新消息】中的 message id”（:172-175，user prompt 再次强调 l1-extraction.ts:410-416）。**没有“不确定就不输出”类指令**；代码层面也没有置信度阈值——LLM 返回的全部入库，仅限流 `maxMemoriesPerSession=10` 截断（l1-extractor.ts:152, 283-288）和无效 type 丢弃（:227-230）。
- **时间信息**：episodic 要求“尽量基于消息的 timestamp 推算绝对时间……输出 activity_start_time / activity_end_time（ISO 8601），无法确定时可省略”（l1-extraction.ts:49, :89）；team 模式 work_task 可填 owner/deadline/status、work_fact 可填 activity_start/end（:243-246, :359）。user prompt 中消息以 `[id] [role] [ISO 时间]` 逐条呈现并要求“务必结合 timestamp 推算时间”（:402-416）。合并时 `merged_timestamps` 取并集（`prompts/l1-dedup.ts:46-47, :109-111`）。

---

## 3. 候选如何获得可用地位（裁决方）

- **冲突检测/合并 = 模型裁决**：`batchDedup`（`MemoryCore/src/core/record/l1-dedup.ts:56-135`）两阶段：先混合召回候选（native hybrid 或 FTS∥vector+RRF，:118-131），再一次 LLM 批量判定 `store|update|merge|skip`（:140-200；prompt 在 `prompts/l1-dedup.ts:16-70`）。解析失败/无候选时**默认全部 store**（l1-dedup.ts:85-90, :128-131, fallbackStoreAll :417-423；l1-extractor.ts:361-363 dedup 异常也直接全存）。写侧按决策执行：update/merge 删旧 VDB 记录、append 新记录；skip 直接丢弃（`record/l1-writer.ts:184-186, 275-310`）。
- **来源归属 = 代码/请求上下文**：记录落 `teamId/userId/agentId/sessionId`（l1-writer.ts:233-241）；候选召回按调用方 filter 隔离，“dedup never crosses tenants”（l1-dedup.ts:73-74, l1-extractor.ts:323）。
- **敏感信息 = 仅 prompt 软约束**：chat prompt 无敏感词指令；team prompt 仅“不提取……私人生活或敏感信息”（l1-extraction.ts:155, :330）。**在 MemoryCore/MemoryProxy 源码中未找到任何敏感词扫描/PII 正则/准入闸门**（搜索关键词：`敏感|sensitive|pii|身份证|手机号`，仅命中日志脱敏 `metadata/utils/user-key.ts:9` 与 `gateway/error-handler.ts:160-172` 的字段掩码）。`sanitize.ts` 只有标签/噪声清理和 prompt-injection 正则 `looksLikePromptInjection`——且在 `shouldExtractL1` 中**被注释掉未启用**（`MemoryCore/src/utils/sanitize.ts:150-153`，仍保留于 ：180-221）。
- **时效**：无 TTL/有效期字段；只有保留期清理（见第 4 节）。
- **人工确认 = 无**：写入路径没有用户确认队列/审批流（搜索 `pending|confirm|review` 仅命中 skill 侧 `MemoryProxy/src/mem-command/pending-store.ts` 与 skill-review，与 L1 记忆无关）。唯一的准入门是：代理侧配置总开关 `isExtractionAllowed`（`MemoryProxy/src/extraction-gate.ts:35-44`）和配额检查（server.ts:2908-2915）。

---

## 4. 纠错、遗忘、合并、去重与重放

- **delete**：`/atomic/delete` 按 id 删 L1（`v2-router.ts:1293-1342`），`/conversation/delete` 删 L0（:1000-1054），`/v3/chat-memory/clear` 按 (team,agent) 整体清 L0/L1/L2/L3+文件（`gateway/chat-memory-handlers.ts:193-219`）。**没有 "forget/supersede" 命名语义**（grep `forget|supersede` 无命中）；“纠错”即 dedup 的 update（“新记忆……或纠错，以新记忆为主覆盖旧记忆”，`prompts/l1-dedup.ts:37`）。
- **合并/去重键**：去重不是内容 hash，而是“混合召回候选 + LLM 判定同一事实”；记录主键 `record_id`（`m_<ts>_<rand>`，l1-writer.ts:148-150），VDB upsert by record_id 幂等（pipeline-worker.ts:14, :1244）。版本号 update/merge 自增 `version`（l1-writer.ts:191-200, 232；v2-router.ts:1084-1096）。
- **复活风险**：
  - L1 删除后 L0 证据仍在（JSONL append-only“旧记录仍留文件，由 memory-cleaner 周期清理”，l1-writer.ts:8-11, 276-278）；正常情况下 L1 游标已越过，不会重放；但 **checkpoint 丢失/回退时会从 L0 重新抽出已删记忆**（server.ts:2849-2850 注释：“后写者用旧快照覆盖先写者……导致 L1 游标丢失并永久重复抽取”）。
  - JSONL 属 append-only，`update/merge` 只删 VDB 行；清理器 `LocalMemoryCleaner` **只按文件名日期整体过期删除**（`utils/memory-cleaner.ts:227-277`，注释自述 reconcile 逻辑并未实现），不会按 VDB 状态逐行修剪。若有代码路径从 JSONL 重建 L1（seed/reindex），旧版本行可被重放（`l1-reader.ts` 读 VDB 为主，重建路径未在本分支发现直接调用）。
- **审计日志**：`store.appendAudit`——update/delete 每条记一行（v2-router.ts:184-198, :1112-1120, :1324-1334），clear 按 L1/L2/L3 各记一条 delete 事件、“不保留任何原内容”（chat-memory-handlers.ts:9-12, :333-365）。审计失败不阻塞主流程（v2-router.ts:175-182）。

---

## 5. 检索与注入（MemoryProxy）

- **管线**：`InjectionPipeline.process` = adapter.parse → agent profile 识别 → 按 `system.prefix/before_tools/after_tools/suffix, tools.*, user.first_turn/before/after` 顺序执行 hooks → serialize（`MemoryProxy/src/injection/pipeline.ts:80-147, :164-175`）；hook 失败非致命继续（:229-250）。缓存策略 none/session_init/hybrid + self-heal（:269-328）。
- **注入层次与路径差异（当前分支）**：
  - **L3 persona 全文直注**：`TdaiProfileMemoryInjector`（`injectors/tdai-profile-memory-injector.ts:25-144`），point=system.suffix + anchor，`cacheStrategy="session_init"`；L3 截断 6000 字符（:107），**L2 只注入路径+summary 索引** `<l2_scene_index>`（:109-121），正文靠 `tdai_read_scene` 工具拉。即使 L2/L3 全空也注入 `MEMORY_TOOLS_GUIDE`（:83-90）——即**画像层“空也注入工具说明”，非空则全文始终注入，与查询无关**。
  - **L0/L1 不再每轮自动注入**：`injection/index.ts:347-349` 明确注释“L0/L1 不再每轮自动召回注入到 user prompt（会破坏 KV/prompt cache）……L1 recall injector 已下线，recallL1 配置保留但不再注册”。`TdaiL1RecallInjector`（`tdai-l1-recall-injector.ts:21-120`）仅导出、无注册点（grep 全目录只有 export）。改为 `TdaiToolsInjector` 在 system 里教 LLM 用 Bash curl `memory-bridge/v3/atomic/search` 等只读工具（`tdai-tools-injector.ts:62-97`），每轮 ≤3 次调用限制写在指南里（tdai-profile-memory-injector.ts:207）。
  - 插件（非 proxy）路径仍有自动召回：`MemoryCore/src/core/hooks/auto-recall.ts`——L1 用 keyword/embedding/hybrid(FFS+cosine RRF) 检索 prepended 到 user prompt，L3 persona + L2 scene navigation appended 到 system（文件头注释 1-11）。
- **排序**：`/atomic/search` → `executeMemorySearch`（`v2-router.ts:1204-1228`），混合召回统一走 `recallL1Candidates`（`core/tools/l1-candidate-recall.ts:14-60`）：TCVDB native hybrid 优先，否则 FTS BM25 ∥ client 向量 cosine 用 RRF(k=60) 融合（`core/store/search-utils.ts:18-62`）。**没有 priority/recency 加权**——priority 只在展示行输出（`core/tools/memory-search.ts:194-195`）。代理侧多 agent 合并按 score 降序取 globalTopK=5（tdai-l1-recall-injector.ts:86-89，虽已下线，memory-bridge 聚合同理）。
- **预算控制**：L3 截断 6000 字符、L2 summary 截 200（tdai-profile-memory-injector.ts:107, 115）；auto-recall 截断行加“…（已截断；可用 tdai_memory_search …）”（auto-recall.ts:27）。
- **错误召回控制**：检索词用“干净 user_query”而非整条消息（tdai-l1-recall-injector.ts:51-55）；命中结果标注 `[type][from agent][score]` 供模型自行判断（:104），并声明“仅用于辅助回答当前这一轮，不要视为永久系统规则”（:95）；XML 注入转义 `escapeXmlTags`（sanitize.ts:288-294）。无 score 阈值过滤。

---

## 6. 用户 / Agent / 团队隔离与 ACL

- **数据模型**：metadata SQLite 表 `meta_users / meta_user_keys / meta_teams / meta_team_members / meta_agents / meta_tasks / meta_task_agents / meta_assets / meta_agent_fixed_assets / meta_asset_acl` 等（`MemoryCore/src/metadata/store/sqlite-adapter.ts:132-317`）；Mongo 侧对应 `metadata/store/mongodb-adapter.ts`。
- **ACL 判定**：`checkPermission`（`metadata/service/permission-checker.ts:43-139`）顺序：资源存在 → owner 全允许（:54-57）→ 必须是 active 团队成员（:60-63）→ visibility：`private` 严格仅 owner（:67-82）、`restricted` 仅显式 ACL（:83-100）、`task` 仅读（:102-107）→ 角色默认（admin=rw，member=只读，:117-121）→ 显式 ACL 三类主体 user/team_role/agent（:124-135）。`canBindAsset` 管绑定（:155-172）。
- **记忆数据隔离**：三维 tenancy `(teamId, userId, agentId)` 贯穿 L0/L1（`store/types.ts:141-161`、`store/isolation.ts:20-96` 强制 userId/agentId 非空）；写入时缺省落 `DEFAULT_ISOLATION_ID`（l0-recorder.ts:282-283, l1-writer.ts:240-241）；`/conversation/add` enforce 模式下缺隔离头直接 422（v2-router.ts:673-680）。L2/L3 输出按 profile scope `profiles/team:T|agent:A/` 目录隔离（pipeline-factory.ts:112-139；chat-memory-handlers.ts:127-148 清空也按该前缀）。
- **同一用户多 Agent 默认隔离**：是——L1 检索 filter 显式带 `agentId` 且**不带 sessionId**（跨 session 但不跨 agent，v2-router.ts:1211-1221）；跨 agent 共享只能通过“固定资产绑定（同 team，最多借入 2 个 agent）”显式开启：`resolveFixedAssetCtxs`（`tdai-fixed-asset.ts:49-126`，items.slice(0,2) :115），检索工具侧 “绑定了多个 chat_memory 会默认同时检索 self + imported”（tdai-tools-injector.ts:76）。

---

## 7. 异步作业（pipeline-worker 等）

- **队列/消费组**：`IStateBackend` 工厂 `createStateBackend`（`core/state/index.ts:49-94`）：`local`（内置 `LocalStateBackend`）或 `redis`（**Redis Stream + XREADGROUP consumer group，实现位于私有子模块 `src/integrations/redis/`，本仓库内不存在**，index.ts:54-64 明确“private submodule”）；service 模式默认 redis（server.ts:1720-1726）。
- **锁**：分布式锁 per-(instance,team,agent) hash-tag 分桶，L1 session 级、L2/L3 agent 级（`pipeline-worker.ts:1073-1152` getLockKey）；锁 TTL 600s、续约 30s，续约失败置 lockLost 并 abort executor 的 AbortSignal（:587-630），ACK 前校验 PEL owner（`ackTaskIfOwned` :966-980）。
- **重试/退避**：任务失败 `maxRetries=3`，指数退避 `retryBaseDelayMs * 3^retryCount` = 5s/15s/45s（pipeline-worker.ts:686-698，注释 ：13 “5s/15s/45s”）；锁冲突不 ACK 不重投，进入进程内 parked 队列按 scope FIFO 退避 `[200,600,1800,5000]ms` ±20% jitter（:226-227, 757-783）。
- **死信**：超上限进 `moveToDeadLetter`——**先**持久化回调 `onDeadLetter` 再 ACK（:1158-1174）；但 **server.ts 未注入 onDeadLetter**（grep server.ts 无命中；`new PipelineWorker(...)` 传参表 ：1962-1983 无该项），所以 service 模式死信仅存进程内 `deadLetterQueue` + metrics + `obsLogger.error("core.task.dead_letter", …)`（:184-185, :1189-1196）。**dead 后无自动恢复**，只清理该 session 的 L1/L2 timer 防 ghost 触发（:1182-1187）。恢复途径只有死信前的 PEL stale 回收：`startPendingRecovery` 每 30s `claimStaleTasks`（XAUTOCLAIM 语义，:1246-1304，pendingStaleMs 自动抬升至 lockTtl+2×renew，:223-224）。
- **Prompt 版本持久化**：不在任务 payload 里，而是随每次生成写 provenance：`MemoryGenerationLog` 含 `prompt: {memory_prompt_id, memory_prompt_version, source, prompt_sha256}`（`core/memory-generation-log/types.ts:48-49`；`store.ts:49-57` builtin prompt 记 `builtin:<layer>` v1），input_refs 指向 L0 消息 id、output_refs 指向产出的 L1 记录 id（l1-extractor.ts:371-406；L2/L3 在 pipeline-factory.ts:887-926, :1068-1106），并 best-effort upsert 到 VDB generation refs。
- **超时**：LLM 调用硬超时 180s（l1-extractor.ts:518/534，l1-dedup.ts:167/183）；worker 侧无单独任务级超时，靠 abort signal + pendingStale 回收。

---

## 三个具体问题

**A. 平叙事实能否被提取？——能，在 prompt 层。**
chat 模式的 `episodic` 定义即“客观发生的动作、决定、计划或达成结果。绝不包含纯主观感受”，提取句式为“用户在[时间]于[地点]做了某事”（l1-extraction.ts:46-50），不要求“以后/我喜欢”句式——那些触发词（“以后都、从现在开始” :55）只属于 `instruction` 类。team 模式 `work_fact` 同样覆盖平叙事实（“项目目标/决策结论/当前状态/实验结果…”，:187-215），且 :164-165 明确要求把“某人建议”与“团队决策”区分表述。代码层无句式过滤：`shouldExtractL1` 只做结构/噪声过滤（sanitize.ts:135-156），LLM 输出的所有合法 type 都入库（l1-extractor.ts:223-240）。

**B. 一条消息多个事实拆成多条？——无强制拆分机制，且 prompt 明确反对碎片化。**
没有“必须拆成原子记忆”的指令：恰恰相反，“必须合并为一条完整记忆，不可碎片化”（l1-extraction.ts:35；team 版 ：167-171）。拆分只发生在模型认为“不同工作对象/不同任务”时。输出侧 `memories` 数组天然支持多条，每条带各自的 `source_message_ids`（l1-extraction.ts:76-85），代码只扁平化透传（l1-extractor.ts:223-240），无后处理分割。**逐字引用无从保持**：prompt 无逐字 quote 要求（grep `quote|原文|逐字` 在 prompts/ 与 offload prompts/ 均无命中，唯一相关是 offload l1-prompt.ts:27 的“对原文的可替代性”打分），`source_message_ids` 仅是 ID 引用，content 是改写后的陈述句。

**C. 零词法命中时非指令记忆仍能注入？——分路径。**
- **始终注入层（不依赖检索）**：L3 persona 全文走 `session_init` 缓存直注 system（tdai-profile-memory-injector.ts:32, :105-108）——与查询零重叠也注入；L2 以 path+summary 索引形式全量注入（:109-121）。这是“画像层兜底”，**不依赖 embedding**。插件路径 `auto-recall` 亦直注 L3 persona + L2 scene navigation（auto-recall.ts 头注释 4-11）。
- **查询相关层**：L1 检索为 FTS BM25 ∥ 向量 cosine + RRF（l1-candidate-recall.ts:26-60, search-utils.ts:38-62），**依赖 embedding 服务/向量库**（sqlite vec0、TCVDB dense+sparse 或 server-side embedding，`store/types.ts:596-598`）；零词法命中时向量语义召回仍可命中（仅当 embedding 不可用才退化为纯 FTS，l1-dedup.ts:103-131）。**没有 recency 兜底**：/atomic/search 结果只按 RRF/hybrid 分数排序，无时间加权（v2-router.ts:1279-1288）。另注意：在 MemoryProxy 注入主路径上 L1 自动注入已下线（injection/index.ts:347-349），L1 现在依赖 LLM 主动调 search 工具，零命中时指南要求明确说“没找到”（tdai-profile-memory-injector.ts:208）。

---

### 主要证据文件清单
- `MemoryCore/src/core/conversation/l0-recorder.ts`、`core/hooks/auto-capture.ts`、`gateway/v2-router.ts`（L0 写入）
- `core/prompts/l1-extraction.ts`、`core/prompts/l1-dedup.ts`、`core/record/l1-extractor.ts`、`core/record/l1-dedup.ts`、`core/record/l1-writer.ts`
- `utils/pipeline-factory.ts`、`utils/checkpoint.ts`（经 server.ts 引用）、`utils/memory-cleaner.ts`、`utils/sanitize.ts`
- `services/pipeline-worker.ts`、`core/state/index.ts`、`gateway/server.ts`
- `MemoryProxy/src/injection/pipeline.ts`、`injection/index.ts`、`injection/injectors/tdai-profile-memory-injector.ts`、`tdai-l1-recall-injector.ts`、`tdai-tools-injector.ts`、`tdai-fixed-asset.ts`、`tdai/pending-writes.ts`、`extraction-gate.ts`
- `metadata/service/permission-checker.ts`、`metadata/store/sqlite-adapter.ts`、`core/store/isolation.ts`、`core/store/types.ts`、`core/store/search-utils.ts`、`core/tools/l1-candidate-recall.ts`、`core/memory-generation-log/{types,store}.ts`

### 在搜索范围内未找到的机制
- L1 写入前的敏感词/PII 扫描、置信度阈值准入、用户确认队列（关键词：sensitive/敏感/pii/threshold/confidence/confirm/review/approval，MemoryCore+MemoryProxy src 全量）。
- 命名为 forget/supersede/tombstone 的遗忘语义；基于 VDB 状态的 JSONL 逐行 reconcile 清理。
- Redis Stream 后端实现体（`src/integrations/redis/` 为私有子模块，仓库内不存在）；服务模式死信的持久化回调接线（`onDeadLetter` 在 server.ts 无注入点）。