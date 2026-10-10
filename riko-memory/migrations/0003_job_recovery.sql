-- 0003（doc4/02 §1）：作业恢复、公平调度与人工跳过所需的持久结构。
-- 不修改已发布的 0001/0002。
-- claim_generation：每次领取与过期恢复原子递增；旧执行者（generation 不匹配）的
--   续租、候选写入、成功/失败提交均不得生效（doc4/02 §4—5）。
-- jobs_through_unique：窗口前驱与下界改按 through_event_seq 数值判定（doc4/02 §3）；
--   建索引前若既有数据存在 (tenant,user,host,session,through) 重复，迁移执行器在
--   同一事务内先检查并报出冲突 job ID 后中止（不删行、不做部分 DDL）。
-- extraction_job_skips：仅记录对 dead/WINDOW_TOO_LARGE 作业的显式本地管理员跳过
--   （doc4/03 §4）；不让模型或 DSH 工具写入；不删除 L0 原始事件。
ALTER TABLE extraction_jobs
  ADD COLUMN claim_generation INTEGER NOT NULL DEFAULT 0 CHECK (claim_generation >= 0);
CREATE UNIQUE INDEX jobs_through_unique
  ON extraction_jobs (tenant_id,user_id,host_id,session_id,through_event_seq);
CREATE TABLE extraction_job_skips (
  job_id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  reason_code TEXT NOT NULL,
  actor_kind TEXT NOT NULL,
  actor_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  FOREIGN KEY (tenant_id,user_id,job_id)
    REFERENCES extraction_jobs(tenant_id,user_id,id)
);
