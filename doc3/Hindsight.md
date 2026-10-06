以下为只读源码审计结果。仓库根 `C:\TRAE\Agent-Memory\hindsight`（HEAD ce4ca028，main）。除特别注明外，所有路径均相对该根；行号为当前 HEAD 实测。

---

## 1. 原始对话如何持久化

**表结构**
- `documents` 表：`id, bank_id, original_text, content_hash, metadata, created_at, updated_at`，主键 `(id, bank_id)` — `hindsight-api-slim/hindsight_api/alembic/versions/5a366d414dce_initial_schema.py:200-215`
- `memory_units` 表（事实）：`document_id` 外键指向 `documents(id,bank_id) ON DELETE CASCADE`，含 `fact_type, confidence_score, occurred_start/end, mentioned_at` 等列 — 同文件 :265-343
- chunks 单独成表：迁移 `b7c4d8e9f1a2_add_chunks_table.py`

**入口与写入**
- 抽取入口：`hindsight-api-slim/hindsight_api/engine/retain/orchestrator.py:1320` `retain_batch(...)`；流式管线 `_streaming_retain_batch` :2342；文档正文分片写入 `_store_document_bodies` :2065-2147（注释明确"Content-addressed and idempotent, so this is safe to call up front, **before the facts commit**"）。
- 原文落在 `documents.original_text`（`fact_storage.py:361` `_upsert_document_row`；`config.store_document_text` 可关掉全文只留 chunk 文本，见 `orchestrator.py:2128-2131`）。存储托管（store-owned）bank 无 SQL documents 行，正文走对象存储式 `store.put_document`（`orchestrator.py:2113-2140`）。

**抽取失败时证据是否保留：保留**
- 流式路径结尾对“零批次/无事实”分支也有事务保护：`orchestrator.py:3370-3440`，注释 "is tracked **regardless of extraction results**"（:3370），在事务内 `INSERT INTO documents (id,bank_id,original_text,content_hash) VALUES ($1,$2,'','__pending__') ON CONFLICT DO NOTHING` + `SELECT ... FOR UPDATE`（:3389-3399）。抽取失败只把 async operation 置 failed（worker 侧），documents 行已落库。
- LLM 抽取失败明细有持久化：`fact_extraction.py:2698` `_write_batch_extraction_errors`；操作元数据 `RetainExtractionErrors`（`engine/operation_metadata.py`）。
- 注意 trade-off：非 store-owned 路径中，若 retain 在抽取完成前崩溃，documents 行可能只到 `__pending__` 状态；恢复靠下述 retry 判定。

**重试不会重复**
- 内容寻址去重：`content_hash` + chunk 哈希。重试/崩溃恢复判定见 `orchestrator.py:2406`（"Check if this is a retry of the same content (crash recovery)"）、:2452-2455；delta retain `_try_delta_retain` :3587 只处理变化的 chunk（`_classify_chunk_diff` :3568）；`force_reextract` 显式绕过 :1637-1646。
- document_id 重试复用同一生成 id：`orchestrator.py:1603-1614`（"lets retries reuse the same generated id"，持久化到 `async_operations.result_metadata`）。
- 同一 document 重新 ingest 是 full-replace：`fact_storage.py:187-325` `handle_document_tracking` 先删旧文档（级联删 units）、同时删派生 observations（`delete_stale_observations_for_memories` :146-186），再插入新行。

## 2. 模型如何抽取事实（fact_extraction.py）

文件：`hindsight-api-slim/hindsight_api/engine/retain/fact_extraction.py`（3625 行）。

**结构**
- 输出 schema：`FactExtractionResponse.facts: list[ExtractedFact]` :297-300。每条事实字段 `what/when/where/who/why + fact_kind + occurred_start/end + fact_type + entities + causal_relations + from_attachments` :255-294（verbose 版 :362-452）。
- fact 类型二分：`fact_type: Literal["world","assistant"]`（:272-274、:425-427；world = 客观事实，**明确包含用户偏好/规则/纠正/约束**，即使是对话中说出的）。存储侧映射："assistant" → `experience`，其余 → `world`，兜底用 fact_kind（:2167-2176）。存储枚举为 `world/experience`（`Fact` :165；`types.py:331`）。
- fact_kind：`event`（可定日期，写 occurred_start/end）vs `conversation`（持续状态/偏好，不写日期）:1077-1079、:1252-1301。

**发生时间抽取**
- Prompt 强制相对时间→绝对日期、粗粒度日期取整段区间（"in 2015"→2015-01-01~2015-12-31，不得坍缩到第一天）:1089-1097、verbose 版 :1276-1297；以输入提供的 "Event Date" 为参照 :1279-1285。
- 代码侧兜底：`_infer_temporal_date` :83-119（"last night/yesterday..."正则→日期偏移），仅当 LLM 没给 occurred_start 时生效。
- 可选 grammar 约束：ISO 时间戳正则 `ISO_TIMESTAMP_PATTERN` :221-234，仅当 `llm_supports_string_pattern` 时注入 schema（:1600-1610 附近）。

**quote/原文引用：在搜索范围内未找到**
- 抽取 schema 与 prompt 均无“必须附原文 quote”字段；最接近的机制是：(a) verbatim 模式——原文 chunk 直接作为 fact_text，LLM 只抽元数据（:511-547、:1186-1206）；(b) `from_attachments` 归因——"Only list an attachment when the fact could not be stated without looking at it"（:287-294、:1900-1904）。

**一条消息多事实与原子性**
- 输出天然是 facts 数组，一条 chunk 可出多条事实。
- Prompt 层并未要求“每句话拆一条”；相反 concise 模式要求 "CONSOLIDATE related statements into ONE fact when possible"（:1132），且 "what" 限 1-2 句（:1057）。facet 级原子性放在 consolidation 层（见 §4，`prompts.py:41` 规则 2 "ONE OBSERVATION PER DISTINCT FACET"）。
- 输出超长时的机械兜底：`_split_chunk_for_output_retry` :303-359（按 JSON 数组对半/句界切 chunk 重抽）。

**漏提与无依据补全的约束**
- 否定约束（不要抽的）：:1126-1131（寒暄、filler、过程话、重复信息）；示例前警告 "Never emit their facts, entities, or dates unless those details also appear in the actual input"（:1141-1143）。
- 防漏提的工程保障：missing `what` 视为 malformed 触发 re-prompt，避免"模型返回 [] 导致 retain 静默完成"（:2151-2165，issue #3708）；>20% facts malformed 整体重试（:2331-2336）；重试上限 `outer_attempts = llm_max_retries + 1`（:2046-2060）。
- 逐字事实清洗：`Fact.sanitize_fact_text` / 实体名清洗 :184-209。

**情绪/偏好/动机保留**
- verbose 模式 `why` 字段定义明确："Include EVERYTHING: feelings, preferences, motivations, observations..."，并给出示例（:401-409）；concise 准则列 "Preferences: likes, dislikes..."（:1118）、"Sensory/emotional details"（:1123）及收尾强调（:1165-1168）。
- 语言规则：`:1044` `_DEFAULT_LANGUAGE_RULE`（"Write every fact in the same language and script as the input text. Never translate."）。

## 3. 事实如何获得可用地位

- **写入即可检索**：retain 抽出的 facts 经 `insert_facts_batch`（`engine/retain/fact_storage.py:65-103`）→ `get_memories().insert_facts` 直接写入 `memory_units`（含 embedding、text_signals），无独立“验证后才可见”的门。`recall`/`reflect` 直接查该表。
- **无置信层**：`confidence_score` 列在初始 schema 中存在（`5a366d414dce_initial_schema.py` memory_units 定义），但全 engine 源码 grep 无任何写入点（仅读取路径），即**代码里没有生产者，置信层名存实亡**。`proof_count` 只作用于 observation 的 rerank 加权（§5）。
- **冲突（contradiction）在哪裁决**：没有独立的 contradiction 检测器。裁决者是 consolidation 的 LLM：prompt 要求 "`deletes`: only when an observation is directly superseded or **contradicted** by new facts"（`engine/consolidation/prompts.py:169`），执行在 `_execute_delete_action`（`engine/consolidation/consolidator.py:3079`，"Delete a superseded or contradicted observation"）。状态变更走 UPDATE 规则（`prompts.py:45` 规则 4）。另有一个 LLM 语义去重裁决器 `_DedupDecision`/`_dedup_adjudicate`（`consolidator.py:163-214、291+`）处理近重复。reflect 层也有矛盾处理措辞（`engine/reflect/prompts.py:291、1103`）。
- **归档/失效**：curation 将被 invalidate 的事实移入 `invalidated_memory_units` 表（`engine/memories/pg/curation.py:11、154`），recall 只认“live"（`pg/reads.py:807`）。

## 4. 巩固（consolidation）

文件：`hindsight-api-slim/hindsight_api/engine/consolidation/consolidator.py`（3763 行）+ `prompts.py`。

- **observation 是 `memory_units` 中 `fact_type='observation'` 的行**，带 `source_memory_ids`（uuid 数组）与 `proof_count`（文件头注释 :9-10；GIN 索引迁移 `a2b3c4d5e6f8`）。
- **来源记忆 ID 关联**：LLM 输出的 `source_fact_ids` 必须逐字复制批次内的 `[uuid]`（`prompts.py:59-62、165`），代码侧再映射为 source memories；UPDATE 时 `source_ids = list(model.source_fact_ids) + live_ids`，即**旧来源保留、新来源追加**（`consolidator.py` `_apply_update_action`，约 :2915-2930）。
- **proof_count 累计**：UPDATE 路径 `proof_count = len(source_ids)` 写入 UPDATE 语句（约 :2926-2946）；dedup fold 路径 `proof_count = count(DISTINCT unnest(source_memory_ids || new_ids))`（:411-414、:508-514）。
- **旧 observation 被替换（UPDATE 重写 text + 重嵌入），且保留历史**：每次更新写 `observation_history` 快照（`_ObservationHistorySnapshot` + `_append_observation_history`，约 :2960-3010，带容量上限）；旧 observation 的删除是物理 DELETE（`_execute_delete_action` :3079-3100），同时清 history，**无软删**；软删/失效语义在 fact 级走 `invalidated_memory_units`（§3）。
- **事务性**：一次 LLM 响应的全部 creates/updates/deletes + `mark_consolidated` 戳在**同一个事务**提交（"all-or-nothing ... #3876"：设计说明 :2504-2515，执行 `async with conn.transaction()` :2683-2760，stamp 同事务 :2746-2758）；LLM/嵌入调用在事务外先 prepare（:2509-2513、`_PreparedUpdate` 注释 :863-870）。LLM 失败的批次不盖戳以便重试（:2674-2676）。
- **无证据推断的 prompt 约束**：`prompts.py:53` 规则 8 "NO COMPUTATION — never calculate, derive, or adjust numeric values... never do arithmetic or logical deductions"（含 2 只狗+1 只 Rex≠3 的示例）；:39 规则 1 限定只在"同一 canonical 事实"时 UPDATE；:103-106 决策指南；`reason` 必填用于审计重复 create（:170）。
- **已删除来源不会复活**：
  - 三重防线：(1) 选取待巩固事实的谓词是 `consolidated_at IS NULL AND consolidation_failed_at IS NULL AND fact_type IN ('experience','world')`（`engine/memory_engine.py:16251-16258`）；(2) 事务内 `FOR SHARE` 活性复查 `_filter_live_source_memories`（`consolidator.py:731-762`，UPDATE 内联检查 :2896-2903），全部死亡则跳过；(3) 删除/重 ingest 任何来源时会级联删其派生 observation 并把幸存共同来源 `consolidated_at` 复位重排队（`engine/memories/pg/writes.py:240-330` `delete_stale_observations`）。DELETE 文档同样先扫出 units 再清 observations（`fact_storage.py:210-250`）。
  - 任务级防复活：worker 重试 UPDATE 带 `AND status <> 'cancelled'` 守卫（`worker/poller.py:1011-1035`，issue #4131）。
- **审计**：`engine/audit.py:124-230` `AuditLogger`，写 `audit_log` 表（迁移 `c2d3e4f5g6h7`；INSERT :213-224，含 request/response JSONB）。consolidation 的批次审计主要靠 `reason` 字段与 `llm_requests` 追踪（§7），consolidator 本身未直接调 AuditLogger。

## 5. 检索与注入

- **多路检索**：`engine/search/retrieval.py`
  - semantic + BM25 合并 SQL：`retrieve_semantic_bm25_combined_sql` :123-400。semantic = pgvector ANN，per-fact_type partial HNSW 索引的 UNION ALL arms（:261-278）；BM25 = 词法 arms，**仅在 tokenize 后有 token 时生成**（:186-190、:233），走 `text_search_extension`（Postgres tsvector / pgroonga / ParadeDB，`config.py:1444、5119`）。Oracle 文本索引故障时自动降级 semantic-only（:327-375）。
  - temporal：`retrieve_temporal_combined_sql` :463+；时间窗内 ANN 取种子池（`_TEMPORAL_POOL_SIZE=60`）+ 8 桶时间覆盖率选择 10 个入口（:404-460）。
  - graph：`engine/search/graph_retrieval.py:19-68` 抽象 `GraphRetriever`；实现 `LinkExpansionRetriever`（`engine/search/link_expansion_retrieval.py:112+`），基于 Postgres 里的 `memory_links`/`unit_entities` 实体共现链接扩展（:316-325），种子来自 semantic arm 的高分行（`retrieval.py:198-208、391-398`）。
- **RRF 融合**：`engine/search/fusion.py:29-109`，`score(d) = Σ 1/(k + rank(d))`，`k=60`（:33、:85），arm 顺序固定 `["semantic","bm25","graph","temporal"]`（:56）。另有 `interleave_fusion`（:112-176）为 consolidation 去重召回保证每路 #1 必入池（解决语义 #1 与旧 observation 零词法重叠被 RRF 淘汰的失败模式，:115-125）；`cap_per_source` :8-26。
- **Rerank 有界调整**：`engine/search/reranking.py:16-37` —— recency α=0.2、temporal proximity α=0.2、proof count α=0.1，均为乘法 boost，界 ±10%/±10%/±5%（公式 :177-207、:284-287）；proof_count 对数归一化且非 observation 中性（:269-275）；粗粒度日期从区间末端计龄并封顶中性（:107-155）；slim 部署的 passthrough reranker 用 RRF 排名作基底分（:222-245）。
- **零词法重叠时 BM25 表现**：无命中（tsquery 不匹配；`bm25_min_score` 默认 0.0，`config.py:1303`）。召回靠 semantic（embedding 语义），consolidation 场景另有 interleave 保证（上文），以及 graph 扩展命中语义相关但无共现词的事实。
- **结果带来源证据**：`RetrievalResult` 含 `document_id, chunk_id, proof_count` 等（cols :210-213）；recall 返回每条 observation 的 `source_fact_ids` 与按 token 预算填充的 `source_facts` 溯源 map（`engine/source_facts.py:1-70`）；结果文本按预算裁剪（`engine/fact_budget.py:1-60`，保底不空手）。
- **预算/top-k**：每 arm limit（`retrieval.py:158-165`）；reranker 全局候选上限 `RERANKER_MAX_CANDIDATES`（见 `recall_boost.py:31`）；token 预算 `fact_budget.py`。
- **“指令类记忆独立于查询”**：存在两套。(1) **directives**：用户定义硬规则，"always included in relevant prompts"，不参与检索（`engine/directives/models.py:9-32`）；reflect 时按 tag 范围加载并"注入为 prompts 顶部的硬规则"（`engine/memory_engine.py:12806-12821、15491-15521`）。(2) **mental models**：后台 cron 定时刷新（迁移 `f4d1c2b3a5e6`），reflect 时一并注入（`memory_engine.py:15524-15538`）。

## 6. 用户与 Agent 归属

- **bank 是最高隔离层级**，即调用方自命的命名空间（一个 agent 通常一个 bank）。物理隔离：`documents` 主键含 `bank_id`（`initial_schema.py:212`），所有查询 SQL 都带 `bank_id = $N`（如 `retrieval.py:270-277`）；bank 元数据在 `banks` 表（`initial_schema.py:190-198`）。
- **与 user/agent 的关系**：代码中只有 `bank_id`，没有独立 user/agent 实体；MCP 场景 bank 由会话级 `bank_id_resolver` 解析，可多 bank（`mcp_tools.py:269-285、383`）。同一 bank 内多 Agent 共享 = 使用同一 `bank_id` 即共享，无 per-agent 维度。
- **更细 ACL = tags 可见性范围**：tag 匹配 any/all + 复合 `tag_groups`（`engine/search/tags.py`）；untagged reflect 的隔离（`memory_engine.py:12816-12821` isolation_mode）；consolidator 里 tag 过滤注释为安全边界（"SECURITY: prevent cross-tenant/cross-user"，`consolidator.py` `_find_related_observations` 约 :3120）。**未找到比 tags 更细的行级 ACL 机制**。
- 之上还有 schema 级租户：`_authenticate_tenant` 通过 tenant extension 认证并把 schema 放进 contextvar（`memory_engine.py:2933-2970`）。bank_id 也可作为成本归因透传给 LLM 供应商（`engine/bank_attribution.py:19-48`，opt-in）。

## 7. 异步任务

- **队列 = Postgres `async_operations` 表作 broker**：`engine/task_backend.py:153-260` `BrokerTaskBackend`（INSERT payload :244-255；`_submit_async_operation` 统一入口 `memory_engine.py:21724`）。三种 backend：Broker（API 进程）、Worker（no-op submit，行已落库，`task_backend.py:126-150`）、Sync（嵌入式/测试 :95-123）。
- **状态机**：`pending/processing/completed/failed`（`initial_schema.py:217-250`），后加 `cancelled`（迁移 `i4j5k6l7m8n9`）。
- **领取/执行**：`worker/poller.py`（`claim_batch` :529，FOR UPDATE SKIP LOCKED 式领取；retain 批次折叠 `_fold_retain_peers` :735）。
- **重试**：`_schedule_retry` :1011-1035（`status='pending', next_retry_at, retry_count+1`，带 cancelled 防复活守卫）；默认 `max_retries=3`（:312、:336、:367）；耗尽后标记 failed（"exceeded max recovery attempts" :1326）。
- **Dead-letter：未找到**——搜索范围（`worker/` 全部、`async_operations` 迁移）内没有独立 dead-letter 队列/表；终态就是 `failed` + `error_message`，另有 `consolidation_failed_at` 列让失败事实可被例行任务重新排队（`memory_engine.py:11692-11734`）。
- **超时**：每任务 wall-clock 上限 `_wall_timeout_for` + `_WallCeiling`（`poller.py:57-149`，consolidation 用“无进展时限”；未映射类型无上限）；模型懒加载超时 `model_init_timeout`（`reranking.py:336-345`）；背压力 defer 不计重试（:1037-1075、:1236-1243）。
- **Prompt 版本持久化**：prompt 本体是代码常量（版本=git 版本），无 prompt 模板表；LLM 调用持久化在 **`llm_requests` 表**（迁移 `d3e4f5a6b7c8`："capturing the input messages, model output, token usage"；写入 `engine/llm_trace.py:584-600`，Postgres-only :40-43）；prompt 可预览渲染（`engine/prompt_preview.py`，API `api/http.py:2376-2379`）。

---

## A. 平叙事实（plain declarative statements）的提取保障

- **Prompt 层是“选择性抽取”而非“全量抽取”**：入口句 "Extract SIGNIFICANT facts... Be SELECTIVE"（`fact_extraction.py:1049`）；正例清单包含 "Important context: projects, problems, constraints"（:1122）与 "Observations: descriptions... with specific details"（:1124）——平叙的陈述性事实靠这两条兜住；反例清单 :1126-1131；校验问句 "Would this be useful to recall in 6 months? If no, skip it"（:1160-1163）。**没有“每条陈述都必须保留”的硬保障**。
- 有 few-shot 示例（:1145-1157）演示“跳过寒暄/琐事、保留计划与事件”。
- 银行可覆盖默认取舍：per-bank `retain_mission` 以 "FOCUS... takes priority over the general guidelines" 注入用户消息（`_retain_mission_preamble`，:1761-1779）。
- 工程兜底：`what` 缺失→整轮 re-prompt 防静默漏抽（:2151-2165）；>20% malformed→重试（:2331-2336）；要原文级保真只有 verbatim 模式（:511-547）。

## B. “一条消息多事实拆原子”的机制

- 结构上支持：输出 schema 是 facts 数组，一条消息/chunk 可产出任意多条事实（`fact_extraction.py:297-300`）；conversation/JSONL 输入按 turn/line 边界切块，不跨块截断一条消息（:872-927、:976-1023）。
- **Prompt 并不追求句子级原子化**：concise 模式反向要求 "CONSOLIDATE related statements into ONE fact when possible"（:1132），单事实 what 限 1-2 句（:1057）。真正按“单一 facet”原子化的是 consolidation 层：`engine/consolidation/prompts.py:41` 规则 2 "ONE OBSERVATION PER DISTINCT FACET ... Never merge different facets into one observation"，:43 规则 3 按 entity/facet 匹配而非 topic。
- 输出超长时对 dense chunk 再对半切以挽救多事实输出（`_split_chunk_for_output_retry`，:303-359）。

## C. 零词法命中时的兜底，及“去掉 embedding 基础设施”后剩什么

- **BM25 路**：查询无 token 时整路省略（`retrieval.py:186-190、233`）；有 token 但零词法重叠时 tsquery 无命中。BM25 依赖 Postgres 文本检索扩展（tsvector/GIN、可选 pgroonga/ParadeDB，`config.py:1444、5119`）与 `text_signals` 列（迁移 `a2b3c4d5e6f7`）。
- **semantic 路（主兜底）**：pgvector HNSW 向量检索（per-fact_type partial index，`retrieval.py:149-165`），依赖 embedding 模型——默认**本地 in-process SentenceTransformers**（`config.py:1206` `DEFAULT_EMBEDDINGS_PROVIDER="local"`；env `HINDSIGHT_API_EMBEDDINGS_PROVIDER` :464），也可 TEI/OpenAI 等（`engine/embeddings.py`）。
- **graph 路**：`LinkExpansionRetriever` 查询时只依赖 Postgres 表 `memory_links`/`unit_entities`/`entities`（`link_expansion_retrieval.py:316-325`），但**入口种子来自 semantic arm 的高分行**（`retrieval.py:198-208、391-398`）——图扩展本身不用 embedding，找入口用。
- **temporal 路**：时间窗内 ANN 打分 + 时间覆盖桶选择入口（:404-460），同样 embedding 依赖（ANN）+ 纯 Postgres 日期索引。
- **融合层兜底**：RRF `1/(60+rank)`（`fusion.py:85`）；consolidation 去重召回改用 interleave，保证纯语义命中的 #1 必进候选池（`fusion.py:112-130`）。
- **若完全去掉 embedding 基础设施**：semantic 臂消失；temporal 与 graph 的入口选择因依赖 semantic 行而饿死（temporal 池直接来自 ANN，`retrieval.py:536-542`；graph 种子同上）——图结构数据还在但无查询期入口。剩余可用的只有：(1) BM25 词法臂（纯 Postgres，前提是查询与记忆有词法重叠）；(2) directives/mental models 的非检索式注入（`memory_engine.py:15491+`）；(3) tag 过滤与 expand 工具类按 ID 访问。**搜索范围内未找到“BM25-only 也能完成 recall”的开关**——`enable_text_search` 只能关 BM25，semantic 臂无条件参与（`retrieval.py:186-190` 处逻辑对称读取）；注意默认 embedding provider 是本地的，因此“去掉 hosted embedding 服务”并不等于去掉 embedding 能力。