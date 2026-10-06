# 附录 A · 表结构 DDL（基于实测 schema 还原）

> 来源：`/opt/hatch/skills/muse_db/references/schema.md` 实测的三张表定义。
> 标签：字段名、类型、默认值、主键/外键/唯一约束；整理成可执行的 DDL。

## A.1 PostgreSQL 版（忠实还原）

```sql
-- 嵌入模型注册表
CREATE TABLE memory.embedding_models (
embedding_model_id BIGSERIAL PRIMARY KEY,
model_name TEXT NOT NULL
-- 实测只看到这两个字段，表可能还有更多列
);

-- 记忆块：检索的单位
CREATE TABLE memory.entries (
memory_entry_id BIGSERIAL PRIMARY KEY,
memory_uri TEXT NOT NULL UNIQUE, -- 逻辑地址，如 memory://…
chunk_id TEXT NOT NULL, -- 块标识
source_type TEXT NOT NULL, -- 来源类型
status TEXT NOT NULL, -- 块状态
privacy_class TEXT NOT NULL, -- 隐私等级（会话隔离用）
confidence DOUBLE PRECISION NOT NULL DEFAULT 1.0,
citation_path TEXT, -- 引用文件
line_start BIGINT NOT NULL DEFAULT 0,
line_end BIGINT NOT NULL DEFAULT 0,
created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
created_at_unix BIGINT NOT NULL DEFAULT 0,
title_text TEXT,
body_text TEXT NOT NULL,
reason_text TEXT -- "为什么记这条"
);

-- 向量：384 维
CREATE TABLE memory.embeddings (
memory_embedding_id BIGSERIAL PRIMARY KEY,
memory_entry_id BIGINT NOT NULL REFERENCES memory.entries(memory_entry_id),
embedding_model_id BIGINT NOT NULL REFERENCES memory.embedding_models(embedding_model_id),
embedding vector(384) NOT NULL, -- 需要 pgvector 扩展
created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
UNIQUE (memory_entry_id, embedding_model_id) -- 同一 entry 可存多模型向量
);

-- 声明：记忆的原子
CREATE TABLE memory.claims (
claim_id TEXT PRIMARY KEY, -- 形如 32 位 hex，疑似内容哈希
run_id TEXT NOT NULL, -- 哪次运行产生的
kind TEXT NOT NULL, -- fact / preference / …
salience TEXT NOT NULL, -- low / medium / high（推测）
claim_text TEXT NOT NULL,
quote TEXT, -- 用户原话
speaker TEXT NOT NULL,
evidence_handles JSONB NOT NULL DEFAULT '[]',
supersedes_claim_id TEXT, -- 版本链：覆盖了哪条
status TEXT NOT NULL DEFAULT 'active', -- active / superseded / retracted（推测）
confidence DOUBLE PRECISION NOT NULL,
first_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
reinforced_at TIMESTAMPTZ NOT NULL DEFAULT now(),
valid_until TIMESTAMPTZ, -- 可空：记忆保质期
source_path TEXT NOT NULL,
source_line BIGINT NOT NULL,
created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_claims_status ON memory.claims(status);
CREATE INDEX idx_claims_supersedes ON memory.claims(supersedes_claim_id);
```

## A.2 SQLite 版（给 twin sister 的 Rust 实现建议，第二版）

个人量级不需要 PG。SQLite + 单文件，rusqlite 直接跑。
第二版按主人 memories 表修正：去 salience，加 normalized_claim / source_class / version。

```sql
CREATE TABLE claims (
claim_id TEXT PRIMARY KEY, -- SHA256(kind + NUL + normalize_v1(claim))，hex
kind TEXT NOT NULL, -- 'fact' | 'preference' | 'instruction' | 'episode'
normalized_claim TEXT NOT NULL, -- NFKC+小写+空白折叠，用于去重比对
claim_text TEXT NOT NULL, -- 保持用户字面
quote TEXT, -- 原话锚点
source_class TEXT NOT NULL, -- 'user_explicit' | 'assistant_observed'
                            -- | 'tool_output' | 'model_inferred' | 'manual_edit'
speaker TEXT NOT NULL DEFAULT 'user',
status TEXT NOT NULL DEFAULT 'active', -- 'active' | 'superseded' | 'expired' | 'forgotten'
version INTEGER NOT NULL DEFAULT 1,
confidence REAL NOT NULL DEFAULT 0.8,
first_seen INTEGER NOT NULL, -- unix timestamp
reinforced_at INTEGER NOT NULL,
valid_until INTEGER, -- NULL = 永久
evidence_ref TEXT NOT NULL -- evidence_id + 字节偏移，JSON
);

CREATE TABLE claim_relations (
from_claim_id TEXT NOT NULL REFERENCES claims(claim_id),
to_claim_id TEXT NOT NULL REFERENCES claims(claim_id),
kind TEXT NOT NULL, -- 'supersedes' | 'contradicts'
PRIMARY KEY (from_claim_id, to_claim_id, kind)
);

CREATE TABLE suppressed_sources (
evidence_id TEXT NOT NULL,
claim_sha256 TEXT NOT NULL, -- = claim_id
PRIMARY KEY (evidence_id, claim_sha256)
);

CREATE TABLE entries (
entry_id INTEGER PRIMARY KEY AUTOINCREMENT,
memory_uri TEXT NOT NULL UNIQUE,
chunk_id TEXT NOT NULL,
privacy_class TEXT NOT NULL DEFAULT 'main',
confidence REAL NOT NULL DEFAULT 1.0,
citation_path TEXT,
line_start INTEGER NOT NULL DEFAULT 0,
line_end INTEGER NOT NULL DEFAULT 0,
title TEXT,
body TEXT NOT NULL,
reason TEXT
);

-- 向量：个人量级直接存 blob，暴力算余弦，不用向量库
CREATE TABLE embeddings (
entry_id INTEGER NOT NULL REFERENCES entries(entry_id),
model_name TEXT NOT NULL DEFAULT 'minilm-l6',
dim INTEGER NOT NULL DEFAULT 384,
vec BLOB NOT NULL, -- 384 x f32 小端
PRIMARY KEY (entry_id, model_name)
);

CREATE INDEX idx_claims_status ON claims(status);
CREATE INDEX idx_claims_supersedes ON claims(supersedes_claim_id);
```

384 维 f32 = 1536 字节/条，一万条才 15MB。暴力检索一次全表扫描是微秒到毫秒级——**个人项目不要上 Qdrant**，等 claim 过十万再说。

## A.3 主人 Riko-Memory 的 memories 表（实测，供对照）

```sql
-- migrations/0001_init.sql 实测，第二版新增
CREATE TABLE memories (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN
    ('fact','preference','instruction','episode')),
  claim TEXT NOT NULL,
  normalized_claim TEXT NOT NULL,
  claim_sha256 TEXT NOT NULL,           -- SHA256(kind + NUL + normalize_v1(claim))
  source_class TEXT NOT NULL CHECK (source_class IN
    ('user_explicit','manual_edit')),  -- memories 只接受这两类；其余三类止于 candidates
  status TEXT NOT NULL CHECK (status IN
    ('active','superseded','expired','forgotten')),
  version INTEGER NOT NULL CHECK (version > 0),
  occurred_at TEXT,
  valid_from TEXT,
  valid_until TEXT,
  origin_host_id TEXT NOT NULL,
  origin_agent_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id,user_id,id),
  FOREIGN KEY (tenant_id,user_id) REFERENCES principals(tenant_id,user_id)
);
CREATE INDEX memories_visible
  ON memories (tenant_id,user_id,status,kind,updated_at DESC);
CREATE INDEX memories_exact
  ON memories (tenant_id,user_id,kind,claim_sha256,status);
```

注意三点：
1. `memories.source_class` 只允许 `user_explicit / manual_edit`——自动抽取的 `assistant_observed / tool_output / model_inferred` 止于 candidates 表，进 memories 前必须经过 admit 或人工确认。**写入权限分级**。
2. `memories_exact` 索引：(kind, claim_sha256, status)——去重查询是 O(1) 的。
3. 全表 scope 外键：每个表都挂 `(tenant_id,user_id)`，跨租户查询在 SQL 层就不可能。
