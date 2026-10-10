-- doc7/10（V2-Q1，分支 Riko-Muse）：上下文条目（entry）与检索对照。
-- 0001—0018 冻结不动。entry 从可见 L0 用户事件确定性切块，body 逐字、逐条来源可定位。
-- 依据：doc7/10 §1—§3；Muse-V2迭代开发文档/04 §3—§4。

CREATE TABLE context_entries (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL DEFAULT 'user_main',
  host_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL,
  source_type TEXT NOT NULL CHECK (source_type IN ('l0_window')),
  first_event_seq INTEGER NOT NULL CHECK (first_event_seq >= 0),
  last_event_seq INTEGER NOT NULL CHECK (last_event_seq >= first_event_seq),
  version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
  status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','stale','removed')),
  generator_version TEXT NOT NULL,
  source_fingerprint TEXT NOT NULL,
  batch_version INTEGER NOT NULL CHECK (batch_version > 0),
  generated_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  -- entry_sources 的组合外键要求父表在 (tenant,user,id) 上有唯一约束。
  UNIQUE (tenant_id, user_id, id),
  UNIQUE (tenant_id, user_id, domain_id, session_id, first_event_seq),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
CREATE INDEX context_entries_by_scope
  ON context_entries (tenant_id, user_id, domain_id, status, session_id);

-- 逐条来源：字节 span 指向 evidence_events.content；同时记该事件派生出的原子记忆版本，
-- 便于记忆一被更正就即时屏蔽该条目。
CREATE TABLE entry_sources (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  entry_id TEXT NOT NULL,
  evidence_id TEXT NOT NULL,
  start_byte INTEGER NOT NULL CHECK (start_byte >= 0),
  end_byte INTEGER NOT NULL CHECK (end_byte > start_byte),
  content_sha256 TEXT NOT NULL,
  memory_id TEXT,
  memory_version INTEGER,
  claim_sha256 TEXT,
  FOREIGN KEY (tenant_id, user_id, entry_id)
    REFERENCES context_entries(tenant_id, user_id, id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX entry_sources_unique
  ON entry_sources (tenant_id, user_id, entry_id, evidence_id, COALESCE(memory_id,''));
CREATE INDEX entry_sources_by_evidence
  ON entry_sources (tenant_id, user_id, evidence_id);
CREATE INDEX entry_sources_by_memory
  ON entry_sources (tenant_id, user_id, memory_id);

-- entry 的词法索引：与 page 同形（FTS5 拉丁 token + CJK 二元字）。
CREATE VIRTUAL TABLE entry_fts USING fts5(
  entry_id UNINDEXED,
  title,
  body,
  tokenize = 'unicode61'
);
CREATE TABLE entry_grams (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  entry_id TEXT NOT NULL,
  gram TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, entry_id, gram)
);

-- 向量对象扩到 entry：0008 的 CHECK 不能原地改，重建两张小表后复制旧行。
CREATE TABLE semantic_vectors_with_entry (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  object_kind TEXT NOT NULL CHECK (object_kind IN ('memory','page','entry')),
  object_id TEXT NOT NULL,
  model_id TEXT NOT NULL,
  source_version INTEGER NOT NULL CHECK (source_version > 0),
  content_sha256 TEXT NOT NULL,
  dimensions INTEGER NOT NULL CHECK (dimensions > 0),
  vector_blob BLOB NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('ready','stale')),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, object_kind, object_id, model_id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
INSERT INTO semantic_vectors_with_entry
  SELECT tenant_id, user_id, object_kind, object_id, model_id, source_version,
         content_sha256, dimensions, vector_blob, status, created_at, updated_at
  FROM semantic_vectors;
DROP TABLE semantic_vectors;
ALTER TABLE semantic_vectors_with_entry RENAME TO semantic_vectors;
CREATE INDEX semantic_vectors_scan
  ON semantic_vectors (tenant_id, user_id, object_kind, model_id, status);

CREATE TABLE semantic_jobs_with_entry (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  object_kind TEXT NOT NULL CHECK (object_kind IN ('memory','page','entry')),
  object_id TEXT NOT NULL,
  source_version INTEGER NOT NULL CHECK (source_version > 0),
  content_sha256 TEXT NOT NULL,
  model_id TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN
    ('queued','running','succeeded','retryable_failed','provider_wait','dead','stale_input')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  run_after TEXT NOT NULL,
  lease_until TEXT,
  claim_generation INTEGER NOT NULL DEFAULT 0 CHECK (claim_generation >= 0),
  error_code TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
INSERT INTO semantic_jobs_with_entry
  SELECT id, tenant_id, user_id, object_kind, object_id, source_version, content_sha256,
         model_id, status, attempts, run_after, lease_until, claim_generation, error_code,
         created_at, updated_at
  FROM semantic_jobs;
DROP TABLE semantic_jobs;
ALTER TABLE semantic_jobs_with_entry RENAME TO semantic_jobs;
CREATE INDEX semantic_jobs_claim
  ON semantic_jobs (status, run_after);
CREATE UNIQUE INDEX semantic_jobs_pending_unique
  ON semantic_jobs (tenant_id, user_id, object_kind, object_id, model_id)
  WHERE status IN ('queued','running','retryable_failed','provider_wait');
