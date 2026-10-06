-- doc7（Riko-Muse，分支 Riko-Muse）：Muse 增量能力。
-- M2/M3：rupture（纠正/裂痕）事件、repair（修复）线程、alignment synthesis（相处指南）。
-- 只新增表与索引，不改 0001—0013；M1（valid_until 到期转 expired）复用 memories.status
-- 既有 'expired' 枚举，本迁移不涉及 memories 列变更。
-- 依据：doc7/01 §1；Muse文档 12 §五（➕1/➕2/➕3）。

-- 纠正/裂痕事件（doc7/01 §1.1）：确定性 Rust 规则对 user 事件正文的字节 span 匹配。
-- 幂等键防止重扫/重放重复；来源可反查（evidence_id + 字节偏移）。
CREATE TABLE rupture_events (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  evidence_id TEXT NOT NULL,
  host_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  event_seq INTEGER NOT NULL CHECK (event_seq >= 0),
  signal TEXT NOT NULL,
  cue TEXT NOT NULL,
  start_byte INTEGER NOT NULL CHECK (start_byte >= 0),
  end_byte INTEGER NOT NULL CHECK (end_byte > start_byte),
  thread_id TEXT,
  detected_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, evidence_id, signal, start_byte),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id),
  FOREIGN KEY (tenant_id, user_id, evidence_id)
    REFERENCES evidence_events(tenant_id, user_id, id)
);
CREATE INDEX rupture_events_by_scope
  ON rupture_events (tenant_id, user_id, detected_at);

-- rupture 扫描游标（doc7/01 §1.2）：按 evidence_events.rowid 单调推进。
CREATE TABLE rupture_scan_cursors (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  last_rowid INTEGER NOT NULL CHECK (last_rowid >= 0),
  updated_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);

-- 修复线程（doc7/01 §1.3）：7 天窗口归组；关线只经显式 API/CLI。
CREATE TABLE repair_threads (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  title TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('open','closed')),
  rupture_count INTEGER NOT NULL DEFAULT 1 CHECK (rupture_count > 0),
  first_rupture_at TEXT NOT NULL,
  last_rupture_at TEXT NOT NULL,
  closed_at TEXT,
  close_reason TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
CREATE INDEX repair_threads_by_scope
  ON repair_threads (tenant_id, user_id, status, last_rupture_at DESC);

-- 相处指南（doc7/01 §1.4）：确定性派生、版本递增、来源可溯；非 LLM 产物。
CREATE TABLE alignment_synthesis (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  version INTEGER NOT NULL CHECK (version > 0),
  window_since TEXT NOT NULL,
  window_until TEXT NOT NULL,
  rupture_turns INTEGER NOT NULL CHECK (rupture_turns >= 0),
  user_turns INTEGER NOT NULL CHECK (user_turns >= 0),
  correction_free_rate REAL NOT NULL
    CHECK (correction_free_rate >= 0 AND correction_free_rate <= 1),
  open_repair_threads INTEGER NOT NULL CHECK (open_repair_threads >= 0),
  body TEXT NOT NULL,
  source_refs_json TEXT NOT NULL,
  generated_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, version),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
