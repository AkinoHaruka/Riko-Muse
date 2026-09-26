-- D6-1（doc6/02 §2）：soul/resident 存储与 metadata-only 审计。
-- 本迁移只新增表与索引，不改 0001—0004 的任何表/列；旧行不受影响。
-- 正文长度等 Unicode 标量规则在 Rust 校验（SQLite length 不按标量计）。

-- 记忆修改审计（doc6/14）：只存已有 L1/L2/L3 记录 update/delete 的元数据，
-- 不存正文、前后值、reason_code、actor_kind 或完整请求 JSON。
-- record_id 表示固定：L1=memory_id，L2=page_id，L3=本 scope 的 agent_id；
-- 读取/清理总是同时带 layer,tenant_id,user_id，agent_id 不作认证依据。
CREATE TABLE memory_audit (
  audit_id TEXT PRIMARY KEY,
  record_id TEXT NOT NULL,
  layer TEXT NOT NULL CHECK (layer IN ('L1','L2','L3')),
  action TEXT NOT NULL CHECK (action IN ('update','delete')),
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  agent_id TEXT,
  task_id TEXT,
  version INTEGER NOT NULL CHECK (version > 0),
  updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0),
  request_id TEXT,
  FOREIGN KEY (tenant_id,user_id) REFERENCES principals(tenant_id,user_id)
);
CREATE INDEX memory_audit_by_record
  ON memory_audit (record_id, updated_at_ms);
CREATE INDEX memory_audit_by_agent
  ON memory_audit (tenant_id, user_id, agent_id);
CREATE INDEX memory_audit_by_task
  ON memory_audit (tenant_id, user_id, task_id);
CREATE INDEX memory_audit_by_time
  ON memory_audit (updated_at_ms);

-- 用户编辑的 Soul（doc6/02 §2）：按 (tenant,user,agent) 隔离；agent_id 为宿主
-- 稳定 ID，禁止全局唯一键。空正文代表禁用；正文字符数由 Rust 校验。
CREATE TABLE soul_profiles (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  agent_id TEXT NOT NULL CHECK (length(agent_id) BETWEEN 1 AND 256),
  body_md TEXT NOT NULL,
  version INTEGER NOT NULL CHECK (version > 0),
  body_sha256 TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, agent_id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);

-- Soul 内容版本快照（doc6/02 §2）：独立内容版本能力，不替代 memory_audit；
-- 正文不复制进审计表。actor_kind 无 model。
CREATE TABLE soul_revisions (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  version INTEGER NOT NULL,
  body_md TEXT NOT NULL,
  body_sha256 TEXT NOT NULL,
  actor_kind TEXT NOT NULL CHECK (actor_kind IN ('user_cli','user_api','admin_cli')),
  changed_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, agent_id, version),
  FOREIGN KEY (tenant_id, user_id, agent_id)
    REFERENCES soul_profiles(tenant_id, user_id, agent_id)
);

-- Resident 固定记忆（doc6/02 §2）：pin/unpin 是选择配置，不改 L1 状态。
-- unpin 不删行（enabled=0 且 version+1），重新 pin 沿原行增 version；
-- pin 可保留在 forgotten/expired/retired 记录上作历史，但不得注入（读路径过滤）。
CREATE TABLE resident_pins (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  memory_id TEXT NOT NULL,
  enabled INTEGER NOT NULL CHECK (enabled IN (0,1)),
  position INTEGER NOT NULL CHECK (position >= 0),
  pinned_at TEXT NOT NULL,
  version INTEGER NOT NULL CHECK (version > 0),
  PRIMARY KEY (tenant_id, user_id, memory_id),
  FOREIGN KEY (tenant_id, user_id, memory_id)
    REFERENCES memories(tenant_id, user_id, id)
);
-- 同 scope 中 enabled=1 的 position 唯一（部分唯一索引）；disabled 行不占位。
CREATE UNIQUE INDEX resident_pins_active_position
  ON resident_pins (tenant_id, user_id, position) WHERE enabled = 1;

-- 幂等回执（doc6/02 §2）：同键同请求返回原结果，同键异请求 409
-- IDEMPOTENCY_CONFLICT；key 字符集与请求哈希规范化在 Rust 校验。
-- receipts 必须与目标修改同事务提交（调用方负责），响应中不得含令牌/正文。
CREATE TABLE mutation_receipts (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  operation TEXT NOT NULL,
  idempotency_key TEXT NOT NULL
    CHECK (length(idempotency_key) BETWEEN 1 AND 128
           AND idempotency_key NOT GLOB '*[^A-Za-z0-9._-]*'),
  request_sha256 TEXT NOT NULL,
  result_status TEXT NOT NULL,
  response_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, operation, idempotency_key),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
