-- D6-7（doc6/02 §5）：Dream 触发、冻结输入与处理账本。
-- 只新增表，不改 0001—0007；不回改 evidence_events/extraction_jobs。
-- 新 Dream 路径的候选归属 dream_jobs，绝不写入旧 memory_candidates
-- （其 job_id FK 指向 extraction_jobs，doc6/02 §5）。

-- Dream 作业（doc6/02 §5）：trigger_key 幂等（同 key 重放返回原 job）；
-- 状态机 queued|running|succeeded|retryable_failed|provider_wait|dead|stale_input；
-- extract_version 独立版本化（dream_extract_v1），旧 extract_v1/v2/v3 不用于 Dream。
CREATE TABLE dream_jobs (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  trigger_kind TEXT NOT NULL CHECK (trigger_kind IN ('compact','scheduled','custom','manual')),
  trigger_key TEXT NOT NULL,
  agent_id TEXT,
  host_id TEXT,
  session_id TEXT,
  trigger_event_id TEXT,
  pipeline_version TEXT NOT NULL,
  extract_version TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN
    ('queued','running','succeeded','retryable_failed','provider_wait','dead','stale_input')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  run_after TEXT NOT NULL,
  lease_until TEXT,
  claim_generation INTEGER NOT NULL DEFAULT 0 CHECK (claim_generation >= 0),
  input_fingerprint TEXT NOT NULL,
  model_name TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  error_code TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, id),
  UNIQUE (tenant_id, user_id, trigger_key),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
CREATE INDEX dream_jobs_claim
  ON dream_jobs (status, run_after, lease_until);

-- 冻结输入（doc6/02 §5）：job 实际读取的 event IDs/role/host/session/event_seq/
-- content hash 与顺序；重试仍用同一列表，不得重新查询"当前所有新事件"。
CREATE TABLE dream_job_inputs (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  input_order INTEGER NOT NULL CHECK (input_order >= 0),
  evidence_id TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('user','assistant','tool','system')),
  host_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  event_seq INTEGER NOT NULL CHECK (event_seq >= 0),
  content_sha256 TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, job_id, evidence_id),
  FOREIGN KEY (tenant_id, user_id, job_id)
    REFERENCES dream_jobs(tenant_id, user_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, user_id, evidence_id)
    REFERENCES evidence_events(tenant_id, user_id, id)
);

-- 处理账本（doc6/02 §5）：每个用户 evidence 在该 Dream pipeline 中
-- pending|assigned|processed 并关联唯一 active job；同一证据不能同时分给两个 job。
CREATE TABLE dream_evidence_state (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  evidence_id TEXT NOT NULL,
  pipeline_version TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending','assigned','processed')),
  active_job_id TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, evidence_id, pipeline_version),
  FOREIGN KEY (tenant_id, user_id, evidence_id)
    REFERENCES evidence_events(tenant_id, user_id, id)
);

-- Dream 候选（doc6/02 §5）：candidate ID 由 Rust 生成，模型只引用该批给定 ID；
-- 成功的 defer/conflict 留 Held 及来源版本。
CREATE TABLE dream_candidates (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  dream_job_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('fact','preference','instruction','episode')),
  claim TEXT NOT NULL,
  quote TEXT NOT NULL,
  quote_sha256 TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('candidate','held','rejected','committed')),
  reason_code TEXT,
  policy_version TEXT NOT NULL,
  occurred_at TEXT,
  created_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, id),
  UNIQUE (tenant_id, user_id, dream_job_id, kind, quote_sha256),
  FOREIGN KEY (tenant_id, user_id, dream_job_id)
    REFERENCES dream_jobs(tenant_id, user_id, id) ON DELETE CASCADE
);

-- 候选到冻结 user event 的逐字 byte span（doc6/02 §5；Rust 校验后落库）。
CREATE TABLE dream_candidate_evidence (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  candidate_id TEXT NOT NULL,
  evidence_id TEXT NOT NULL,
  start_byte INTEGER NOT NULL CHECK (start_byte >= 0),
  end_byte INTEGER NOT NULL CHECK (end_byte > start_byte),
  PRIMARY KEY (tenant_id, user_id, candidate_id, evidence_id),
  FOREIGN KEY (tenant_id, user_id, candidate_id)
    REFERENCES dream_candidates(tenant_id, user_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, user_id, evidence_id)
    REFERENCES evidence_events(tenant_id, user_id, id)
);
