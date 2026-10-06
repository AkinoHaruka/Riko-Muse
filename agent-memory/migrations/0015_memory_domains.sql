-- doc7/04（V2-S1，分支 Riko-Muse）：记忆域与 side-chat 隔离。
-- 身份 scope（tenant/user）保持不变；新增独立 MemoryDomain。0001—0014 冻结不动。
-- 旧数据全部归属 user_main（列缺省即回填）；principals 回填 user_main 域注册行。
-- 依据：doc7/04 §1；Muse-V2迭代开发文档/02（域矩阵与授权为本项目明确选择）。

-- 域注册表：user_main 隐式存在。
CREATE TABLE memory_domains (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('user_main','side')),
  status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','closed')),
  policy_version INTEGER NOT NULL DEFAULT 1,
  created_reason TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, domain_id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);

-- 会话→域绑定：可信宿主/用户经 API 登记；同会话唯一。
CREATE TABLE session_domain_bindings (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  host_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  domain_id TEXT NOT NULL,
  registered_by TEXT NOT NULL DEFAULT 'trusted_user',
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, host_id, session_id),
  FOREIGN KEY (tenant_id, user_id, domain_id)
    REFERENCES memory_domains(tenant_id, user_id, domain_id)
);

-- 跨域授权：reader 域额外可读 granted 域；撤销置 revoked_at。
CREATE TABLE cross_domain_grants (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  reader_domain TEXT NOT NULL,
  granted_domain TEXT NOT NULL,
  granted_by TEXT NOT NULL,
  reason TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  revoked_at TEXT,
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);

-- L0 证据域映射：evidence_events 保持来源身份；域归属记在本表。
-- 域功能未启用时不写行；无映射行的事件按 user_main 处理。
CREATE TABLE evidence_domain_map (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  evidence_id TEXT NOT NULL,
  domain_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, evidence_id),
  FOREIGN KEY (tenant_id, user_id, domain_id)
    REFERENCES memory_domains(tenant_id, user_id, domain_id)
);

-- 一等对象加域列（缓存/子表经 join 父对象继承，不加列，见 doc7/04 §1.2）。
ALTER TABLE memories ADD COLUMN domain_id TEXT NOT NULL DEFAULT 'user_main';
ALTER TABLE memory_candidates ADD COLUMN domain_id TEXT NOT NULL DEFAULT 'user_main';
ALTER TABLE extraction_jobs ADD COLUMN domain_id TEXT NOT NULL DEFAULT 'user_main';
ALTER TABLE memory_pages ADD COLUMN domain_id TEXT NOT NULL DEFAULT 'user_main';
ALTER TABLE consolidation_jobs ADD COLUMN domain_id TEXT NOT NULL DEFAULT 'user_main';
ALTER TABLE semantic_jobs ADD COLUMN domain_id TEXT NOT NULL DEFAULT 'user_main';
ALTER TABLE repair_threads ADD COLUMN domain_id TEXT NOT NULL DEFAULT 'user_main';
ALTER TABLE dream_jobs ADD COLUMN domain_id TEXT NOT NULL DEFAULT 'user_main';
ALTER TABLE suppressed_sources ADD COLUMN domain_id TEXT NOT NULL DEFAULT 'user_main';
ALTER TABLE rupture_events ADD COLUMN domain_id TEXT NOT NULL DEFAULT 'user_main';

-- 主键/唯一索引重建（SQLite 不能原地改 PK；小表重建，旧行复制为 user_main）。
CREATE TABLE alignment_synthesis_domains (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL DEFAULT 'user_main',
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
  PRIMARY KEY (tenant_id, user_id, domain_id, version),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
INSERT INTO alignment_synthesis_domains
  (tenant_id, user_id, domain_id, version, window_since, window_until,
   rupture_turns, user_turns, correction_free_rate, open_repair_threads,
   body, source_refs_json, generated_at)
SELECT tenant_id, user_id, 'user_main', version, window_since, window_until,
   rupture_turns, user_turns, correction_free_rate, open_repair_threads,
   body, source_refs_json, generated_at
FROM alignment_synthesis;
DROP TABLE alignment_synthesis;
ALTER TABLE alignment_synthesis_domains RENAME TO alignment_synthesis;

CREATE TABLE purge_tombstones_domains (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL DEFAULT 'user_main',
  source_kind TEXT NOT NULL CHECK (source_kind IN ('evidence','memory')),
  source_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, domain_id, source_kind, source_id)
);
INSERT INTO purge_tombstones_domains
  (tenant_id, user_id, domain_id, source_kind, source_id, created_at)
SELECT tenant_id, user_id, 'user_main', source_kind, source_id, created_at
FROM purge_tombstones;
DROP TABLE purge_tombstones;
ALTER TABLE purge_tombstones_domains RENAME TO purge_tombstones;

-- 同一 topic/画像 key 允许各域各自维护 published 页面。
DROP INDEX pages_published_unique;
CREATE UNIQUE INDEX pages_published_unique
  ON memory_pages (tenant_id, user_id, domain_id, document_kind, document_key)
  WHERE status = 'published';

-- 旧 principal 回填 user_main 域注册行（幂等）。
INSERT INTO memory_domains (tenant_id, user_id, domain_id, kind, status, policy_version,
                            created_reason, created_at, updated_at)
SELECT p.tenant_id, p.user_id, 'user_main', 'user_main', 'active', 1,
       'migration_backfill',
       strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM principals p
WHERE NOT EXISTS (
  SELECT 1 FROM memory_domains d
  WHERE d.tenant_id = p.tenant_id AND d.user_id = p.user_id AND d.domain_id = 'user_main'
);

CREATE INDEX memories_by_domain ON memories (tenant_id, user_id, domain_id, status);
CREATE INDEX candidates_by_domain ON memory_candidates (tenant_id, user_id, domain_id, status);
