//! 整理作业存储（doc6/02 §3、doc6/05 §2）：claim/lease/generation/过期恢复沿用
//! doc4 作业原则；输入固化（重试不得改输入）；同 key 幂等靠 0006 的两个部分
//! 唯一索引（mental_model 含非空 question_version；topic_page 不含空列）。
//! 模型调用不在此发生（D6-7 Dream job 编排后才接 LLM）；本模块只管持久化与
//! 状态机，固定响应验证在单元测试完成。

use memory_domain::ScopeKey;
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

/// 作业行（doc6/02 §3 列集）。
#[derive(Debug, Clone)]
pub struct ConsolidationJobRow {
    pub id: String,
    pub document_kind: String,
    pub document_key: String,
    pub question_version: Option<i64>,
    pub input_fingerprint: String,
    pub generator_version: String,
    pub status: String,
    pub attempts: i64,
    pub run_after: String,
    pub lease_until: Option<String>,
    pub claim_generation: i64,
    pub error_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 固化输入项：(memory_id, memory_version, claim_sha256)。
pub type JobInput = (String, i64, String);

impl Store {
    /// 入队（doc6/05 §2）：同 key 同 fingerprint 同 generator 的未终态 job 幂等
    /// 返回既有 job；否则固化输入并排队。输入必须全部当前 active/同版/未过期
    /// （由调用方从规范表选取并传入；此处再核一次，防漂移）。
    pub fn consolidation_enqueue(
        &mut self,
        scope: &ScopeKey,
        document_kind: &str,
        document_key: &str,
        question_version: Option<i64>,
        generator_version: &str,
        input_fingerprint: &str,
        inputs: &[JobInput],
        run_after: &str,
    ) -> Result<ConsolidationJobRow, StoreError> {
        if inputs.is_empty() {
            return Err(StoreError::InvalidPageField);
        }
        let now = now_rfc3339()?;
        // 来源再核（防调用方传漂移输入）：任一不符 → 作业不入队（doc6/05 §2）。
        for (mid, ver, sha) in inputs {
            let row: Option<(i64, String)> = self
                .conn()
                .query_row(
                    "SELECT version, claim_sha256 FROM memories
                     WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='active'
                       AND (valid_until IS NULL OR valid_until > ?4)",
                    params![scope.tenant_id, scope.user_id, mid, now],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match row {
                Some((v, s)) if v == *ver && s == *sha => {}
                _ => return Err(StoreError::StaleInput),
            }
        }
        let tx = self.conn_mut().transaction()?;
        // 幂等：查既有未终态 job（同 scope/kind/key/fingerprint/generator）。
        let existing: Option<ConsolidationJobRow> = tx
            .query_row(
                &format!(
                    "SELECT id, document_kind, document_key, question_version, input_fingerprint,
                            generator_version, status, attempts, run_after, lease_until,
                            claim_generation, error_code, created_at, updated_at
                     FROM consolidation_jobs
                     WHERE tenant_id=?1 AND user_id=?2 AND document_kind=?3 AND document_key=?4
                       AND input_fingerprint=?5 AND generator_version=?6
                       AND status NOT IN ('dead','stale_input')
                       AND (?7 IS NULL OR question_version=?7) LIMIT 1"
                ),
                params![scope.tenant_id, scope.user_id, document_kind, document_key,
                        input_fingerprint, generator_version, question_version],
                map_job_row,
            )
            .optional()?;
        if let Some(job) = existing {
            tx.commit()?;
            return Ok(job);
        }
        let id = Uuid::now_v7().to_string();
        tx.execute(
            "INSERT INTO consolidation_jobs
               (id, tenant_id, user_id, document_kind, document_key, question_version,
                input_fingerprint, generator_version, status, attempts, run_after,
                claim_generation, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued', 0, ?9, 0, ?10, ?10)",
            params![id, scope.tenant_id, scope.user_id, document_kind, document_key,
                    question_version, input_fingerprint, generator_version, run_after, now],
        )?;
        for (order, (mid, ver, sha)) in inputs.iter().enumerate() {
            tx.execute(
                "INSERT INTO consolidation_job_inputs
                   (tenant_id, user_id, job_id, input_order, memory_id, memory_version, claim_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![scope.tenant_id, scope.user_id, id, order as i64, mid, ver, sha],
            )?;
        }
        tx.commit()?;
        let job = self
            .consolidation_get(scope, &id)?
            .ok_or(StoreError::JobNotFound)?;
        Ok(job)
    }

    /// 领取（doc4 契约）：queued 且 run_after<=now → running + lease + generation+1。
    /// 每次一个（同 scope 至多一个 running 的并发控制由调用方/调度器保证；doc6/05 §2）。
    pub fn consolidation_claim(
        &mut self,
        scope: &ScopeKey,
        now: &str,
        lease_secs: u64,
    ) -> Result<Option<ConsolidationJobRow>, StoreError> {
        let (id, generation): (String, i64) = {
            let mut stmt = self.conn().prepare(
                "SELECT id, claim_generation FROM consolidation_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND status='queued' AND run_after<=?3
                 ORDER BY run_after, created_at LIMIT 1",
            )?;
            let rows = stmt.query_row(params![scope.tenant_id, scope.user_id, now], |r| {
                Ok((r.get(0)?, r.get(1)?))
            });
            match rows {
                Ok(v) => v,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        };
        let lease_until = lease_until_from(now, lease_secs)?;
        self.conn_mut().execute(
            "UPDATE consolidation_jobs SET status='running', lease_until=?4, claim_generation=?5,
                    attempts=attempts+1, updated_at=?6
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='queued'",
            params![scope.tenant_id, scope.user_id, id, lease_until, generation + 1, now_rfc3339()?],
        )?;
        Ok(self.consolidation_get(scope, &id)?)
    }

    /// 心跳续租（短事务；doc4/02 §4）。
    pub fn consolidation_heartbeat(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        lease_secs: u64,
    ) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let lease = lease_until_from(&now, lease_secs)?;
        let n = self.conn_mut().execute(
            "UPDATE consolidation_jobs SET lease_until=?5, updated_at=?6
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status='running' AND claim_generation=?4",
            params![scope.tenant_id, scope.user_id, job_id, expected_generation, lease, now],
        )?;
        Ok(n > 0)
    }

    /// 完成（成功，可能 0 文档产出）：成功结果由 publish_page 写入；此处只推进状态。
    pub fn consolidation_succeed(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        model_name: Option<&str>,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
    ) -> Result<bool, StoreError> {
        let n = self.conn_mut().execute(
            "UPDATE consolidation_jobs SET status='succeeded', model_name=?5,
                    input_tokens=?6, output_tokens=?7, updated_at=?8
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status='running' AND claim_generation=?4",
            params![
                scope.tenant_id,
                scope.user_id,
                job_id,
                expected_generation,
                model_name,
                input_tokens,
                output_tokens,
                now_rfc3339()?
            ],
        )?;
        Ok(n > 0)
    }

    /// 可重试失败：退避后回 queued（run_after=now+delay）；确定性 dead 由调用方显式。
    pub fn consolidation_retryable_fail(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        error_code: &str,
        retry_delay_secs: u64,
    ) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let run_after = lease_until_from(&now, retry_delay_secs)?;
        let n = self.conn_mut().execute(
            "UPDATE consolidation_jobs SET status='queued', error_code=?5, run_after=?6, updated_at=?7
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status='running' AND claim_generation=?4",
            params![scope.tenant_id, scope.user_id, job_id, expected_generation, error_code, run_after, now],
        )?;
        Ok(n > 0)
    }

    /// 确定性失败（坏 JSON、未知 generator version、输入超限）：dead，不自动循环。
    pub fn consolidation_dead(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        error_code: &str,
    ) -> Result<bool, StoreError> {
        let n = self.conn_mut().execute(
            "UPDATE consolidation_jobs SET status='dead', error_code=?5, updated_at=?6
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status='running' AND claim_generation=?4",
            params![scope.tenant_id, scope.user_id, job_id, expected_generation, error_code, now_rfc3339()?],
        )?;
        Ok(n > 0)
    }

    /// 输入失效（模型运行中来源被纠错/遗忘/退休）：stale_input，不发布；新来源重排队。
    pub fn consolidation_stale_input(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
    ) -> Result<bool, StoreError> {
        let n = self.conn_mut().execute(
            "UPDATE consolidation_jobs SET status='stale_input', error_code='STALE_INPUT', updated_at=?5
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status='running' AND claim_generation=?4",
            params![scope.tenant_id, scope.user_id, job_id, expected_generation, now_rfc3339()?],
        )?;
        Ok(n > 0)
    }

    /// 过期 running 恢复（worker 崩溃后）：running 且 lease_until<now → 回 queued，
    /// 保留原冻结输入与 attempts（doc6/05 §5）。
    pub fn consolidation_recover_expired(&mut self, now: &str) -> Result<i64, StoreError> {
        let n = self.conn_mut().execute(
            "UPDATE consolidation_jobs SET status='queued', lease_until=NULL, updated_at=?2
             WHERE status='running' AND (lease_until IS NULL OR lease_until < ?1)",
            params![now, now],
        )?;
        Ok(n as i64)
    }

    /// 读取单 job（scope 内）。
    pub fn consolidation_get(
        &self,
        scope: &ScopeKey,
        job_id: &str,
    ) -> Result<Option<ConsolidationJobRow>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT id, document_kind, document_key, question_version, input_fingerprint,
                        generator_version, status, attempts, run_after, lease_until,
                        claim_generation, error_code, created_at, updated_at
                 FROM consolidation_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, job_id],
                map_job_row,
            )
            .optional()?;
        Ok(row)
    }

    /// 读取 job 的固化输入（按 input_order；重试仍用同一列表）。
    pub fn consolidation_inputs(
        &self,
        scope: &ScopeKey,
        job_id: &str,
    ) -> Result<Vec<JobInput>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT memory_id, memory_version, claim_sha256 FROM consolidation_job_inputs
             WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3 ORDER BY input_order",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, job_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 作业列表（诊断；doc6/05 §5 doctor 输入）。
    pub fn consolidation_list(
        &self,
        scope: &ScopeKey,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConsolidationJobRow>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, document_kind, document_key, question_version, input_fingerprint,
                    generator_version, status, attempts, run_after, lease_until,
                    claim_generation, error_code, created_at, updated_at
             FROM consolidation_jobs
             WHERE tenant_id=?1 AND user_id=?2 AND (?3 IS NULL OR status=?3)
             ORDER BY created_at DESC, id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, status, limit as i64],
            map_job_row,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn map_job_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ConsolidationJobRow> {
    Ok(ConsolidationJobRow {
        id: r.get(0)?,
        document_kind: r.get(1)?,
        document_key: r.get(2)?,
        question_version: r.get(3)?,
        input_fingerprint: r.get(4)?,
        generator_version: r.get(5)?,
        status: r.get(6)?,
        attempts: r.get(7)?,
        run_after: r.get(8)?,
        lease_until: r.get(9)?,
        claim_generation: r.get(10)?,
        error_code: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

/// RFC3339 now + secs（lease/run_after 计算）。
fn lease_until_from(now: &str, secs: u64) -> Result<String, StoreError> {
    let t = chrono::DateTime::parse_from_rfc3339(now)
        .map_err(|e| StoreError::Time(e.to_string()))?
        + chrono::Duration::seconds(secs as i64);
    Ok(t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}
