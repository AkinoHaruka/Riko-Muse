# EverOS 只读源码审计报告（HEAD 462ebf9，main）

所有路径相对 `C:\TRAE\Agent-Memory\EverOS\`。以下全部结论出自源码，docs 仅作对照线索。

---

## 1. 原始对话如何持久化

**入口与归一化**
- `src/everos/service/memorize.py:181` `memorize()` 是唯一入口（`/add`、`/flush` 都走它）；`src/everos/entrypoints/api/routes/memorize.py:183/201` 定义 `POST /add` 与 `POST /flush`，`/add` 支持 `defer_extraction`（routes/memorize.py:138-142），`/flush` 以 `is_final=True` 调用（routes/memorize.py:219）。
- 先做 ingest 归一化：`src/everos/memory/extract/ingest/service.py:42` `process()` 把 payload 变成 `CanonicalMessage` 列表，`app_id/project_id` 缺省 `"default"`（service.py:49-50），`message_id` 由 `gen_message_id(session_id, ts_ms, idx)` 生成（service.py:67）。
- **第一落盘是 SQLite `unprocessed_buffer`**：`src/everos/service/_boundary.py:136-140` `prepare_cells()` 先 `unprocessed_buffer_repo.list_for_track()` 读出缓冲，与新消息 `_merge_dedupe_sort()`（按 message_id 去重、(timestamp, message_id) 排序，_boundary.py:442-450）。无消费标记，整段原子重写：`src/everos/infra/persistence/sqlite/repos/unprocessed_buffer.py:47-68` `replace()` 是"单事务 delete-then-insert"。defer 模式走 `buffer_messages()`（_boundary.py:96-116），只写缓冲不做 LLM。

**boundary → MemCell**
- boundary 检测是 LLM 调用（`everalgo.boundary.detect_boundaries` / `AgentBoundaryDetector`，_boundary.py:32-33、242-287）；chat 模式过滤掉 tool 行（_boundary.py:227-233）。
- 切出 cell 后：每个 cell 铸一个 `mc_` id（_boundary.py:522-524），写一行 SQLite `memcell` 台账（_boundary.py:199 `memcell_repo.insert_many(rows)`；行结构 `_build_memcell_row` _boundary.py:497-519，`payload_json=cell.model_dump_json()` 存完整 MemCell 原文）。MemCell 本体来自外部包 `everalgo.types.MemCell`（_boundary.py:37）。
- 尾部消息（tail）写回缓冲（_boundary.py:211-212），保证未切分的对话不丢。

**MemCell vs Episode**
- MemCell（SQLite `memcell` 表，`src/everos/infra/persistence/sqlite/tables/memcell.py:23`）：owner-agnostic 的原始对话切片 + message_ids/sender_ids/payload_json，是长期归档（`extract_user_profile.py` docstring 明说 "sqlite memcell.payload_json column is the long-term archive that lets us replay this beyond unprocessed_buffer's lifetime"，`src/everos/memory/strategies/extract_user_profile.py:52-54`）。
- Episode（Markdown 日志 + LanceDB 派生行）：`src/everos/memory/extract/pipeline/user_memory.py:135-147` 每个 cell 一次 LLM 生成叙事，按 user sender fan-out（`_unique_user_senders` user_memory.py:222-236），`EpisodeWriter.append_entry` 写 md（user_memory.py:159-165）；md entry inline 带 `owner_id/session_id/timestamp/parent_type=memcell/parent_id/sender_ids`（`_episode_to_entry_body` user_memory.py:264-272），Content section 存叙事全文（user_memory.py:292）。

**失败重试是否丢证据**
- boundary 阶段：`_detect` 对 ValueError（LLM JSON 解析失败）重试 3 次（_boundary.py:239、254-287），重试前后**缓冲内容未被销毁**——只有成功切出 cell 后才 `_replace_buffer(tail)`；若 boundary 失败抛异常，merged 消息仍在内存中未落盘回缓冲——这里有一个窗口：`prepare_cells` 在 `_detect` 之前**没有**先写回缓冲，异常抛出时新消息只存在于请求内存中，依赖客户端重发（这是真实的边界情况，非"已保证不丢"）。
- 无 LLM client 时安全降级为整段回写缓冲（_boundary.py:151-157）。
- Episode 抽取重试：`_extract_with_retry` 对 ValueError 重试共 3 次尝试（user_memory.py:41 `_EXTRACT_MAX_RETRIES = 2` + user_memory.py:296-317），最终失败异常上抛 → 请求失败，但 memcell 行已持久化，原始消息证据在 sqlite。
- 每会话锁 + 超时：`memorize()` 用 per-session lock 包整个读-合并-切分-写周期，`asyncio.timeout(session_lock_timeout_seconds)`（默认 360s，`src/everos/config/settings.py:445`）防止 LLM 卡死丢锁（memorize.py:197-237）。

---

## 2. 模型如何抽取 atomic facts

- 策略本体：`src/everos/memory/strategies/extract_atomic_facts.py:38-85`。由 `EpisodeExtracted` 事件触发（`Immediate(on=[EpisodeExtracted])`，line 40），调 **外部包** `everalgo.user_memory.AtomicFactExtractor.aextract_from_text(event.episode_text, timestamp=...)`（line 47-50），批量 `append_entries` 写 md（line 75-77）。
- **prompt 不在本仓库**：本仓库可配置的 prompt slot 只有 `src/everos/config/prompt_slots/boundary_detection.yaml` 和 `episode_extract.yaml`（目录列举确认）；`PromptLoader.load()` 只覆盖这两个名字（`src/everos/memory/prompt_slots/loader.py:39-56`），slot 禁用/缺失时返回 None "use algo default"（loader.py:20-23）。atomic facts / profile / foresight 的 prompt 全在 PyPI 外部依赖 `everalgo-user-memory==0.4.0`（`pyproject.toml:90-93`）。**因此"是否要求逐字原文引用、如何避免漏提与无依据补全、时间信息如何处理"这些 prompt 细节在搜索范围内未找到**（全仓 grep `verbatim|逐字|quote|原文` 仅命中无关文件）。
- 拆分：一条 episode 的多个事实 = `aextract_from_text` 返回列表，逐条构造 `AtomicFact.from_algo(...)`（extract_atomic_facts.py:62-70）——拆分由外部 everalgo 的 LLM 抽取器完成，everos 侧只负责逐条落盘。
- 引用保持：fact 的 `parent_id = event.episode_entry_id`（extract_atomic_facts.py:67），md inline 写 `parent_type:"episode", parent_id`（extract_atomic_facts.py:101-102）；检索侧明确支持新旧两代 parent 语义（"New facts (post-1.5): parent_id = episode_entry_id; Old facts (pre-1.5): parent_id = memcell_id"，`src/everos/memory/search/recall/atomic_fact.py:8-13`）。
- **分工**：
  - `extract_foresight.py:76-83`：per-memcell、per-sender，**默认关闭**（`enabled=False`，line 81；docstring line 15-19 说明"没有任何检索路由消费它"），输出带 `Evidence` section（line 154）。
  - `extract_user_profile.py:1-58`：双触发（cluster 路径 Tier2 / direct 路径 Tier1 无 embedding），从 cluster/memcell 合成单行用户画像，写 `user.md`；节流 `profile_extraction_interval`（docstring line 41-45）。
  - `extract_agent_case/extract_agent_skill/reflect_episodes` 走 agent 侧与合并侧（`_STRATEGIES_REQUIRE_EMBED`，memorize.py:123-128，各自 body-guard embedding 能力）。

---

## 3. 候选/记忆如何获得可用地位

**分层，写入即进入检索面，无人工闸门：**
- 层次：SQLite buffer → MemCell 台账 → Episode/AtomicFact/Profile 等 **Markdown（真相）** → cascade 同步进 LanceDB 索引（`src/everos/memory/cascade/handlers/_daily_log_base.py:1-22` 三方 reconcile：hash 变化→tokenise+embed+upsert；无变化→skip；md 中消失→delete，line 99-145）。
- Episode 写入 md 后立即 emit `EpisodeExtracted`（user_memory.py:179-191），atomic facts 等策略并行执行；cascade worker/watcher 把 md 变成 LanceDB 行（`src/everos/memory/cascade/worker.py`、`watcher.py`）。检索只读 LanceDB（`src/everos/memory/search/manager.py:25` "The manager never writes to storage"）。
- **没有内容准入闸门**：全仓 grep `sensitive|pii|moderation|redact|approval|confirm` 只命中遥测 redaction hook（`src/everos/core/observability/tracing/attributes.py:46-64`，仅用于 span 内容脱敏）——**没有敏感信息过滤、冲突裁决或人工确认机制**。OME 的"gate"（`src/everos/infra/ome/gates.py:14-52` Counter；`src/everos/infra/ome/_dispatch/dispatcher.py:100-115` enabled/applies_to/Counter 三门）是**调度频率闸门**，不是内容闸门。
- 冲突/时效的裁决是**事后**的：靠 Reflection 合并 + `deprecated_by` 软归档（见第 4 节）与查询侧 `deprecated_by IS NULL` 过滤（见第 6 节）。时效只体现为检索参数 `radius/min_score`（dto.py:95-103）和 reflection 的 cluster 更新模式。

---

## 4. 纠错、遗忘、合并与重放

**deprecated_by 软归档实现**
- 字段：`src/everos/infra/persistence/lancedb/tables/episode.py:77`（"Soft-delete marker set by Reflection... NULL means active"）；atomic_fact 表同样有（`src/everos/infra/persistence/lancedb/tables/atomic_fact.py:62`）。
- 写入方：`src/everos/memory/reflection/orchestrator.py`
  - `_deprecate_lance_episodes()` line 838-866：LanceDB `update({"deprecated_by": merged_entry_id}, where=entry_id+owner+scope)`；
  - `_deprecate_lance_facts()` line 868-900：按 `parent_id IN (被合并 episodes)` 批量标记；
  - md 侧：`_patch_md_frontmatter()` line 989-1017 把 `deprecated_entries: {entry_id: merged_entry_id}` 写进 episode md frontmatter；
  - 整个 deprecation 在分区锁下执行（`_deprecate` line 677-679，`get_partition_lock("reflection_deprecate", f"{app}:{proj}:{cluster}")`）。
- 查询侧排除：`src/everos/memory/search/filters.py:66-67` `if owner_type == "user": base.append(is_null("deprecated_by"))`；repo 层 `src/everos/infra/persistence/lancedb/repos/episode.py:60,146` `AND deprecated_by IS NULL`；Milvus 后端同样（`src/everos/infra/persistence/milvus/repos.py:39,61`）。

**旧条目会否被重放复活**
- Episode：md frontmatter 的 `deprecated_entries` 是真相，cascade 每次同步都会重放标记：`_daily_log_base.py:216-239` `_propagate_deprecations()`（"The md file is the source of truth; cascade reconstructs the deprecated_by column on every sync/rebuild"，line 225-227）→ 重建后依然被软删，**不会复活**。
- **Atomic facts 有一个真实的复活缝隙**：Reflection 只 patch episode 的 md frontmatter（orchestrator.py:989-1017 只用 `self._episode_writer`），`_deprecate_lance_facts` 只改 LanceDB 行；而 `AtomicFactHandler._build_row`（`src/everos/memory/cascade/handlers/atomic_fact.py:57-90+`）构造的行**不携带 deprecated_by**，fact md 里也没有 deprecated 标记。若 atomic_fact 表从 md 全量重建（`.index/` 删除或行被删除后重挂），deprecated facts 会以 `deprecated_by=NULL` 复活。（日常 diff 同步不会复活——deprecated 行未从 md 删除，diff 只按 content_sha256 跳过或删除 md 中消失的行，_daily_log_base.py:147-162、206-214。）

**Reflection 是否保留原始 episode**
- 保留：软删而非物理删除；被合并成员从 cluster 移除、merged 加入并重算质心（`_update_cluster_after_merge` orchestrator.py:902-932）；md 原文不删除，只加 frontmatter 标记。
- 审计：`_create_reflection_report` 写 SQLite `ReflectionReport`（orchestrator.py:934-987，含 mode/source_members json/merged_entry_id/deprecated_fact_count；表定义 `src/everos/infra/persistence/sqlite/tables/reflection_report.py:11`）。合并前的重新抽取用 `ctx.emit(EpisodeExtracted(source="reflection"))` + `ctx.wait_for_event(timeout=120s)`（orchestrator.py:452-501，`_WAIT_TIMEOUT_SECONDS = 120.0` line 42），超时则放弃本次合并（merged episode 已写但返回 None，下次 `_detect_orphans` 会警告孤儿 line 505-534）。

---

## 5. 检索与注入

**keyword 检索算法**
- 是 **BM25 / LanceDB FTS**，不是自研打分：`EpisodeRecaller.sparse_recall`（`src/everos/memory/search/recall/episode.py:48-68`）先分词（`self._deps.tokenizer.tokenize(query)`），无词返回 `[]`（line 59-61）；OR-mode SHOULD 子句（docstring line 51-58，防止单个 IDF≈0 的词毒化整查询）。
- 底层：`src/everos/infra/persistence/backends/lancedb.py:166-203` `sparse_search` 对每个 BM25 字段执行 `table.query().nearest_to_text(build_or_query(terms, field))`，跨字段取每行最高 `_score`。FTS 索引建在 **app 层预分词列** 上：`episode_tokens`（jieba 空格拼接，`src/everos/infra/persistence/lancedb/tables/episode.py:66-72`，`BM25_FIELDS = ["episode_tokens"]` line 32）。
- **embedding 可选，keyword 可独立工作**：`src/everos/service/search.py:8-18` docstring + 实现——LLM/embedding/rerank 均可缺省，`_get_llm_client()` 吞掉 `LLMNotConfiguredError` 返回 None（search.py:55-75）；`SearchManager._validate_components` 只对 VECTOR/HYBRID/AGENTIC/LLM_MULTIROUND 要求 embedding（manager.py:869-884），KEYWORD 不要求。
- 排序/预算：KEYWORD/VECTOR 是单路 recall 直接截断（manager.py:404-426）；recall pool = `top_k * 2`，`top_k=-1` 上限 100（manager.py:107-108、804-821）；HYBRID 走 RRF → fact 挂接 → heap-expand + fact eviction（manager.py:433-457），`min_score` 事后过滤（manager.py:455-456）；VECTOR 走 MaxSim（atomic_fact ANN → 按 parent max-pool → 取回 episode，manager.py:692-730）；无限模式默认 cosine radius 0.5（manager.py:122、987-1004）。

**指令/画像/事实路径差异**
- 硬分区：`user` → episodes(+profiles)，`agent` → cases+skills（manager.py:223-243；dto.py:5-9）。
- 画像类：`ProfileRecaller.fetch` 是**无查询相关的 KV 按 owner 直取**，`include_profile=true` 即返回，score=None（manager.py:628-631；`src/everos/memory/search/recall/profile.py:32-60`）——与事实类的打分检索完全不同路。
- 事实类（atomic facts）不是独立返回项：只嵌在 HYBRID episode 结果里（`SearchEpisodeItem.atomic_facts`，dto.py:9-10、181；manager.py:20-23 说明其他方法为空的原因）；KEYWORD/VECTOR 单路结果明确不回填 facts（manager.py:417-421）。
- 未处理消息：仅当 `filters.session_id` 为顶层 eq 标量才返回缓冲原文（manager.py:285-304、1011-1024）。

**零词法重叠时**
- KEYWORD：分词后无命中词 → `sparse_recall` 返回 `[]`（episode.py:59-61）；有词但零重叠 → BM25 无命中，同样空。
- 此时除非调用方换 VECTOR/HYBRID（需 embedding，`_validate_components` 422），否则无结果。**没有任何自动 fallback**。

**来源/证据**
- 结果带 `session_id / timestamp / sender_ids / parent 链 / score / atomic_facts（带 id+content+score）`（dto.py:156-199；shaper.py:44-99）；episode 行本身回链 memcell（tables/episode.py:48-56 parent_id "Source memcell id"）。
- 调用方集成接口形状：`POST /api/v2/memory/search`，`SearchRequest{user_id XOR agent_id, app_id, project_id, query, method∈{keyword,vector,hybrid,agentic,llm_multiround}, top_k(-1或1..100), radius, min_score, include_profile, enable_llm_rerank, filters}` → `SearchResponse{request_id, data{episodes, profiles, agent_cases, agent_skills, unprocessed_messages}}`（dto.py:71-141、270-299）。注入（拼 prompt）完全在调用方，everos 侧无"注入"代码。

---

## 6. 用户与 Agent 的归属边界

- **XOR 校验**：`src/everos/memory/search/dto.py:116-120` `_validate_user_xor_agent`：`if (self.user_id is None) == (self.agent_id is None): raise ValueError("exactly one of user_id / agent_id must be provided")`，Pydantic `mode="after"` model validator，且 `model_config = ConfigDict(extra="forbid")`（dto.py:81）。派生属性 `owner_id`（dto.py:128-135）、`owner_type`（dto.py:137-140）。
- **app_id/project_id 固定进查询范围**：`src/everos/memory/search/filters.py:51-72` `compile_filters()` 无条件追加 `eq("owner_id"), eq("owner_type"), eq("app_id"), eq("project_id")`（line 60-65），且 `owner_id/owner_type/app_id/project_id` 是 RESERVED_FIELDS，用户 filters 里出现直接 `FilterError`（filters.py:46-48、89-92）。dto.py:90-91 docstring："Pinned into the LanceDB where so a search never crosses into another space's rows"。写入侧同样分区：buffer PK `(message_id, app_id, project_id)`（sqlite/tables/unprocessed_buffer.py:18-39），memcell 唯一键含 app/project（tables/memcell.py:32-35）。
- **同一用户不同 Agent 能否共享记忆**：存储按 owner 分（`users/<user_id>/` vs `agents/<agent_id>/`，docs/storage_layout.md 对照）；user 记忆路径完全不含 agent 维度（episode 行只有 owner_type="user"，`shape_episode_from_candidate` 对非 user 的 owner_type 直接丢弃并告警，shaper.py:54-62）。在搜索范围内未找到"多个 agent_id 共享同一 user 记忆"的显式机制——共享只体现在 episode 行的 `sender_ids` 里可以包含多个 sender（user_memory.py:140-147 按 user sender fan-out），但检索面仍以单一 `owner_id` 为界。

---

## 7. 异步派生作业（OME）

- **队列**：APScheduler + SQLAlchemyJobStore（独立 SQLite `aps_jobstore_path`，`src/everos/infra/ome/engine.py:328-346`），Immediate 事件经 `EventDispatcher.dispatch`（enabled/applies_to/Counter 三门，dispatcher.py:100-115）后由 `_enqueue_run` 落成一次性 APS job（engine.py:652-697，event 以 `model_dump_json()` pickle 进 jobstore，line 687）。单引擎文件锁 portalocker（engine.py:547-558）。
- **重试**：`Runner.run` per-attempt 循环，attempt 级退避 `base*2^(n-1)` capped + jitter（`src/everos/infra/ome/_dispatch/runner.py:148-181`）；`engine_sem` 只在 attempt 内持有、退避睡眠不占槽（runner.py:124-137）。`max_retries` 策略级声明（如 `extract_atomic_facts.py:42 max_retries=2`），可被 ome.toml 热改（default_ome.toml:3-8，ConfigReloader 热载 engine.py:430-441）。
- **死信**：状态机 RUNNING→SUCCESS/FAILED/DEAD_LETTER/CRASHED（`src/everos/infra/ome/_stores/run_record.py:1-11`）；重试耗尽或 `StrategyContractError` → `_terminate_dead_letter` + `on_dead_letter` 回调（runner.py:254-267、313-331；engine.py:258-269 注册）。run_record 环形缓冲同事务裁剪（run_record.py:64-70）。
- **崩溃恢复**：启动时扫描 stale RUNNING → 标 CRASHED 并以原 event payload 重新入队（engine.py:348-367；`src/everos/infra/ome/_background/crash_recovery.py`）。幂等契约明确要求策略体可重放（runner.py:13-17）。
- **超时**：每 attempt `asyncio.timeout(config.run_timeout_seconds)` 包策略体，TimeoutError 进重试/死信（runner.py:244-251）；Reflection 等 cascade 完成的 `wait_for_event` 默认 120s 超时（engine.py:876-916）。
- **Prompt 版本持久化：未找到**。`RunRecord` 只存 `event_topic/event_payload/max_retries_snapshot`（run_record.py:161-165）；全 `src/everos/infra/ome/` grep `prompt` 零命中。boundary 的 prompt slot 覆盖文件也不含版本号（prompt_slots/boundary_detection.yaml）。prompt（slot 或 everalgo 默认）与 run 之间无版本关联。

---

## A. 平叙事实的提取保障

- **prompt 层：搜索范围内未找到。** atomic facts 的 prompt 在外部包 `everalgo-user-memory==0.4.0`（pyproject.toml:91），仓库内唯一可覆盖 prompt 的 slot 是 boundary/episode 两个 yaml（config/prompt_slots/ 目录），无 atomic_facts slot。
- **结构层兜底：有。** episode 本身全量保留叙事原文——LanceDB `episode` 列 docstring："Full narrative text — original surface form (returned for display)"（tables/episode.py:63-64），md Content section 即叙事全文（user_memory.py:292）；且 episode 是检索一等公民（所有 method 都以 episode 为返回单元）。atomic facts 缺失/漏提时，平叙事实仍以 episode 形式可被 BM25/向量命中。atomic fact 抽取的输入正是这段 episode 全文（extract_atomic_facts.py:49 `event.episode_text`），而非单条消息——粒度是 episode 级。

## B. 一条消息多事实拆成原子记忆

- **有机制，但拆分发生在 episode 级、由外部 everalgo 的 LLM 抽取器完成**：`aextract_from_text` 返回事实列表（extract_atomic_facts.py:48-50），everos 逐条构造 `AtomicFact.from_algo`（line 62-70）并逐条落 md。**未找到**按"单条消息"边界的强制拆分（粒度控制不在 everos 侧）。
- 引用保持：fact.inline 写 `parent_type:"episode", parent_id=<episode_entry_id>`（extract_atomic_facts.py:98-107）；检索侧 `facts_for_episodes` 用 `parent_id IN (...)` 反查回挂父 episode（recall/atomic_fact.py:93-152），并兼容旧版 `parent_id=memcell_id`（dual parent 策略，atomic_fact.py:8-13）。依赖基础设施：LanceDB atomic_fact 表 + episode entry_id 反查（无 embedding 也可用 flat scan，`_query_facts_for_parents` line 165-169）。

## C. 非指令记忆零词法命中时能否注入

- **KEYWORD 不能**：零词法重叠 → `sparse_recall` 空结果（episode.py:59-61），无 fallback。
- **VECTOR/HYBRID 可以**（依赖 embedding provider + LanceDB ANN）：HYBRID 的 dense 路甚至通过 ~28× 更密的 atomic_fact 表做 MaxSim 挽回长 episode 的局部语义（manager.py:692-730，"~28× denser" 注释 line 697-698）；**依赖**：`get_embedding_capability()` 可用（manager.py:872-884 会 422）+ LanceDB 向量列（tables/episode.py:89 `vector: Vector(1024)`）。embedding 不可用时 cascade 把 vector 写 None、行仍可 BM25/标量检索（cascade/handlers/atomic_fact.py docstring line 22-24）。
- **画像总是可注入**（与 query 无关）：`include_profile=true` 即按 owner KV 直取，零命中查询也返回（manager.py:628-631，profile.py:32-60），依赖仅 LanceDB 标量读。
- **episode 没有总注入通道**：unprocessed buffer 原文仅随 `filters.session_id` 顶层 eq 返回（manager.py:295-304）；merged episode（session_id=None，dto.py:172-174）也走同一检索面。是否"注入"由调用方决定——everos 只提供上述检索接口，无自动注入逻辑。

**其他补充发现**
- `docs/how-memory-works.md:32-42` "Markdown is the source of truth; delete `.index/` and no memory is lost" 与源码一致：SQLite/LanceDB 均可从 md 重建（`_daily_log_base.py` 三方 reconcile、`core/persistence/lancedb/repository.py:630` `rebuild_indexes`）。
- 潜在问题汇总（源码证据）：(a) atomic facts 软删标记不落 md，全量重建可复活（第 4 节）；(b) `prepare_cells` 在 `_detect` 抛异常时不回写缓冲，该批新消息依赖客户端重试（第 1 节）；(c) prompt 版本无审计（第 7 节）。