-- doc7/06（V2-D1，分支 Riko-Muse）：蒸馏视图——精炼常驻 compact_memory 与四分面投影。
-- 0001—0015 冻结不动。派生条目逐条带来源；读路径即时屏蔽失效来源，不等下次重建。
-- 依据：doc7/06 §1；Muse-V2迭代开发文档/03（分面规则为本项目确定性选择，非 Muse 已知算法）。

-- 派生条目：compact_memory 与四个分面共用一张表，document_kind 区分。
CREATE TABLE derived_items (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL DEFAULT 'user_main',
  document_kind TEXT NOT NULL CHECK (document_kind IN
    ('compact_memory','facet_experience','facet_opinions','facet_reflections','facet_world')),
  position INTEGER NOT NULL CHECK (position >= 0),
  body TEXT NOT NULL,
  -- observed=可从来源逐字复核；inferred=派生解释（V2-D1 不产生，为后续生成器留位）
  observed_or_inferred TEXT NOT NULL CHECK (observed_or_inferred IN ('observed','inferred')),
  generator_version TEXT NOT NULL,
  source_fingerprint TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('active','stale','removed')),
  batch_version INTEGER NOT NULL CHECK (batch_version > 0),
  generated_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  -- derived_item_sources 的组合外键要求父表在 (tenant,user,id) 上有唯一约束。
  UNIQUE (tenant_id, user_id, id),
  UNIQUE (tenant_id, user_id, domain_id, document_kind, id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
CREATE INDEX derived_items_by_doc
  ON derived_items (tenant_id, user_id, domain_id, document_kind, status, position);

-- 逐条来源：至少一个（服务端校验）；删除原子记忆时据此定位受影响正文。
CREATE TABLE derived_item_sources (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  item_id TEXT NOT NULL,
  memory_id TEXT NOT NULL,
  memory_version INTEGER NOT NULL CHECK (memory_version > 0),
  claim_sha256 TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, item_id, memory_id),
  FOREIGN KEY (tenant_id, user_id, item_id)
    REFERENCES derived_items(tenant_id, user_id, id) ON DELETE CASCADE
);
CREATE INDEX derived_sources_by_memory
  ON derived_item_sources (tenant_id, user_id, memory_id);

-- 只读投影的导出 manifest；正文不入库（每次从同一数据库快照渲染）。
CREATE TABLE derived_exports (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL DEFAULT 'user_main',
  batch_version INTEGER NOT NULL CHECK (batch_version > 0),
  manifest_json TEXT NOT NULL,
  outcome TEXT NOT NULL CHECK (outcome IN ('complete','incomplete')),
  created_at TEXT NOT NULL,
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
CREATE INDEX derived_exports_by_scope
  ON derived_exports (tenant_id, user_id, domain_id, created_at DESC);
