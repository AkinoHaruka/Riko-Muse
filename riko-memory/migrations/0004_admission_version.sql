-- 0004（doc5/03 §1）：准入规则版本与提取提示词版本分离。
-- 不修改已发布的 0001—0003。
-- prompt_version（0002）只记录生成该作业时的模型提示词版本（extract_v1/v2/v3）；
-- admission_version 记录提交候选时使用的 Rust 准入与查库规则版本（admit_v1/v2）。
-- 两列都在作业创建时由服务端写入，不由模型输出填写；worker 领取后按两列分别
-- 分派提示词与准入规则，任何未知版本显式失败（UNKNOWN_PROMPT_VERSION /
-- UNKNOWN_ADMISSION_VERSION，确定性错误立即 dead），不得退回"最新规则"。
-- 历史行由 DEFAULT 回填 'admit_v1'；原 ID、attempts、status、prompt_version、
-- 候选与记忆均不变。本迁移只加元数据列，不重跑作业、不回填 active、不改写旧候选。
ALTER TABLE extraction_jobs
  ADD COLUMN admission_version TEXT NOT NULL DEFAULT 'admit_v1';
