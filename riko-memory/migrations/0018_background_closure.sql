-- doc7/08（V2-B1/A1，分支 Riko-Muse）：后台闭环、处理账本与修复行动。
-- 0001—0017 冻结不动。普通 turn/end/flush 只登记信号，调度判定是纯函数，无新信号零模型调用。
-- 依据：doc7/08 §1—§4；Muse-V2迭代开发文档/08（20 分钟 / 3 次为本项目初始参数）。

-- rupture 事件补两列：目标分类与检测器版本。历史行是 v1 语义（都会开线程），
-- 默认值保持历史统计不被改写。
ALTER TABLE rupture_events ADD COLUMN target TEXT NOT NULL DEFAULT 'agent_correction'
  CHECK (target IN ('agent_correction','self_correction','third_party','neutral'));
ALTER TABLE rupture_events ADD COLUMN detector_version TEXT NOT NULL DEFAULT 'rupture_v1';

-- 处理账本：同一 (域, 任务能力, 信号引用) 只消化一次（doc 08 §1）。
CREATE TABLE processing_ledger (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL DEFAULT 'user_main',
  task_kind TEXT NOT NULL CHECK (task_kind IN
    ('upkeep','relationships','nightly','quiet','rupture_scan')),
  signal_ref TEXT NOT NULL,
  generation INTEGER NOT NULL DEFAULT 1 CHECK (generation > 0),
  processed_at TEXT NOT NULL,
  PRIMARY KEY (tenant_id, user_id, domain_id, task_kind, signal_ref),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);

-- 任务运行回执：每日 quiet 次数上限等按此计数；失败也留痕，不吞失败当成功。
CREATE TABLE task_runs (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  domain_id TEXT NOT NULL DEFAULT 'user_main',
  task_kind TEXT NOT NULL,
  outcome TEXT NOT NULL CHECK (outcome IN ('ok','empty','error','skipped')),
  signal_count INTEGER NOT NULL DEFAULT 0 CHECK (signal_count >= 0),
  detail TEXT NOT NULL DEFAULT '',
  run_at TEXT NOT NULL,
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id)
);
CREATE INDEX task_runs_by_day ON task_runs (tenant_id, user_id, task_kind, run_at DESC);

-- 修复行动（A1）：具体行为，不是「避免同类问题」。
CREATE TABLE repair_actions (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  thread_id TEXT NOT NULL,
  action TEXT NOT NULL,
  expected_behavior TEXT NOT NULL,
  conditions TEXT NOT NULL DEFAULT '',
  counterexamples TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL CHECK (status IN ('proposed','active','done','withdrawn')),
  -- 模型只能 propose；激活/关闭必须有人或可信规则的授权记录。
  proposed_by TEXT NOT NULL CHECK (proposed_by IN ('model','user','cli')),
  authorized_by TEXT,
  authorized_at TEXT,
  close_reason TEXT,
  recurrence_count INTEGER NOT NULL DEFAULT 0 CHECK (recurrence_count >= 0),
  source_memory_id TEXT,
  generator_version TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (tenant_id, user_id, id),
  FOREIGN KEY (tenant_id, user_id) REFERENCES principals(tenant_id, user_id),
  FOREIGN KEY (tenant_id, user_id, thread_id)
    REFERENCES repair_threads(tenant_id, user_id, id)
);

-- 执行与复发观察：同一 action 关联多条 rupture 才算复发。
CREATE TABLE repair_action_events (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  action_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('recurrence','resolved','note')),
  rupture_id TEXT,
  detail TEXT NOT NULL DEFAULT '',
  observed_at TEXT NOT NULL,
  -- 复发按 rupture 幂等：同一 rupture 重复记录只留一条。
  -- 不能把 observed_at 放进唯一键——同一时刻的两条不同 rupture 会互相冲突。
  -- NULL rupture_id（note 等）在 SQLite 里互不冲突，可以多条。
  UNIQUE (tenant_id, user_id, action_id, kind, rupture_id),
  FOREIGN KEY (tenant_id, user_id, action_id)
    REFERENCES repair_actions(tenant_id, user_id, id) ON DELETE CASCADE
);
