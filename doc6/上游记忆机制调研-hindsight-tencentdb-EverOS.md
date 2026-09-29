# 上游参考项目记忆机制调研报告

> 调研对象：`hindsight/`、`tencentdb-agent-memory/`、`EverOS/`（均为本仓库只读参考）
> 调研日期：2026-09-26。所有结论均核对源码，关键文件路径见文末附录。遗忘/删除/衰减一节随后再次按源码复核；旧稿相关结论已在 2026-09-26 更正，实施以 [12](12-遗忘、过期与时间衰减方案决策.md) 为准。

## 0. 一页总览

| 维度 | hindsight | tencentdb-agent-memory | EverOS |
|---|---|---|---|
| 技术栈 | Python/FastAPI + PostgreSQL/pgvector，CLI 为 Rust | TypeScript/Node monorepo，存储可插拔（SQLite/FTS5 默认、腾讯云 TCVDB、MongoDB 实验） | Python（DDD + FastAPI/CLI），Markdown（事实源）+ SQLite（状态）+ LanceDB（向量） |
| LLM 依赖 | 深（抽取/合并/reflect/mental model 全走 LLM） | 深（抽取、去重判定、L2/L3 生成全走 LLM） | 深（边界检测、抽取、画像/反思全走 LLM） |
| 通用记忆评分 | 当前主记忆模型无 opinion confidence 字段（旧字段已移除） | L1 `priority` 表达重要性，缺省 50；不代表可信度 | 普通记忆无统一分；Agent case/skill 有专用质量、置信度或成熟度指标 |
| 写入时机 | HTTP 默认同步（等 LLM 抽取完返回），可选异步排队 | L0 立即落盘 JSONL；L1 由调度器择机抽取 | 先暂存 buffer，边界检测/flush 时才 LLM 抽取 |
| 召回 | 4 路并行（向量/BM25/图/时间）→ RRF → cross-encoder rerank + recency decay | 3 策略自动降级（hybrid=FTS5+向量 RRF / 纯向量 / 纯 FTS），无 LLM rerank | 4 方法（KEYWORD/VECTOR/HYBRID/AGENTIC），HYBRID=双路+RRF+LR 校准，默认无 rerank |
| 自动注入 prompt | 有（LiteLLM wrapper：调用前 recall，调用后 retain） | 有（before_prompt_build 钩子：记忆→user prompt，persona→system prompt） | 无自动注入，宿主自行决定（纯 API/工具返回） |
| 审计 | 可配置 `audit_log` 记录操作请求/响应；Memory Defense 单独处理 PII 脱敏/拦截 | 专用 `memory_audit` 记录 L1/L2/L3 update/delete 的 team/user/agent/task scope、version/time/request_id；best-effort；不存旧/新正文 | SQLite `md_change_state` 是 cascade 队列/重放状态；`reflection_report` 记录反思合并来源与结果，不是通用记忆修改历史 |
| 整理机制 | retain 后自动 consolidation + 周期 reconcile | 防抖定时器管道（L1→L2→L3）；另有显式删除 API 与可配置 retention cleaner | OME 事件驱动 + APScheduler；reflect_episodes 每周合并，deprecated_by 软归档 |
| 遗忘/保留/时间排序 | 单条可逆 invalidate；bank/document/clear 硬删除面；recency rerank | L1/L0/全层显式删除；配置后定时物理清理；检索不做 recency 衰减 | episode 合并软归档 `deprecated_by`；Knowledge 文档删除是独立对象；未见 recency decay |
| soul.md / memory.md 等价物 | mental models + knowledge pages（DB 为源，可镜像为本地 markdown） | **persona.md（L3）+ scene_blocks/{name}.md（L2）**，LLM 生成 | **Markdown 即事实源**；ProfileWriter 原生支持 `soul.md`/`agent.md` 等单文件覆写，但当前仅 `user.md` 被实际写入 |

---

## 1. hindsight

### 1.1 存入（retain）

- 入口 `MemoryEngine.retain_async / retain_batch_async`；HTTP 默认**同步**（等 LLM 抽取完才返回 unit ids），`async=true` 时先写 documents 再入 `async_operations` 队列由后台 worker 处理。
- 流程：orchestrator 协调 → LLM 抽取 facts → 实体归一（bank 内 lower(name) 唯一）→ 建链。
- 数据模型核心表：`documents`（content_hash 幂等）、`entities`、`memory_units`（text/embedding/event_date/occurred_start-end/fact_type ∈ {world, experience, observation}）、`memory_links`（temporal/semantic/entity/causes/caused_by/enables/prevents + weight）、`unit_entities`、`entity_cooccurrences`；后续迁移加了 `mental_models`、`knowledge_pages`、`directives`、observation 的 `source_memory_ids`/`proof_count`。早期 schema 曾有 opinion 专用 `confidence_score`，当前迁移已删除 opinion 类型、旧记录和该字段。
- 准入规则：document/chunk 级 content_hash 去重幂等、实体去重、可选 Memory Defense（PII/密钥脱敏拦截）。当前主记忆模型无统一置信度分，且**无逐字 span 硬约束**；observation 保留 `source_fact_ids` 溯源。

### 1.2 召回（recall）

- **4 路并行**：向量（pgvector/HNSW/DiskANN 等）、BM25（tsvector/vchord_bm25 等，单条组合 SQL）、图（LinkExpansion 走 memory_links）、时间（时间窗 + 时间语言解析）。
- 融合排序：RRF k=60（另有 interleave）→ cross-encoder rerank → recency decay 加权（linear/exponential/none）→ final score（reranker + recency/temporal/proof boosts）。
- 注入：LiteLLM wrapper 在 LLM 调用前 recall、注入 system_message 或 prepend_user；调用后自动 retain 对话。

### 1.3 整理（consolidation）

- retain 完成后自动触发（默认开），后台 worker 执行；另有周期性 reconcile 重排失败项。
- 核心动作：将相关 facts 合并为带证据的 **observation**（proof_count + source_fact_ids，召回时按 token 预算回填原文）；新证据 refine 而非覆盖。
- 去重合并：embedding 近邻候选 + LLM merge/keep 裁决；时间边界取 min/max 折叠。
- **单条可逆退休**：MCP `invalidate_memory` 可对单条 world/experience fact 做软失效，旧事实仍可恢复，并触发派生 observation 重算；这是停止使用单条记忆的 Agent-facing 路径，不等于物理擦除。
- **存在硬删除面**：另有 bank/document/clear 等硬删除接口，但本次源码未核到面向 Agent 的单条 memory-unit 硬删除路径。不要把“有 invalidate”表述成“没有删除能力”，也不要把 bank/document 级清除误当作单条 forget。
- **recency decay 只排序**：linear/exponential/none 对检索结果做时间加权，不改变事实有效性或删除数据；本次核对未把它当作通用 retention cleaner。
- mental model 刷新：consolidation 后或 cron（full/delta 两种模式）；另有 graph/entity maintenance queue。

### 1.4 soul.md / memory.md 等价机制

- **有等价物，但无单一文件**：
  - **mental models**：定义问题 → LLM 生成答案 → 后台自动重写，读取是纯 DB 查询；
  - **knowledge pages**：wiki 式 folders/pages 树，可导出 markdown bundle，并可用 `hindsight fs mount` 把知识库**镜像为本地 markdown 文件夹**。
- 生成者均为 LLM，触发为 consolidation 后 / cron / 手动；另有 `directives` 硬规则表与 bank 级 disposition/reflect_mission 人格配置。

---

## 2. tencentdb-agent-memory

### 2.1 存入（L0 → L1 分层）

- **L0 原始对话**：`agent_end` 钩子触发，经清洗（去注入标签、去 base64 图、过滤短/命令消息）后**立即**按天写 JSONL（`conversations/YYYY-MM-DD.jsonl`），用「位置切片 + 时间戳游标」原子去重。证据链由此建立：L1 记录的 `source_message_ids` 指回 L0 消息 ID。
- **L1 原子记忆**：**不立即入库**，由调度器决定时机。LLM 单次调用做「场景分段 + 记忆抽取」，产出 content/type/priority（重要性分，默认 50）/source_message_ids；随后两阶段去重：先混合检索召回候选，再一次 LLM 批量判定 store/update。
- 表结构：`l1_records`（record_id/content/type/priority/scene_name/session·team·user·agent·task_id/version/时间戳/metadata_json）与 `l0_conversations`。

### 2.2 召回

- 三策略自动降级：**hybrid**（FTS5 BM25 + 向量并行，RRF k=60 融合）/ 纯向量 / 纯 FTS；TCVDB 后端用原生 dense+sparse+RRF。**无 LLM rerank**。
- **注入机制（本项最强）**：`before_prompt_build` 钩子——L1 记忆前置到用户 prompt（动态），L3 persona + L2 场景导航 + 工具指南追加到 system prompt（可缓存）；带字符预算截断，并注入「主动检索工具、每轮最多 3 次」的使用指南。另有 L0 原文搜索工具。

### 2.3 整理（管道调度）

- L1：**可重置空闲防抖定时器**（默认 60s）+ 对话数阈值（带 warm-up 阈值翻倍）。
- L2 场景抽取：**只提前不下推的定时器**（L1 完成后延迟 90s、最小间隔 900s、最大 3600s、24h 不活跃停止轮询）。
- L3 persona 在 L2 完成后触发；全局互斥 + pending 去重；停机时 flush。
- **有显式分层删除**：`/v3/atomic/delete` 删除指定 L1 IDs；conversation delete 删除 L0 会话/消息；`/v3/chat-memory/clear` 清理该 Agent 的 L0-L3 内容并保留资产数据。这些 API 的覆盖范围不同，不能统称为一种语义级单条 forget。
- **有可配置物理 retention**：配置启用后 daily cleaner 按记录时间清理 L0/L1；原始配置默认值为 0，短周期默认受保护。它是数据保留策略，不是召回排序的 recency decay；检索排序没有时间衰减乘数。
- 合并仍是 L1 冲突检测的 update 决策，记录带 version；这与显式删除/retention 是不同机制。

### 2.4 soul.md / memory.md 等价机制

- **有，且是核心**：
  - **`persona.md`（L3）**：LLM 四层深扫生成，带 3 份滚动备份（`.backup/persona/`）；聊天模式存 persona、code 模式存 Team Operating Doctrine；每轮召回注入 system prompt。
  - **`scene_blocks/{name}.md`（L2）**：LLM 生成的场景画像块，注入场景导航索引，Agent 按路径主动 `read_file` 读全文。

---

## 3. EverOS

### 3.1 存入（缓冲 → 边界 → 抽取）

- 消息**不直接入库**：`POST /add` 先暂存进 SQLite `unprocessed_buffer`（按 session/app/project 分区，message_id 主键天然去重）；boundary 检测触发或 `/flush` 强制时，**一次 LLM 调用**抽取 `MemCell` 落 SQLite `memcell` 表（`payload_json` 存完整原始对话存档）；UserPipeline 同步写 episode markdown（写盘即返回），AgentPipeline 异步发事件给 OME。
- 普通 episode/MemCell **无统一重要性或可信度分，也无逐字 span 证据要求**——内容主要由 LLM 提炼；融合去重按 message_id 去重排序。Agent 专用对象有独立指标：`agent_case.quality_score`，以及 `agent_skill.confidence`/`maturity_score`；这些不等于通用用户记忆评分。agent_case 另有「太薄的轨迹跳过」这类宽松准入。

### 3.2 召回

- 四种方法：KEYWORD / VECTOR / HYBRID / AGENTIC。HYBRID = 稀疏 + 稠密双路召回 → RRF 融合 → LR 校准全局 top-N 竞争（含 fact 驱逐）；AGENTIC 走内部 cross-encoder 多轮循环。
- 支持标量过滤（user/agent/app/project/session 正交）与向量 radius 阈值；召回池放大 2 倍、上限 100。默认无 LLM rerank（rerank 组件存在但非默认）。
- **没有自动 prompt 注入**——纯 API/工具返回，宿主自行决定（如 claude-code-plugin 的 `evermem_search` 工具）。

### 3.3 整理（OME 事件驱动 + 定时）

- OME 策略：extract_atomic_facts / foresight / user_profile / agent_case / agent_skill / 聚类触发。
- 反思合并：`reflect_episodes`（cron `0 2 * * 1`，默认关）把同类 episode 簇 LLM 合并成单篇，旧条目用 `deprecated_by` 标记软删。
- **`deprecated_by` 是自动整理软归档**：episode 反思合并后默认搜索排除旧条目，但旧 Markdown 仍保留；它不是用户主动单条 forget，也不是物理擦除。
- Knowledge 文档有 DELETE endpoint，但对象类型与 Episode 记忆不同；本次未见 Episode 的通用年龄衰减排序，也未把该文档 API 当作 episode forget。

### 3.4 soul.md / memory.md 等价机制

- **Markdown 即事实源**（Git 可版本化、人手/agent 可直接编辑，cascade watcher 自动重索引变更条目）。
- `ProfileWriter` **原生支持 `agent.md` / `soul.md` / `tools.md` / `behaviors.md` 单文件 persona 覆写**，但当前无任何 OME 策略产出 soul.md，**仅 `user.md` 被实际写入**（用户画像由 OME `extract_user_profile` 生成/改写，agent/用户也可直接改文件）。
- 无启动时自动注入，靠检索 profile（`include_profile=true`）或直接读文件。

---

## 4. 横向对比要点（与本项目相关）

1. **三级路径共识**：三个项目都是「暂存/原始档 → LLM 抽取原子记忆 → 后台整理」，与本项目 extract_v3/admit_v2 的「候选 → 准入」思路一致；无一项目把模型输出直接当最终记忆。
2. **准入严格度**：本项目（逐字 span + Rust 准入）比三者都严。三者都只有「去重 + 宽松过滤」，无证据定位硬约束——这正是本项目的差异化设计。
3. **召回融合**：RRF k=60 在三者中全部出现（hindsight、tencentdb hybrid、EverOS HYBRID），是事实标准；hindsight 额外多了图扩展（memory_links）和时间维度，是最全的召回面。
4. **Markdown 持久层**：EverOS 最激进（markdown 即事实源，soul.md 是一等公民但尚未自动生成）；tencentdb 的 persona.md 是三者中唯一「LLM 自动生成 + 自动注入 system prompt + 带备份」的完整闭环；hindsight 用 DB 侧 mental models + 文件系统镜像折中。
5. **遗忘、保留与衰减需拆开比较**：Hindsight 有单条可逆 invalidate、较粗粒度硬删除面和 retrieval recency decay；TencentDB 有 L1/L0/全 Agent 删除 API 与可配置的物理 retention，但无检索 recency boost；EverOS 的 `deprecated_by` 是合并软归档，另有 Knowledge 文档删除，不等于 Episode forget。三者不是“都没有遗忘”，其对象范围与语义不同；本项目组合方案见 [12](12-遗忘、过期与时间衰减方案决策.md)。

## 附录：关键文件索引

- hindsight：
  - `hindsight-api-slim/hindsight_api/alembic/versions/5a366d414dce_initial_schema.py`（初始 schema）
  - `hindsight-api-slim/hindsight_api/engine/retain/orchestrator.py`、`engine/consolidation/consolidator.py`
  - `hindsight-api-slim/hindsight_api/engine/search/retrieval.py`、`engine/search/fusion.py`、`engine/search/reranking.py`
  - `hindsight-api-slim/hindsight_api/engine/mental_model_refresh.py`、`engine/source_facts.py`
  - `hindsight-integrations/litellm/README.md`（recall/retain 注入）
  - `hindsight-cli/src/main.rs`、`src/commands/fs/sync.rs`（markdown 镜像）
- tencentdb-agent-memory：
  - `MemoryCore/src/core/conversation/l0-recorder.ts`、`core/hooks/auto-capture.ts`、`core/hooks/auto-recall.ts`
  - `MemoryCore/src/core/record/l1-extractor.ts`、`record/l1-dedup.ts`
  - `MemoryCore/src/core/store/sqlite/memory-store.ts`、`store/search-utils.ts`
  - `MemoryCore/src/utils/pipeline-manager.ts`（调度管道）
  - `MemoryCore/src/core/persona/persona-generator.ts`、`storage/types.ts`（persona.md）
- EverOS：
  - `docs/how-memory-works.md`（全流程权威叙述）、`docs/storage_layout.md`、`docs/architecture.md`
  - `src/everos/service/_boundary.py`（缓冲/边界/台账）
  - `src/everos/memory/search/manager.py`（检索编排/融合）
  - `src/everos/memory/strategies/`（OME 策略）、`strategies/reflect_episodes.py`
  - `src/everos/infra/persistence/markdown/writers/profile_writer.py`（soul.md 能力）

## 附录 B：遗忘、硬删除与衰减的源码定位（2026-09-26 复核）

- Hindsight 可逆单条失效：`hindsight-api-slim/hindsight_api/mcp_tools.py` 的 `invalidate_memory`；失效/恢复与派生 observation 更新：`hindsight-api-slim/hindsight_api/engine/memory_engine.py` 的 `update_memory_unit`。recency 计算：`hindsight-api-slim/hindsight_api/engine/search/reranking.py`。bank/document/clear 的硬删除路由：`hindsight-api-slim/hindsight_api/api/http.py`；这些粗粒度删除接口不等于 agent-facing 单条 invalidate。
- TencentDB 删除接口：`MemoryCore/v3-api-memorycore-doc.md` 的 atomic delete 与 chat-memory clear；实际处理在 `MemoryCore/src/gateway/v2-router.ts` 的 atomic delete 和 conversation delete handlers。清理器初始化与默认关闭判断在 `MemoryCore/index.ts`；按保留期清理在 `MemoryCore/src/utils/memory-cleaner.ts`；TCVDB L1 删除在 `MemoryCore/src/core/store/tcvdb/memory-store.ts`。查询 RRF 实现见同文件的 search 路径，不含 recency multiplier。
- EverOS 自动合并归档：`docs/reflection.md` 的 `reflect_episodes` 与 `deprecated_by`；默认排除归档记录见 `src/everos/memory/search/filters.py`。Knowledge 文档 DELETE 路由是另一种对象，不能推断为 Episode 的 forget API。
  - `src/everos/infra/persistence/sqlite/tables/`（memcell、unprocessed_buffer 等 8 张表）
