-- doc7/07（V2-R1，分支 Riko-Muse）：关系图谱、实体索引与延迟读取。
-- 0001—0016 冻结不动。实体/别名/条目/来源全部由可见记忆确定性投影而来，逐条带来源。
-- 政策边界：不新增放宽的第三人准入路径；THIRD_PARTY 门保持生效（doc7/07 §0）。
-- 依据：doc7/07 §1；Muse-V2迭代开发文档/01（排序与归并为项目选择，非 Muse 已知算法）。

CREATE TABLE relationship_entities (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL DEFAULT 'user_main',
  entity_kind TEXT NOT NULL CHECK (entity_kind IN ('person','group')),
  slug TEXT NOT NULL,
  display_name TEXT NOT NULL,
  relation TEXT,
  summary TEXT,
  closeness_rank INTEGER,
  rank_source TEXT NOT NULL DEFAULT 'unranked'
    CHECK (rank_source IN ('user_explicit','verified_role','unranked')),
  version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
  status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','stale','removed')),
  generator_version TEXT NOT NULL,
  batch_version INTEGER NOT NULL CHECK (batch_version > 0),
  generated_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  -- relationship_items / entity_aliases 的组合外键要求父表在 (tenant,user,id) 上有唯一约束。
  UNIQUE (tenant_id, user_id, id),
  UNIQUE (tenant_id, user_id, domain_id, entity_kind, slug),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
CREATE INDEX relationship_entities_by_scope
  ON relationship_entities (tenant_id, user_id, domain_id, status, entity_kind);

-- 别名也要有出处；同一别名允许多个实体，resolve 必须返回歧义而不是猜。
CREATE TABLE entity_aliases (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  alias TEXT NOT NULL,
  alias_kind TEXT NOT NULL CHECK (alias_kind IN ('name','nickname','role')),
  memory_id TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, entity_id, alias, alias_kind),
  FOREIGN KEY (tenant_id, user_id, entity_id)
    REFERENCES relationship_entities(tenant_id, user_id, id) ON DELETE CASCADE
);
CREATE INDEX entity_aliases_by_alias ON entity_aliases (tenant_id, user_id, alias);

CREATE TABLE relationship_items (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  section TEXT NOT NULL CHECK (section IN
    ('facts','history','relationship','in_common','open_threads','strengthening')),
  body TEXT NOT NULL,
  observed_or_inferred TEXT NOT NULL CHECK (observed_or_inferred IN ('observed','inferred')),
  occurred_at TEXT,
  valid_until TEXT,
  version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
  status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','stale','removed')),
  generator_version TEXT NOT NULL,
  source_fingerprint TEXT NOT NULL,
  batch_version INTEGER NOT NULL CHECK (batch_version > 0),
  generated_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, id),
  FOREIGN KEY (tenant_id, user_id, entity_id)
    REFERENCES relationship_entities(tenant_id, user_id, id) ON DELETE CASCADE
);
CREATE INDEX relationship_items_by_entity
  ON relationship_items (tenant_id, user_id, entity_id, section, status);

CREATE TABLE relationship_sources (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  item_id TEXT NOT NULL,
  memory_id TEXT NOT NULL,
  memory_version INTEGER NOT NULL CHECK (memory_version > 0),
  claim_sha256 TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, item_id, memory_id),
  FOREIGN KEY (tenant_id, user_id, item_id)
    REFERENCES relationship_items(tenant_id, user_id, id) ON DELETE CASCADE
);
CREATE INDEX relationship_sources_by_memory
  ON relationship_sources (tenant_id, user_id, memory_id);

-- 群组成员关系：成员也必须来自证据。V2-R1 不启用（表先建好，避免以后再改结构）。
CREATE TABLE group_memberships (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  group_id TEXT NOT NULL,
  person_id TEXT NOT NULL,
  memory_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, group_id, person_id),
  FOREIGN KEY (tenant_id, user_id, group_id)
    REFERENCES relationship_entities(tenant_id, user_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, user_id, person_id)
    REFERENCES relationship_entities(tenant_id, user_id, id) ON DELETE CASCADE
);
