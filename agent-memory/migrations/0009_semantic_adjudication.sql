-- D6-8（doc6/02 §6）：版本化语义裁决持久层（admit_v3 / adjudicate_v1）。
-- 本迁移只新增表与索引，不改 0001—0008；不复用或改写历史 admit_v1/v2 作业语义。
-- 裁决作业独立于旧 extraction_jobs 与主题页 consolidation_jobs；
-- dream_job_id 指向同 scope 的 dream_jobs，绝无 extraction_job_id 外键。

CREATE TABLE adjudication_jobs (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  dream_job_id TEXT NOT NULL,
  input_fingerprint TEXT NOT NULL,
  admission_version TEXT NOT NULL,
  adjudication_version TEXT NOT NULL,
  embedding_model_id TEXT,
  status TEXT NOT NULL CHECK (status IN
    ('queued','running','succeeded','retryable_failed','provider_wait','dead','stale_input')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  run_after TEXT NOT NULL,
  lease_until TEXT,
  claim_generation INTEGER NOT NULL DEFAULT 0 CHECK (claim_generation >= 0),
  model_name TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  error_code TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id),
  FOREIGN KEY (tenant_id, user_id, dream_job_id)
    REFERENCES dream_jobs(tenant_id, user_id, id)
);
CREATE INDEX adjudication_jobs_claim
  ON adjudication_jobs (status, run_after);
CREATE INDEX adjudication_jobs_by_dream
  ON adjudication_jobs (tenant_id, user_id, dream_job_id);
-- 同 scope、同输入指纹和策略版本幂等（doc6/02 §6）。
CREATE UNIQUE INDEX adjudication_jobs_fingerprint_unique
  ON adjudication_jobs (tenant_id, user_id, dream_job_id, input_fingerprint, adjudication_version);

-- 冻结输入（doc6/02 §6）：本次 dream_candidates IDs（可含同 scope 旧 Held ID）、
-- evidence IDs 与 UTF-8 byte span。重试不能换输入。
CREATE TABLE adjudication_job_inputs (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  candidate_id TEXT NOT NULL,
  evidence_id TEXT NOT NULL,
  start_byte INTEGER NOT NULL CHECK (start_byte >= 0),
  end_byte INTEGER NOT NULL CHECK (end_byte > start_byte),
  input_order INTEGER NOT NULL,
  PRIMARY KEY (tenant_id, user_id, job_id, candidate_id, evidence_id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id),
  FOREIGN KEY (tenant_id, user_id, job_id)
    REFERENCES adjudication_jobs(tenant_id, user_id, id),
  FOREIGN KEY (tenant_id, user_id, candidate_id)
    REFERENCES dream_candidates(tenant_id, user_id, id),
  FOREIGN KEY (tenant_id, user_id, evidence_id)
    REFERENCES evidence_events(tenant_id, user_id, id)
);

-- 冻结召回（doc6/02 §6）：召回 target memory IDs/versions。target 的跨表 FK 无法
-- 单独证明 scope，Rust 每次读取和写入都须同 scope 核验。
CREATE TABLE adjudication_job_recalls (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  candidate_id TEXT NOT NULL,
  target_memory_id TEXT NOT NULL,
  target_version INTEGER NOT NULL CHECK (target_version > 0),
  recall_channel TEXT NOT NULL CHECK (recall_channel IN ('exact','lexical','semantic')),
  PRIMARY KEY (tenant_id, user_id, job_id, candidate_id, target_memory_id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id),
  FOREIGN KEY (tenant_id, user_id, job_id)
    REFERENCES adjudication_jobs(tenant_id, user_id, id)
);

-- 规范裁决结果（doc6/02 §6）：按 (scope, job, candidate) 唯一；只持久化规范结果，
-- 不持久化完整 prompt 或原始模型响应。confidence 仅诊断，不能单独放行。
CREATE TABLE adjudication_results (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  candidate_id TEXT NOT NULL,
  durability TEXT NOT NULL CHECK (durability IN
    ('durable','time_bound','uncertain','not_memory')),
  action TEXT NOT NULL CHECK (action IN
    ('create','attach_evidence','update','keep_separate','conflict','defer','not_memory')),
  reason_code TEXT,
  target_memory_id TEXT,
  expected_target_version INTEGER,
  applied_result_memory_id TEXT,
  application_status TEXT NOT NULL CHECK (application_status IN
    ('pending','applied','rejected','stale')),
  model_confidence REAL,
  valid_until TEXT,
  created_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, job_id, candidate_id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id),
  FOREIGN KEY (tenant_id, user_id, job_id)
    REFERENCES adjudication_jobs(tenant_id, user_id, id)
);
CREATE INDEX adjudication_results_by_candidate
  ON adjudication_results (tenant_id, user_id, candidate_id, created_at DESC);
