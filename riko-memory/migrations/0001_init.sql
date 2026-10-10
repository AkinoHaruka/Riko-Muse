-- 首版规范存储蓝图（doc/11）。实现可调整排版与索引名，
-- 不得改字段含义、唯一键、scope 外键与状态集。
-- 前三条 PRAGMA 由迁移执行器在事务外执行。
CREATE TABLE schema_migrations (
  version INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  applied_at TEXT NOT NULL
);

CREATE TABLE principals (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  token_sha256 TEXT NOT NULL UNIQUE,
  status TEXT NOT NULL CHECK (status IN ('active','disabled')),
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id,user_id)
);

CREATE TABLE evidence_events (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  host_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  event_seq INTEGER NOT NULL CHECK (event_seq >= 0),
  role TEXT NOT NULL CHECK (role IN ('user','assistant','tool','system')),
  source_kind TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  received_at TEXT NOT NULL,
  content TEXT NOT NULL,
  content_sha256 TEXT NOT NULL,
  UNIQUE (tenant_id,user_id,id),
  UNIQUE (tenant_id,user_id,host_id,session_id,event_seq),
  FOREIGN KEY (tenant_id,user_id) REFERENCES principals(tenant_id,user_id)
);
CREATE INDEX evidence_by_session
  ON evidence_events (tenant_id,user_id,host_id,session_id,event_seq);

CREATE TABLE extraction_jobs (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  host_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  window_key TEXT NOT NULL,
  through_event_seq INTEGER NOT NULL CHECK (through_event_seq >= 0),
  status TEXT NOT NULL CHECK (status IN
    ('queued','running','succeeded','retryable_failed','dead')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  run_after TEXT NOT NULL,
  lease_until TEXT,
  model_name TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  error_code TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id,user_id,id),
  UNIQUE (tenant_id,user_id,host_id,session_id,window_key),
  FOREIGN KEY (tenant_id,user_id) REFERENCES principals(tenant_id,user_id)
);
CREATE INDEX jobs_claim
  ON extraction_jobs (status,run_after,lease_until);

CREATE TABLE memory_candidates (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  primary_evidence_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN
    ('fact','preference','instruction','episode')),
  quote TEXT NOT NULL,
  quote_sha256 TEXT NOT NULL,
  claim TEXT NOT NULL,
  source_class TEXT NOT NULL CHECK (source_class IN
    ('user_explicit','assistant_observed','tool_output','model_inferred','manual_edit')),
  status TEXT NOT NULL CHECK (status IN ('candidate','held','rejected')),
  reason_code TEXT,
  model_confidence REAL,
  occurred_at TEXT,
  valid_until TEXT,
  created_at TEXT NOT NULL,
  UNIQUE (tenant_id,user_id,id),
  UNIQUE (tenant_id,user_id,job_id,primary_evidence_id,kind,quote_sha256),
  FOREIGN KEY (tenant_id,user_id) REFERENCES principals(tenant_id,user_id),
  FOREIGN KEY (tenant_id,user_id,job_id)
    REFERENCES extraction_jobs(tenant_id,user_id,id),
  FOREIGN KEY (tenant_id,user_id,primary_evidence_id)
    REFERENCES evidence_events(tenant_id,user_id,id)
);
CREATE INDEX candidates_by_status
  ON memory_candidates (tenant_id,user_id,status,created_at);

CREATE TABLE candidate_evidence (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  candidate_id TEXT NOT NULL,
  evidence_id TEXT NOT NULL,
  start_byte INTEGER,
  end_byte INTEGER,
  CHECK ((start_byte IS NULL AND end_byte IS NULL) OR
         (start_byte >= 0 AND end_byte > start_byte)),
  FOREIGN KEY (tenant_id,user_id,candidate_id)
    REFERENCES memory_candidates(tenant_id,user_id,id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id,user_id,evidence_id)
    REFERENCES evidence_events(tenant_id,user_id,id)
);
CREATE UNIQUE INDEX candidate_evidence_unique
  ON candidate_evidence (tenant_id,user_id,candidate_id,evidence_id,
    COALESCE(start_byte,-1),COALESCE(end_byte,-1));

CREATE TABLE memories (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN
    ('fact','preference','instruction','episode')),
  claim TEXT NOT NULL,
  normalized_claim TEXT NOT NULL,
  claim_sha256 TEXT NOT NULL,
  source_class TEXT NOT NULL CHECK (source_class IN
    ('user_explicit','manual_edit')),
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

CREATE TABLE memory_evidence (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  memory_id TEXT NOT NULL,
  evidence_id TEXT NOT NULL,
  start_byte INTEGER,
  end_byte INTEGER,
  CHECK ((start_byte IS NULL AND end_byte IS NULL) OR
         (start_byte >= 0 AND end_byte > start_byte)),
  FOREIGN KEY (tenant_id,user_id,memory_id)
    REFERENCES memories(tenant_id,user_id,id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id,user_id,evidence_id)
    REFERENCES evidence_events(tenant_id,user_id,id)
);
CREATE UNIQUE INDEX memory_evidence_unique
  ON memory_evidence (tenant_id,user_id,memory_id,evidence_id,
    COALESCE(start_byte,-1),COALESCE(end_byte,-1));

CREATE TABLE memory_revisions (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  memory_id TEXT NOT NULL,
  version INTEGER NOT NULL,
  previous_claim TEXT,
  new_claim TEXT NOT NULL,
  previous_status TEXT,
  new_status TEXT NOT NULL,
  actor_kind TEXT NOT NULL CHECK (actor_kind IN ('user','system')),
  actor_id TEXT NOT NULL,
  reason_code TEXT NOT NULL,
  changed_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id,user_id,memory_id,version),
  FOREIGN KEY (tenant_id,user_id,memory_id)
    REFERENCES memories(tenant_id,user_id,id)
);

CREATE TABLE memory_relations (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  from_memory_id TEXT NOT NULL,
  to_memory_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('supersedes','contradicts')),
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id,user_id,from_memory_id,to_memory_id,kind),
  FOREIGN KEY (tenant_id,user_id,from_memory_id)
    REFERENCES memories(tenant_id,user_id,id),
  FOREIGN KEY (tenant_id,user_id,to_memory_id)
    REFERENCES memories(tenant_id,user_id,id)
);

CREATE TABLE suppressed_sources (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  evidence_id TEXT NOT NULL,
  claim_sha256 TEXT NOT NULL,
  forgotten_memory_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id,user_id,evidence_id,claim_sha256),
  FOREIGN KEY (tenant_id,user_id,evidence_id)
    REFERENCES evidence_events(tenant_id,user_id,id),
  FOREIGN KEY (tenant_id,user_id,forgotten_memory_id)
    REFERENCES memories(tenant_id,user_id,id)
);

CREATE TABLE audit_events (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  actor_kind TEXT NOT NULL,
  actor_id TEXT NOT NULL,
  action TEXT NOT NULL,
  target_id TEXT,
  occurred_at TEXT NOT NULL,
  detail_json TEXT NOT NULL,
  FOREIGN KEY (tenant_id,user_id) REFERENCES principals(tenant_id,user_id)
);
CREATE INDEX audit_by_scope
  ON audit_events (tenant_id,user_id,occurred_at DESC);

CREATE TABLE index_state (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  generation INTEGER NOT NULL CHECK (generation >= 0),
  dirty INTEGER NOT NULL CHECK (dirty IN (0,1)),
  updated_at TEXT NOT NULL
);

CREATE VIRTUAL TABLE memory_fts USING fts5(
  memory_id UNINDEXED, claim, tokenize = 'unicode61'
);

CREATE TABLE memory_grams (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  memory_id TEXT NOT NULL,
  gram TEXT NOT NULL,
  PRIMARY KEY (tenant_id,user_id,memory_id,gram),
  FOREIGN KEY (tenant_id,user_id,memory_id)
    REFERENCES memories(tenant_id,user_id,id) ON DELETE CASCADE
);
CREATE INDEX grams_lookup
  ON memory_grams (tenant_id,user_id,gram,memory_id);
