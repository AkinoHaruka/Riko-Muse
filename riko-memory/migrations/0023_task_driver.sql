-- P2: persistent claims for the unattended serve driver. Historical ledger remains success-only.
CREATE TABLE task_claims (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL,
  task_kind TEXT NOT NULL CHECK(task_kind IN ('upkeep','relationships','nightly','quiet')),
  signal_ref TEXT NOT NULL,
  owner TEXT,
  status TEXT NOT NULL CHECK(status IN ('running','done','retry','exhausted')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
  lease_until INTEGER NOT NULL DEFAULT 0,
  next_retry_at INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL,
  PRIMARY KEY(tenant_id,user_id,domain_id,task_kind,signal_ref),
  FOREIGN KEY(tenant_id,user_id) REFERENCES principals(tenant_id,user_id)
);
CREATE INDEX task_claims_status ON task_claims(tenant_id,user_id,domain_id,updated_at DESC);
