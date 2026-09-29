-- D6-12: version snapshots for job-scoped Dream reads. Existing migrations stay immutable.
-- Cascading target FKs ensure purge of a memory/page also removes every Dream read reference.

ALTER TABLE dream_jobs ADD COLUMN read_calls INTEGER NOT NULL DEFAULT 0 CHECK (read_calls >= 0);
ALTER TABLE dream_jobs ADD COLUMN read_budget INTEGER NOT NULL DEFAULT 32 CHECK (read_budget BETWEEN 1 AND 64);

-- Metadata-only proof that the child completed the required recall step. Query
-- text is never stored; targets themselves are snapshotted in the tables above.
CREATE TABLE dream_read_search_receipts (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  dream_job_id TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK (generation > 0),
  operation TEXT NOT NULL CHECK (operation IN ('memory','page')),
  candidate_id TEXT NOT NULL DEFAULT '',
  query_sha256 TEXT NOT NULL,
  semantic_complete INTEGER NOT NULL CHECK (semantic_complete IN (0,1)),
  document_key_match INTEGER NOT NULL DEFAULT 0 CHECK (document_key_match IN (0,1)),
  result_count INTEGER NOT NULL CHECK (result_count >= 0),
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id,user_id,dream_job_id,generation,operation,candidate_id,query_sha256),
  FOREIGN KEY (tenant_id,user_id,dream_job_id)
    REFERENCES dream_jobs(tenant_id,user_id,id) ON DELETE CASCADE
);

CREATE TABLE dream_read_memory_targets (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  dream_job_id TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK (generation > 0),
  memory_id TEXT NOT NULL,
  memory_version INTEGER NOT NULL CHECK (memory_version > 0),
  claim_sha256 TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, dream_job_id, generation, memory_id),
  FOREIGN KEY (tenant_id, user_id, dream_job_id)
    REFERENCES dream_jobs(tenant_id, user_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, user_id, memory_id)
    REFERENCES memories(tenant_id, user_id, id) ON DELETE CASCADE
);
CREATE INDEX dream_read_memory_targets_by_memory
  ON dream_read_memory_targets (tenant_id, user_id, memory_id);

CREATE TABLE dream_read_page_targets (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  dream_job_id TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK (generation > 0),
  page_id TEXT NOT NULL,
  page_version INTEGER NOT NULL CHECK (page_version > 0),
  page_fingerprint TEXT NOT NULL,
  detail_read INTEGER NOT NULL DEFAULT 0 CHECK (detail_read IN (0,1)),
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, dream_job_id, generation, page_id),
  FOREIGN KEY (tenant_id, user_id, dream_job_id)
    REFERENCES dream_jobs(tenant_id, user_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, user_id, page_id)
    REFERENCES memory_pages(tenant_id, user_id, id) ON DELETE CASCADE
);
CREATE INDEX dream_read_page_targets_by_page
  ON dream_read_page_targets (tenant_id, user_id, page_id);
