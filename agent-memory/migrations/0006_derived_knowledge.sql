-- D6-5（doc6/02 §3）：派生知识文档、问题目录、整理作业与页面索引。
-- 本迁移只新增表与索引，不改 0001—0005；旧行不受影响。
-- 正文字符数上限等 Unicode 标量规则在 Rust 校验。

-- 统一保存问题画像与主题页（doc6/02 §3）：同一 scope/kind/key 最多一条 published
-- （部分唯一索引）。模型不能提供 ID、kind/key、question 定义、status/version 或 scope。
CREATE TABLE memory_pages (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  document_kind TEXT NOT NULL CHECK (document_kind IN ('mental_model','topic_page')),
  document_key TEXT NOT NULL,
  question_version INTEGER,
  question_text TEXT,
  title TEXT NOT NULL,
  body_md TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('published','stale','archived')),
  version INTEGER NOT NULL CHECK (version > 0),
  generator_version TEXT NOT NULL,
  input_fingerprint TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id),
  CHECK ((document_kind = 'mental_model' AND question_version IS NOT NULL AND question_text IS NOT NULL)
      OR (document_kind = 'topic_page' AND question_version IS NULL AND question_text IS NULL))
);
CREATE UNIQUE INDEX pages_published_unique
  ON memory_pages (tenant_id, user_id, document_kind, document_key) WHERE status = 'published';
CREATE INDEX pages_by_kind_key
  ON memory_pages (tenant_id, user_id, document_kind, document_key, status);

-- 画像允许使用的问题目录（doc6/02 §3）：Rust 管理的同 scope 版本化目录；
-- 首版每个 scope 默认为空，模型不能创建或修改。
CREATE TABLE mental_model_questions (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  question_key TEXT NOT NULL CHECK (length(question_key) BETWEEN 1 AND 64
    AND question_key NOT GLOB '*[^a-z0-9_]*'),
  question_text TEXT NOT NULL,
  version INTEGER NOT NULL CHECK (version > 0),
  status TEXT NOT NULL CHECK (status IN ('active','archived')),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, question_key),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);

-- 问题目录历史（用户审阅用）；不是 memory_audit 的 L1/L2/L3 对象。
CREATE TABLE mental_model_question_revisions (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  question_key TEXT NOT NULL,
  version INTEGER NOT NULL,
  question_text TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('active','archived')),
  actor_kind TEXT NOT NULL CHECK (actor_kind IN ('user_cli','user_api','admin_cli')),
  changed_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, question_key, version),
  FOREIGN KEY (tenant_id, user_id, question_key)
    REFERENCES mental_model_questions(tenant_id, user_id, question_key)
);

-- 页面来源（doc6/02 §3）：双边复合 FK；发布时 L1 版本与内容摘要固化，
-- 读取时再核源当前 active/版本/哈希/有效期。
CREATE TABLE page_sources (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  page_id TEXT NOT NULL,
  memory_id TEXT NOT NULL,
  memory_version INTEGER NOT NULL CHECK (memory_version > 0),
  claim_sha256 TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, page_id, memory_id),
  FOREIGN KEY (tenant_id, user_id, page_id)
    REFERENCES memory_pages(tenant_id, user_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, user_id, memory_id)
    REFERENCES memories(tenant_id, user_id, id)
);

-- 用户 pin 的派生文档（doc6/02 §3）：独立于 resident_pins；stale/archived 页
-- pin 历史保留但立即不可见。
CREATE TABLE resident_page_pins (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  page_id TEXT NOT NULL,
  enabled INTEGER NOT NULL CHECK (enabled IN (0,1)),
  position INTEGER NOT NULL CHECK (position >= 0),
  pinned_at TEXT NOT NULL,
  version INTEGER NOT NULL CHECK (version > 0),
  PRIMARY KEY (tenant_id, user_id, page_id),
  FOREIGN KEY (tenant_id, user_id, page_id)
    REFERENCES memory_pages(tenant_id, user_id, id)
);
CREATE UNIQUE INDEX resident_page_pins_active_position
  ON resident_page_pins (tenant_id, user_id, position) WHERE enabled = 1;

-- 页面内容版本快照（doc6/02 §3）：旧版只供查看，不能召回；恢复必须按当前
-- 有效来源重新生成。source 列表为 JSON（memory_id+version 数组），不复制正文来源约束。
CREATE TABLE page_revisions (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  page_id TEXT NOT NULL,
  version INTEGER NOT NULL,
  document_kind TEXT NOT NULL CHECK (document_kind IN ('mental_model','topic_page')),
  document_key TEXT NOT NULL,
  question_version INTEGER,
  question_text TEXT,
  previous_title TEXT,
  new_title TEXT NOT NULL,
  previous_body_md TEXT,
  new_body_md TEXT NOT NULL,
  source_ids_json TEXT NOT NULL,
  generator_version TEXT NOT NULL,
  actor_kind TEXT NOT NULL CHECK (actor_kind IN ('user_cli','user_api','admin_cli','system')),
  changed_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, page_id, version),
  FOREIGN KEY (tenant_id, user_id, page_id)
    REFERENCES memory_pages(tenant_id, user_id, id)
);

-- 整理作业（doc6/02 §3）：claim/lease/generation/过期恢复沿用 doc4 契约；
-- 不借用 extraction_jobs 的 (session,window) 唯一键。
-- 幂等用两个部分唯一索引（SQLite 多个 NULL 不相等，不能用含 NULL 的普通 UNIQUE）：
-- mental_model 索引含非空 question_version；topic_page 索引不含该空列。
CREATE TABLE consolidation_jobs (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  document_kind TEXT NOT NULL CHECK (document_kind IN ('mental_model','topic_page')),
  document_key TEXT NOT NULL,
  question_version INTEGER,
  input_fingerprint TEXT NOT NULL,
  generator_version TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN
    ('queued','running','succeeded','retryable_failed','dead','stale_input')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  run_after TEXT NOT NULL,
  lease_until TEXT,
  claim_generation INTEGER NOT NULL DEFAULT 0 CHECK (claim_generation >= 0),
  error_code TEXT,
  model_name TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
-- mental_model 幂等（question_version 非空时生效）。
CREATE UNIQUE INDEX consolidation_jobs_unique_mm
  ON consolidation_jobs (tenant_id, user_id, document_kind, document_key,
    question_version, input_fingerprint, generator_version)
  WHERE document_kind = 'mental_model' AND status NOT IN ('dead','stale_input');
-- topic_page 幂等（question_version 恒空）。
CREATE UNIQUE INDEX consolidation_jobs_unique_topic
  ON consolidation_jobs (tenant_id, user_id, document_kind, document_key,
    input_fingerprint, generator_version)
  WHERE document_kind = 'topic_page' AND status NOT IN ('dead','stale_input');
CREATE INDEX consolidation_jobs_claim
  ON consolidation_jobs (status, run_after, lease_until);

-- 作业输入固化（doc6/02 §3）：重试不得改输入；来源 scope/版本/哈希逐条固化。
CREATE TABLE consolidation_job_inputs (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  job_id TEXT NOT NULL,
  input_order INTEGER NOT NULL CHECK (input_order >= 0),
  memory_id TEXT NOT NULL,
  memory_version INTEGER NOT NULL CHECK (memory_version > 0),
  claim_sha256 TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, job_id, memory_id),
  FOREIGN KEY (tenant_id, user_id, job_id)
    REFERENCES consolidation_jobs(tenant_id, user_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, user_id, memory_id)
    REFERENCES memories(tenant_id, user_id, id)
);

-- 页面检索索引（doc6/02 §3）：只索引 published 且来源校验有效的文档；
-- 索引是派生数据，rebuild-index 同时重建。
CREATE VIRTUAL TABLE page_fts USING fts5(
  page_id UNINDEXED, title, body_md, tokenize = 'unicode61'
);

CREATE TABLE page_grams (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  page_id TEXT NOT NULL,
  gram TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, page_id, gram),
  FOREIGN KEY (tenant_id, user_id, page_id)
    REFERENCES memory_pages(tenant_id, user_id, id) ON DELETE CASCADE
);
CREATE INDEX page_grams_lookup
  ON page_grams (tenant_id, user_id, gram, page_id);
