# 02 · 源证据：数据库 Schema（memory.* 全表）

> [源-Schema] 以下为 `/opt/hatch/skills/muse_db/references/schema.md` 中 `memory.*` 六张表的**完整字段表**，一字未改。表名后的中文为璃的解读（[推理]）。

## 2.1 `memory.claims`——原子命题表

| Column | Type | Nullable | 含义 |
|---|---|---|---|
| `claim_id` | text PK | no | 主键 |
| `run_id` | text | no | 关联标识（无外键声明） |
| `kind` | text | no | 命题类型 |
| `salience` | text | no | 显著度（注意：Riko-Memory 源码实测**无此字段**，已证伪，twin 不做） |
| `claim_text` | text | no | **改写后的命题正文** |
| `quote` | text | yes | **原文引用（改写前的逐字 quote）** |
| `speaker` | text | no | 说话人 |
| `evidence_handles` | jsonb | no | 证据句柄数组 |
| `supersedes_claim_id` | text | yes | 被替代的旧 claim |
| `status` | text | no | 默认 'active' |
| `confidence` | double | no | 置信度 |
| `first_seen` | timestamptz | no | 首次见到 |
| `reinforced_at` | timestamptz | no | 最近一次被强化 |
| `valid_until` | timestamptz | yes | 有效期（M1 已实现自动转 expired） |
| `source_path` | text | no | 来源文件路径 |
| `source_line` | bigint | no | 来源文件行号 |
| `created_at` / `updated_at` | timestamptz | no | |

**关键发现**：一条 claim 同时存 `claim_text`（改写后）+ `quote`（改写前原文）+ `speaker`。这正是附录 D rewrite 契约的设计——**源证据直接验证了设计**（见 07 推理链 R-1）。

## 2.2 `memory.entries`——记忆条目（chunk 级）

| Column | Type | Nullable | 含义 |
|---|---|---|---|
| `memory_entry_id` | bigint PK | no | 主键 |
| `memory_uri` | text UNIQUE | no | 记忆 URI（全局唯一寻址） |
| `chunk_id` | text | no | chunk 身份 |
| `source_type` | text | no | 来源类型 |
| `status` | text | no | 状态 |
| `privacy_class` | text | no | **隐私分级**（Riko-Memory 只有 tenant 级 principals，无 entry 级分级） |
| `confidence` | double | no | 默认 1.0 |
| `citation_path` | text | yes | 引用路径 |
| `line_start` / `line_end` | bigint | no | 行范围 |
| `title_text` | text | yes | 标题 |
| `body_text` | text | no | 正文 |
| `reason_text` | text | yes | **记录原因**（为什么记这条） |

**关键发现**：entries 是 chunk 级（有标题+正文+行范围），claims 是命题级。两层结构：entry（上下文块）→ claims（原子命题）。

## 2.3 `memory.embeddings`——向量索引

| Column | Type | Nullable | 含义 |
|---|---|---|---|
| `memory_embedding_id` | bigint PK | no | 主键 |
| `memory_entry_id` | bigint FK | no | → entries（**注意：挂在 entry 上，不是 claim 上**） |
| `embedding_model_id` | bigint FK | no | → embedding_models |
| `embedding` | vector(384) | no | 384 维向量 |
| UNIQUE | | | (memory_entry_id, embedding_model_id) |

**关键发现**：向量索引的粒度是 **entry（chunk）级**，不是 claim 级。检索返回的是带上下文的块，再取其中的 claims。Riko-Memory 的 semantic_vectors 粒度需在 V2 对齐（见 V2 文档 05）。

## 2.4 `memory.embedding_models`——向量模型注册表

| Column | 含义 |
|---|---|
| `embedding_model_id` PK | 主键 |
| `model_name` / `dimensions` / `distance_metric` | 模型名、维度、距离度量 |
| UNIQUE | (model_name, dimensions, distance_metric) |

多模型并存，有唯一约束防重复注册。

## 2.5 `memory.entry_attributes`——条目扩展属性

| Column | 含义 |
|---|---|
| `memory_entry_attribute_id` PK | 主键 |
| `memory_entry_id` FK | → entries |
| `attribute_name` | 属性名 |
| `scalar_value` | 属性值（文本） |
| UNIQUE | (memory_entry_id, attribute_name) |

**关键发现**：每 entry 的 KV 扩展表。Riko-Memory 无对应物（见遗漏清单）。

## 2.6 `memory.metadata`——系统元数据

| Column | 含义 |
|---|---|
| `key` PK / `value` | KV |
| `updated_at` | 更新时间 |

系统级 KV（如版本号、水位线）。
