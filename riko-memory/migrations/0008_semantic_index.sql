-- D6-8（doc6/02 §4）：语义向量索引与索引作业队列。
-- 本迁移只新增表与索引，不改 0001—0007；旧行不受影响。
-- 向量为有限非 NaN 的 f32 小端数组（Rust 校验），BLOB 长度恒 dimensions*4。
-- 只比较相同 model_id + dimensions 的向量；模型切换不能混算。

-- 对象级版本化向量缓存（doc6/02 §4）：kind 仅 memory|page；source_version 为
-- memories.version / memory_pages.version；content_sha256 用于来源变化 stale 判定。
-- 跨表 object_id 无法由单一 FK 表达，Rust 写入/查询须核对象同 scope 与有效状态。
CREATE TABLE semantic_vectors (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  object_kind TEXT NOT NULL CHECK (object_kind IN ('memory','page')),
  object_id TEXT NOT NULL,
  model_id TEXT NOT NULL,
  source_version INTEGER NOT NULL CHECK (source_version > 0),
  content_sha256 TEXT NOT NULL,
  dimensions INTEGER NOT NULL CHECK (dimensions > 0),
  vector_blob BLOB NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('ready','stale')),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, object_kind, object_id, model_id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
CREATE INDEX semantic_vectors_scan
  ON semantic_vectors (tenant_id, user_id, object_kind, model_id, status);

-- 异步索引队列（doc6/02 §4）：correct/retire/forget/purge/页 stale 同事务置
-- 向量 stale；新建/更新 L1 或发布页面后入队重算。网络调用不持 DB 锁。
CREATE TABLE semantic_jobs (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  object_kind TEXT NOT NULL CHECK (object_kind IN ('memory','page')),
  object_id TEXT NOT NULL,
  source_version INTEGER NOT NULL CHECK (source_version > 0),
  content_sha256 TEXT NOT NULL,
  model_id TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN
    ('queued','running','succeeded','retryable_failed','provider_wait','dead','stale_input')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  run_after TEXT NOT NULL,
  lease_until TEXT,
  claim_generation INTEGER NOT NULL DEFAULT 0 CHECK (claim_generation >= 0),
  error_code TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
CREATE INDEX semantic_jobs_claim
  ON semantic_jobs (status, run_after);
-- 同对象同模型只允许一个待处理索引作业（幂等 coalesce，doc4 作业原则）。
CREATE UNIQUE INDEX semantic_jobs_pending_unique
  ON semantic_jobs (tenant_id, user_id, object_kind, object_id, model_id)
  WHERE status IN ('queued','running','retryable_failed','provider_wait');
