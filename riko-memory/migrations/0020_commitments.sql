CREATE TABLE commitments (
  id          TEXT PRIMARY KEY,
  memory_id   TEXT NOT NULL,
  kind        TEXT NOT NULL,
  due_at      TEXT,
  status      TEXT NOT NULL DEFAULT 'pending',
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  fulfilled_at TEXT,
  FOREIGN KEY (memory_id) REFERENCES memories(id) ON DELETE CASCADE
);
CREATE INDEX idx_commitments_memory_id ON commitments(memory_id);
CREATE INDEX idx_commitments_status_due ON commitments(status, due_at);
