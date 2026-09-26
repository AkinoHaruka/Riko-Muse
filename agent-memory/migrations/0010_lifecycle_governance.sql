-- D6-9（doc6/02 §7）：可逆退休、purge 两阶段账本、retention 策略。
-- 本迁移只新增表与索引，不改 0001—0009；特别是不能扩展 memories.status CHECK，
-- 也不能改变 v1 /forget 的保留与 suppressed_sources 防重放语义。

-- 可逆退休覆盖（doc6/12）：被覆盖记忆不能进入任一 read/query path；
-- restore 通过同事务移除当前覆盖。不新建通用 lifecycle audit ledger。
CREATE TABLE memory_retirements (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  memory_id TEXT NOT NULL,
  memory_version INTEGER NOT NULL CHECK (memory_version > 0),
  actor_kind TEXT NOT NULL CHECK (actor_kind IN ('user','admin_cli')),
  reason_code TEXT,
  user_evidence_id TEXT,
  retired_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, memory_id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id),
  FOREIGN KEY (tenant_id, user_id, memory_id) REFERENCES memories(tenant_id, user_id, id)
);

-- purge 两阶段确认（doc6/02 §7）：preview 只读业务记忆但写确认元数据；
-- 只存随机 token 的哈希；明文只返回给可信调用者一次。
CREATE TABLE purge_confirmations (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  token_sha256 TEXT NOT NULL,
  operation TEXT NOT NULL CHECK (operation = 'purge_memory'),
  target_id TEXT,
  target_version INTEGER,
  dependency_fingerprint TEXT,
  idempotency_key TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  consumed INTEGER NOT NULL CHECK (consumed IN (0,1)),
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, token_sha256),
  UNIQUE (tenant_id, user_id, idempotency_key)
);

-- purge 执行账本：终态清除可反查目标的 ID/fingerprint，只保留无正文状态与计数
-- （doc6/02 §7：target_id/fingerprint 完成后置 NULL）。
CREATE TABLE purge_jobs (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  operation TEXT NOT NULL CHECK (operation = 'purge_memory'),
  target_id TEXT,
  dependency_fingerprint TEXT,
  status TEXT NOT NULL CHECK (status IN ('pending','running','succeeded','failed')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  deleted_counts_json TEXT NOT NULL DEFAULT '{}',
  error_code TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);

-- 无正文防重放墓碑（doc6/02 §7）：按 scope+source ID 唯一；不设指向已删除
-- 对象的 FK，不存 quote/claim/embedding/原始 JSON。与 v1 suppressed_sources
-- 不互相替代：后者保留原语义。
CREATE TABLE purge_tombstones (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  source_kind TEXT NOT NULL CHECK (source_kind IN ('evidence','memory')),
  source_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, source_kind, source_id)
);

-- retention 策略（doc6/12 §5）：默认 0（关闭）；可信用户/管理员显式设正值后
-- 清理器无需逐批二次确认。Agent/Dream 无权写入。历史版本只留无正文策略值。
CREATE TABLE retention_policies (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  policy_version INTEGER NOT NULL CHECK (policy_version > 0),
  effective_at TEXT NOT NULL,
  raw_evidence_retention_days INTEGER NOT NULL CHECK (raw_evidence_retention_days >= 0),
  expired_memory_purge_after_days INTEGER NOT NULL CHECK (expired_memory_purge_after_days >= 0),
  enabled INTEGER NOT NULL CHECK (enabled IN (0,1)),
  updated_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
CREATE TABLE retention_policy_history (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  policy_version INTEGER NOT NULL CHECK (policy_version > 0),
  raw_evidence_retention_days INTEGER NOT NULL CHECK (raw_evidence_retention_days >= 0),
  expired_memory_purge_after_days INTEGER NOT NULL CHECK (expired_memory_purge_after_days >= 0),
  enabled INTEGER NOT NULL CHECK (enabled IN (0,1)),
  effective_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, policy_version)
);

-- retention 作业（doc6/02 §7）：按 scope+policy version+批次 fingerprint 幂等；
-- 共享 purge 的闭包/墓碑流程；提交删除前再次读取当前 policy version。
CREATE TABLE retention_jobs (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  policy_version INTEGER NOT NULL CHECK (policy_version > 0),
  batch_fingerprint TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('queued','running','succeeded','failed')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  deleted_counts_json TEXT NOT NULL DEFAULT '{}',
  error_code TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, policy_version, batch_fingerprint),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
