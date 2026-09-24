-- v2（doc2/05 §3）：每个提取作业持久记录生成时的规则版本。
-- 不修改已发布的 0001；旧作业由 DEFAULT 回填 'extract_v1'。
-- prompt_version 记录"生成该作业时的规则版本"，不能由模型输出填写；
-- worker 必须按作业行版本选择准入规则，未知版本显式失败。
ALTER TABLE extraction_jobs ADD COLUMN prompt_version TEXT NOT NULL DEFAULT 'extract_v1';
