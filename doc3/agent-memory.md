审计完成。以下按 1–9 + A/B/C 组织，全部结论基于源码，附 path:line。路径前缀 `C:\TRAE\Agent-Memory\agent-memory`（下文简写省略）。

---

## 1. L0 事件持久化

**服务端 handler**：`ingest_events` — `crates\memory-server\src\main.rs:367-432`（路由注册 main.rs:205）。字段校验 main.rs:385-402（event_seq 非负、role/source_kind 枚举、RFC3339、content 1~64KiB）。

**幂等/冲突（EVENT_CONFLICT）**：`record_evidence` — `crates\memory-store-sqlite\src\evidence.rs:91-167`
- 幂等键 = `(tenant_id,user_id,host_id,session_id,event_seq)`（evidence.rs:109-116，唯一约束见 migrations\0001_init.sql:35）。
- 同键同 `content_sha256` → `AlreadyRecorded`（evidence.rs:120-123，HTTP 200）；同键异 hash → `StoreError::EventConflict`（evidence.rs:124）→ main.rs:427-429 返回 409 `EVENT_CONFLICT`。

**事务边界**：单事务——tx 开启 evidence.rs:105，INSERT evidence_events（evidence.rs:128-148）+ audit_events `event_ingested`（evidence.rs:150-164）+ commit（evidence.rs:165）。

**适配器捕获**（adapters\dsh\src\events.ts）：
- 回调入口 `observeSessionEvent`（events.ts:100-128）：同步验证 → opId=`host/session/seq` → 有界队列（1024 条/8MiB，满拒收记 CAPTURE_GAP，events.ts:223-236）。
- 事件映射 `mapEvent`（events.ts:421-473）：`user/message` 仅当 `source.kind==='user'` 才 role=user/source_kind=user（events.ts:439-455）；`assistant/message`、`tool/result` 分别 role=assistant/tool（events.ts:456-469）。
- spool/receipt/replay：`Spool.append` fsync 落盘（spool.ts:112-130），`markAcked` 内核 200/201 后写 receipt 含 evidence_id（spool.ts:133-142）；启动重放 `pending()` 返回未 ack op（spool.ts:153-167）→ `enqueueExisting`（events.ts:288-297）。
- 发送链 `drainChain`（events.ts:314-364）：200/201/202 → markAcked；**receipt 写失败保持未 ack 重发**（events.ts:330-336）；401 → 暂停全部出站（events.ts:346-352）；400/409 → 永久停发该 session（events.ts:353-360）；退避重试 `retryLater` 500ms 基数、指数、上限 15s（events.ts:366-377）。
- **dispose 排空**：`EventPipeline.dispose`（events.ts:188-219）——deadline 5s 内等内存队列+在途链清空，再置 stopped、唤醒 writer、再给 2s，最后拒绝全部 waiter；未 ack 保留 spool。插件层 `dispose`（index.ts:134-139）经 ctx.effect 调用。
- **flush 触发条件**：仅 `turn/end` 回调 → `enqueueFlush`（events.ts:104-107、299-310），through 取该 session 已排队最后正文 seq。**"80 事件阈值"在代码中未找到**（在 crates 与 adapters/dsh/src 全文 grep "80"/阈值 无命中）；session 退出时 dispose 只排空在途项，自身不补发 flush。

## 2. 提取窗口与作业

**flush 创建作业**：`flush_window` — `crates\memory-store-sqlite\src\jobs.rs:52-121`
- `window_key = format!("v1:{through_event_seq}")`（jobs.rs:89）。
- 越界（through > max_seq）→ StateConflict（jobs.rs:59-62）；乱序（through < 已排最大 through）→ StateConflict（jobs.rs:79-94）；同 window_key 幂等返回原 job（jobs.rs:96-107）；INSERT 时显式写 `EXTRACT_PROMPT_VERSION`（jobs.rs:110-119）。

**worker 领取**：`spawn_worker`（worker.rs:677-709，500ms 轮询）→ `claim_next_ordered_job`（jobs.rs:179-211）——先算 blocked session 集：存在同 session 更早的 `dead`/`running` 作业则该 session 的作业本轮还原 queued 跳过（jobs.rs:181-198、201-209）；再 `claim_due_job`（jobs.rs:146-176）：取 `created_at` 最早的 queued（到点）或 retryable_failed（lease 过期），原子 UPDATE→running 并写 lease（jobs.rs:162-171，`JOB_LEASE_SECS=90`，memory-contract\src\lib.rs:39）。
- **下界**：`window_lower_bound`（jobs.rs:124-143）= 同 session 中 `window_key < 当前` 且 `status='succeeded'` 的最大 through_event_seq，首窗 -1。`load_window_events`（jobs.rs:214-248）读 `(下界, through]`，>100 事件或 >32KiB → StateConflict（jobs.rs:240-246）。

**claim/lease/重试**：`fail_job`（jobs.rs:267-290）——`attempts >= 3`（`JOB_MAX_ATTEMPTS`，contract lib.rs:41）→ `dead`；否则 `retryable_failed` + `run_after` = 5/15/45s（`JOB_RETRY_DELAYS_SECS`，contract lib.rs:43，索引 attempts-1）。

**/v1/jobs/{id}/retry**：main.rs:752-763 → `retry_dead_job`（jobs.rs:306-314），仅 `status='dead'` 可重试（置回 queued），否则 409 `仅 dead 状态作业可重试`（main.rs:760）。

**模型调用**：`OpenAiCompatibleClient`（worker.rs:594-674）——reqwest，connect+总超时同为 `cfg.timeout`（worker.rs:611-615；`model_timeout_secs` 默认 30，main.rs:186-188 + contract lib.rs:37）；`build_request_body`（worker.rs:566-587）temperature=0.0、`max_tokens` 默认 1024（main.rs:189）、`extra_json` 顶层合并可覆盖默认（worker.rs:580-585）；响应有界 4MiB（worker.rs:590、648-656）；408/504 → Timeout（worker.rs:638-640）。

**usage 持久化**：`complete_job`（jobs.rs:250-265）写 `model_name/input_tokens/output_tokens`；仅成功路径调用（worker.rs:791-799），提供方未给保持 NULL。

## 3. Prompt 版本分派

- 常量：`EXTRACT_SYSTEM_PROMPT_V1`（memory-extract\src\lib.rs:10-14）、`EXTRACT_SYSTEM_PROMPT`（extract_v2，lib.rs:21-31）；版本串 `extract_v1`/`extract_v2`（memory-contract\src\lib.rs:14-15）。
- 分派：`system_prompt_for`（memory-extract lib.rs:37-43）按版本 match，未知返回 None；worker 在 `process_job` 按作业行 `job.prompt_version` 调用（worker.rs:721-731），未知版本 → `fail_job(..., "UNKNOWN_PROMPT_VERSION")` 显式失败，不用最新规则（worker.rs:723-730）。
- 迁移 0002：`ALTER TABLE extraction_jobs ADD COLUMN prompt_version TEXT NOT NULL DEFAULT 'extract_v1'`（migrations\0002_prompt_version.sql:5）。
- `SCHEMA_VERSION = 2`（memory-contract\src\lib.rs:9）。

## 4. 候选准入（对照 doc/13 §5 十条）

纯函数 `admit`（memory-extract\src\lib.rs:132-164，规则 1-7 步）+ 查库规则在 `save_candidate`（memory-store-sqlite\src\jobs.rs:329-498，规则 8/9/10）：

1. **BAD_SOURCE**：admit lib.rs:135-140（不在窗口/role≠user/source_kind≠user）；store 侧复检 jobs.rs:350-355。
2. **QUOTE_MISMATCH**：admit lib.rs:142-144（`ev.content.contains(&c.quote)`）；store 侧 `find_quote_span` 复检 jobs.rs:356-359（memory-domain\src\lib.rs:117-119，纯 `content.find`）。
3. **INVALID_QUOTE**（doc 未列，代码额外）：lib.rs:146-150。
4. **MODEL_PARAPHRASE：代码中未找到**（全仓 grep 仅命中 doc\13:76）。`ModelCandidate` 无 claim 字段且 `#[serde(deny_unknown_fields)]`（lib.rs:51-53）——模型若输出独立 claim 会直接 BAD_JSON，而非 `held:MODEL_PARAPHRASE`；claim 恒取 `fold_whitespace(quote)`（jobs.rs:337、memories.rs:220）。
5. **CONTEXT_UNCERTAIN**：`context_uncertain`（lib.rs:167-176）；一次性词表 `ONCE_ZH=["如果","假如","比如","这次","本次"]`、`ONCE_EN=["if ","for example","this time","just today"]`（lib.rs:169-170）。注意 doc/13 §5.5 的 `今天先` 未进代码词表。
6. **NOT_EXPLICIT**：`explicit_enough`（lib.rs:179-206）——指令词 contains：`["以后","总是","从现在起","记住"]`（lib.rs:182）/`["always","from now on","remember"]`（lib.rs:183）；自我陈述 starts_with：`["我叫","我是","我住在","我喜欢","我不喜欢"]`（lib.rs:191）/`["my name is","i am a","i am an","i live","i like","i dislike"]`（lib.rs:192）；职业：starts_with `"我在"` 且前 20 字符含 `"工作"`（lib.rs:201-204）；否则 false → `Held("NOT_EXPLICIT")`（lib.rs:156-158）。
7. **SENSITIVE**：`sensitive`（lib.rs:210-260）——**手写匹配而非正则**（注释 lib.rs:209 "避免正则依赖"）：password/passwd/密码+分隔符+值≥4（216-227）；api_key 变体/token+≥12（229-240）；sk-+≥12（242-257）；字面词 `["身份证","银行卡","诊断","病历"]`（lib.rs:259）。
   - **SUPPRESSED_SOURCE**（doc 规则 8b）：jobs.rs:433-444，查 `suppressed_sources WHERE evidence_id=? AND claim_sha256=?` → `Rejected{SUPPRESSED_SOURCE}`。
   - **DUPLICATE_CANDIDATE**（幂等）：jobs.rs:368-381（同 job+evidence+kind+quote_sha 跳过）。
   - **DUPLICATE_ACTIVE**（规则 8a）：jobs.rs:416-431——同 kind+`claim_sha256` 的 active 存在时只 INSERT memory_evidence，返回 `Rejected{DUPLICATE_ACTIVE}`（注意是 Rejected 而非 Held）。
   - **POSSIBLE_CONFLICT**（规则 9）：`attribute_conflict`（jobs.rs:502-529）+ 属性键 `extract_attr_key`（jobs.rs:533-551：name/residence/occupation/response_language 固定前后缀）；命中置 held（jobs.rs:447-453）。
8. **重复 claim 只增证据**：即上 DUPLICATE_ACTIVE——`INSERT OR IGNORE INTO memory_evidence`（jobs.rs:426-430），不新建记忆、不改 claim。

## 5. 多事实拆分

- **提示词**：extract_v2（lib.rs:21-31）只有"quote 必须逐字复制同一条用户消息中的连续原文；不要改写、补充或拼接多条消息"（lib.rs:25）和**单个**示例（lib.rs:28）。**未找到**任何"一条消息多个事实拆成多条候选"的指令。
- **准入层**：`admit` 对 quote 只做 `contains`（lib.rs:142）+ 长度 ≤512（lib.rs:147）；**未找到**任何对"整句跨度/带口语前缀 quote"的拦截或拆分逻辑——整句（含"那个/其实"等前缀）只要是连续子串即通过 QUOTE_MISMATCH，之后仅受 NOT_EXPLICIT 前缀规则约束（恰好整句以"我喜欢"等开头时整句直接 active）。store 侧 `find_quote_span` 是纯 `content.find`（memory-domain lib.rs:117-119），无跨度位置校验。

## 6. 纠错/遗忘/去重

- **correct_memory**（memories.rs:609-716）：`check_latest_user_evidence`（memories.rs:585-606，经 `latest_user_event` evidence.rs:43-61 校验是会话最新 user/user 事件）→ 乐观锁 version 校验（memories.rs:621-623）→ 最新消息须同时含 old_quote 与 replacement_quote（memories.rs:626-631）且 old_quote 在旧 claim 中（memories.rs:632-634）→ 旧记忆 superseded+version+1（memories.rs:649-656）→ 新记忆 active（memories.rs:664-674）→ memory_relations `'supersedes'`（memories.rs:687-691）。**注意：correct 路径不写 suppressed_sources**（旧记忆是 superseded 非 forgotten；suppressed_sources 仅 forget 写入）。
- **forget_memory**（memories.rs:719-789）：遗忘动词 `has_forget_cue`（memories.rs:83-88，memories.rs:730-732 校验）；G-13 闸门：target_quote 须同时在最新消息正文与目标 claim（memories.rs:735-740）；**幂等重入**：已 forgotten 再确认直接返回当前状态不卡版本（memories.rs:741-744）；version 校验（memories.rs:748-750）→ forgotten+version+1（memories.rs:754-761）→ **suppressed_sources 对每条证据引用写一行**（memories.rs:770-777）。
- **重放阻断**：`save_candidate` 规则 8b 查询 jobs.rs:434-442：
  ```sql
  SELECT 1 FROM suppressed_sources
   WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3 AND claim_sha256=?4
  ```
  命中 → `Rejected{SUPPRESSED_SOURCE}`（jobs.rs:443-445）。该检查仅在 admission==Active 且无 DUPLICATE_ACTIVE 时执行。
- **memory_revisions 写入点**：extract jobs.rs:473-479（reason `'extract_v1'`）；remember memories.rs:296-302（`'remember'`）；correct memories.rs:657-663 与 680-686（`'user_correct'`）；forget memories.rs:762-768（`'user_forget'`）。
- **audit_events 写入点**：evidence.rs:150-164（`event_ingested`）；memories.rs:303-315（`memory_remember`）；memories.rs:692-700（`memory_correct`）；memories.rs:778-782（`memory_forget`）。

## 7. 检索与注入

**search_memories**（memories.rs:369-556）：
- FTS5 路：`latin_tokens` + quoted terms，`ORDER BY rank LIMIT 100`（memories.rs:383-394，`SEARCH_PER_CHANNEL_LIMIT=100` contract lib.rs:47）。
- memory_grams 中文二元字路：`cjk_bigrams`（memory-recall\src\lib.rs:36-67），按命中 gram 数排序（memories.rs:397-422）。
- 1 字降级：仅当两路皆空且有界子串，扫最近 500 条 active（memories.rs:425-444）。
- 历史路：include_history 时对规范表 superseded/expired 有界扫描（memories.rs:448-470）。
- RRF 融合：两路皆有时 `rrf_score`（memory-recall lib.rs:9-14，`RRF_K=60` contract lib.rs:48）；单路 1/rank（memories.rs:495-502）。
- 最终 join 强制 scope+status+valid_until 过滤（memories.rs:521-533）；`evidence_refs` 只返回 evidence_id 列表（memories.rs:535-539）。

**compose_context**（memories.rs:834-910）：
- `search_memories(scope, query, 20, false)`（memories.rs:842）。
- 指令名额：先收查询命中的 instruction 最多 2 个（memories.rs:851-858）；名额未满时 `active_instruction_hits` 按 `updated_at DESC` 补齐未命中指令（memories.rs:859-870；查询 memories.rs:919-931）。
- **非指令记忆只来自 `hits`（词法搜索结果）**：memories.rs:872-876 直接遍历 `hits.iter().filter(kind != "instruction")`——**无任何 recency/embedding 兜底**（全仓无 embedding 相关代码；recency 补齐路径 `active_instruction_hits` 仅限 kind='instruction'）。
- 预算：max_items 1~5（main.rs:646-648，默认 `COMPOSE_MAX_ITEMS_DEFAULT=5` contract lib.rs:19）；max_chars 100~2000（main.rs:649-652）；单条超预算整条跳过置 truncated，不截断句子（memories.rs:896-899）。
- `<agent_memory scope="current_user" generated_at=...>` 头 + `</agent_memory>` 尾（memories.rs:880-884）；items 返回 memory_id + evidence_ids（main.rs:660-663）。
- **index degraded**：`index_state.dirty`（memories.rs:135-142）；规范事务 mark dirty（memories.rs:119-125），独立索引事务成功才清 dirty（memories.rs:127-133、145-173）；失败保留 dirty 不回滚（注释 memories.rs:3-5）；search/compose 返回该标志（memories.rs:555、909）；`rebuild_index` CLI 全量重建（memories.rs:792-823）。

## 8. scope 隔离

- **token→principal**：`verify_token`（memory-store-sqlite\src\lib.rs:310-321，SHA-256 + status='active'）；中间件 `request_pipeline`（main.rs:264-317）解析后 `req.extensions_mut().insert(scope)`（main.rs:315）。
- **查询绑定**：所有 handler 取 `Extension(scope)` 并传入 store；每条 SQL 均含 `tenant_id=? AND user_id=?`，例如 evidence.rs:109-116、jobs.rs:70/100/298、memories.rs:332/526-530、retry jobs.rs:310。
- **正文 user_id 被拒**：所有请求 DTO 均 `#[serde(deny_unknown_fields)]` 且无 user_id 字段——IngestRequest main.rs:321-330、RememberRequest main.rs:436-443、SearchRequest main.rs:560-566、ComposeRequest main.rs:623-630、Correct/Forget main.rs:767-784。带 user_id → 反序列化失败 → 400 `INVALID_FIELD`（main.rs:379 等）。ComposeRequest 的 agent_id 在 store 层被弃用（memories.rs:837 `_agent_id`）。
- **origin_agent_id 读过滤：未找到**——全仓无任何查询按 origin_agent_id 过滤（该列仅写入 memories 表 jobs.rs:462、memories.rs:268，并在 get_memory 回显 main.rs:548）。

## 9. 适配器注入

- **compose 时机**：`agent/pre-step` 钩子（index.ts:92-95 注册），每轮 pre-step 调用一次；`makePreStepHook`（recall.ts:63-100）：先 `await next()`（recall.ts:65），`decision.kind!=='enter'` 或 aborted 跳过（recall.ts:66），同 decision 已有本插件消息跳过防重复（recall.ts:58-61、67）。
- 查询 = 最后一条 `source.kind==='user'` 的用户消息原文（recall.ts:47-56、68）。
- **超时跳过**：AbortController 定时 `composeTimeoutMs`（默认 500ms，config.ts:30）+ 父 signal 联动（recall.ts:71-79）；非 200 → `logger.warn("compose 跳过注入...")` 返回原 decision（recall.ts:80-83）；异常同样跳过（recall.ts:95-98）；空 text 不加占位（recall.ts:85）。
- **块组装**：`<agent_memory>` 文本由服务端 compose_context 生成（memories.rs:880-909）；适配器用 `createUserMessage({content:[{type:"text",text}], source:{kind:"agent-memory",form:"recall"}})` 追加（recall.ts:86-90）——注入 source 非 'user'，事件线不会回流为用户证据（recall.ts:9-13 注释 + events.ts:441 只认 source.kind==='user'）。

---

## 特别确认

**A. 平叙事实必然 held:NOT_EXPLICIT —— 是（就准入层而言）**
决定性代码：`explicit_enough`（memory-extract\src\lib.rs:179-206）完全不读 `c.kind`——fact 类没有任何专属规则；fact 候选只能通过①指令词 contains（lib.rs:182-189，如含"记住"）②SELF 前缀 starts_with（lib.rs:191-199：`我叫/我是/我住在/我喜欢/我不喜欢` 及英文对应）③职业规则（lib.rs:201-204：`我在`+前 20 字符含`工作`）三者之一才 Active，否则在 lib.rs:156-158 落 `Held("NOT_EXPLICIT")`。因此"我在杭州做后端开发，平时主要写 Rust"这类平叙事实（`我在`开头但前 20 字符无"工作"，且不以`我住在`开头）必然 held。测试佐证：`not_explicit_held`（lib.rs:341-345）、`occupation_rule` 中"我在家里休息" held（lib.rs:364-366）而"我在腾讯工作三年了" active（lib.rs:360-363）。常量表：INSTRUCTION_ZH lib.rs:182、SELF_ZH lib.rs:191、SELF_EN lib.rs:192-193。

**B. 准入/提示词层对"多事实整句候选"无拆分或拦截 —— 确认未找到**
- extract_v2 提示词（lib.rs:21-31）无拆分要求，仅一个单事实示例（lib.rs:28）。
- 准入仅 `contains`（lib.rs:142）+ 长度上限（lib.rs:147），`find_quote_span` 是纯 `content.find`（memory-domain\src\lib.rs:117-119）；全仓（crates + adapters/dsh/src）无任何按句切分、跨度起点校验或口语前缀剥离的代码。

**C. compose_context 非指令记忆候选来源只有词法搜索 —— 确认**
memories.rs:872-876：非指令项只从 `hits`（= `search_memories` 的词法结果，memories.rs:842）筛选；recency（updated_at DESC）兜底路径 `active_instruction_hits`（memories.rs:913-962）查询条件硬编码 `kind='instruction'`（memories.rs:921-922），仅用于指令名额补齐（memories.rs:861）。全仓无 embedding/向量检索代码，非指令记忆零词法命中时确实不会被注入。