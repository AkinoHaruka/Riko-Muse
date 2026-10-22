-- doc7/13 P1: versioned dated reflections and model repair-action citation records.
-- Historical migrations and template synthesis rows remain unchanged.
CREATE TABLE processing_ledger_p1 (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL DEFAULT 'user_main',
  task_kind TEXT NOT NULL CHECK (task_kind IN
    ('upkeep','relationships','nightly','quiet','rupture_scan','alignment','repair_action')),
  signal_ref TEXT NOT NULL,
  generation INTEGER NOT NULL DEFAULT 1 CHECK (generation > 0),
  processed_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id,user_id,domain_id,task_kind,signal_ref),
  FOREIGN KEY(tenant_id,user_id) REFERENCES principals(tenant_id,user_id)
);
INSERT INTO processing_ledger_p1 SELECT * FROM processing_ledger;
DROP TABLE processing_ledger;
ALTER TABLE processing_ledger_p1 RENAME TO processing_ledger;

CREATE TABLE generation_artifacts (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('reflection_v1','repair_action_v1')),
  signal_ref TEXT NOT NULL,
  body TEXT NOT NULL,
  sentence_refs_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  thread_id TEXT,
  UNIQUE(tenant_id,user_id,domain_id,kind,signal_ref),
  FOREIGN KEY(tenant_id,user_id) REFERENCES principals(tenant_id,user_id)
);
CREATE INDEX generation_artifacts_scope ON generation_artifacts(tenant_id,user_id,domain_id,kind,created_at DESC);
ALTER TABLE repair_actions ADD COLUMN generation_artifact_id TEXT;
