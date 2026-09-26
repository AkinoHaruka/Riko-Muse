-- D6 reliability closure: persistent DSH runner presence, Dream-to-consolidation
-- association, and one-time Held-candidate redecision provenance. Migrations 0001-0010
-- remain immutable.

ALTER TABLE dream_jobs ADD COLUMN runner_id TEXT;
ALTER TABLE dream_jobs ADD COLUMN purpose TEXT NOT NULL DEFAULT 'extract'
  CHECK (purpose IN ('extract','redecision','consolidation'));

CREATE TABLE dream_runners (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  runner_id TEXT NOT NULL,
  host_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  capabilities_json TEXT NOT NULL DEFAULT '[]',
  heartbeat_at TEXT NOT NULL,
  lease_until TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, runner_id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
CREATE INDEX dream_runners_live
  ON dream_runners (tenant_id, user_id, lease_until);

-- A Held candidate may enter a new adjudication only through a persisted relation
-- to new evidence or an explicit user redecision request. Same candidate/evidence/
-- strategy is considered at most once.
CREATE TABLE dream_candidate_redecisions (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  candidate_id TEXT NOT NULL,
  dream_job_id TEXT NOT NULL,
  evidence_id TEXT NOT NULL,
  redecision_kind TEXT NOT NULL CHECK (redecision_kind IN ('related_evidence','user_request')),
  strategy_fingerprint TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, candidate_id, dream_job_id, evidence_id),
  UNIQUE (tenant_id, user_id, candidate_id, evidence_id, strategy_fingerprint),
  FOREIGN KEY (tenant_id, user_id, candidate_id)
    REFERENCES dream_candidates(tenant_id, user_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, user_id, dream_job_id)
    REFERENCES dream_jobs(tenant_id, user_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, user_id, evidence_id)
    REFERENCES evidence_events(tenant_id, user_id, id)
);

-- Model-produced mental models/topic pages are downstream of a durable Dream trigger.
-- Manual consolidation uses purpose='manual' and also owns a persisted Dream job.
CREATE TABLE dream_consolidation_links (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  dream_job_id TEXT NOT NULL,
  consolidation_job_id TEXT NOT NULL,
  purpose TEXT NOT NULL CHECK (purpose IN ('derived','manual')),
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, dream_job_id, consolidation_job_id),
  UNIQUE (tenant_id, user_id, consolidation_job_id),
  FOREIGN KEY (tenant_id, user_id, dream_job_id)
    REFERENCES dream_jobs(tenant_id, user_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, user_id, consolidation_job_id)
    REFERENCES consolidation_jobs(tenant_id, user_id, id) ON DELETE CASCADE
);
CREATE INDEX dream_consolidation_by_dream
  ON dream_consolidation_links (tenant_id, user_id, dream_job_id);

-- Existing standalone jobs predate the trigger contract and must never be claimed.
UPDATE consolidation_jobs
SET status='stale_input', error_code='DREAM_TRIGGER_REQUIRED',
    lease_until=NULL, claim_generation=claim_generation+1, updated_at=CURRENT_TIMESTAMP
WHERE status IN ('queued','running','retryable_failed');
