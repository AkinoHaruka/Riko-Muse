CREATE TABLE recall_log (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  ts            TEXT NOT NULL,
  query         TEXT NOT NULL,
  hit_ids       TEXT NOT NULL,
  scores        TEXT,
  rank          TEXT,
  cutoff_reason TEXT,
  excluded      TEXT
);
CREATE INDEX idx_recall_log_ts ON recall_log(ts);
CREATE INDEX idx_recall_log_query ON recall_log(query);
