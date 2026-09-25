//! 提取作业生命周期与候选落库（doc/11 §3.7、doc/13 §3/§5、doc4/02）。
//!
//! 窗口次序与公平调度（doc4/02 §3）：每轮在单个 SQLite 事务内先条件化恢复 lease 过期的
//! running，再原子领取一条"没有未完成前窗"的 due 作业；同一 session 的前窗受阻只阻断
//! 该 session，不阻塞其他 session。前驱与下界一律按 `through_event_seq` 数值比较，
//! 不用 window_key 字符串序。claim_generation 在每次领取/恢复时原子递增，用于隔离
//! 失去所有权的旧执行者。

use memory_domain::{claim_sha256, fold_whitespace, normalize_v1, MemoryKind, Origin, ScopeKey};
use memory_extract::{Admission, WindowEvent};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

#[derive(Debug)]
pub enum FlushOutcome {
    NothingToExtract,
    Created { job_id: String },
    Existing { job_id: String, status: String },
}

pub enum FailOutcome {
    Retryable { run_after: String },
    Dead,
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub tenant_id: String,
    pub user_id: String,
    pub host_id: String,
    pub session_id: String,
    pub window_key: String,
    pub through_event_seq: i64,
    pub status: String,
    pub attempts: i32,
    pub run_after: String,
    pub created_at: String,
    pub updated_at: String,
    /// 生成该作业时的提取规则版本（doc2/05 §3），随作业持久化。
    pub prompt_version: String,
    /// 当前执行权代际：每次领取/过期恢复原子 +1；旧代际的提交一律失效（doc4/02 §4—5）。
    pub claim_generation: i64,
}

/// 候选落库结果（审计/诊断用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateOutcome {
    Active { memory_id: String },
    Held { reason: &'static str },
    Rejected { reason: &'static str },
}

impl Store {
    /// POST /v1/extraction/flush（doc/12 §4）。window_key = v1:<through_seq>。
    pub fn flush_window(
        &mut self,
        scope: &ScopeKey,
        host_id: &str,
        session_id: &str,
        through_event_seq: i64,
    ) -> Result<FlushOutcome, StoreError> {
        let max_seq = self.max_event_seq(scope, host_id, session_id)?.unwrap_or(-1);
        if through_event_seq > max_seq {
            return Err(StoreError::StateConflict);
        }
        // 窗口内须有 role=user 且 source_kind=user 的事件，否则无事可做。
        let has_user: bool = self
            .conn()
            .query_row(
                "SELECT 1 FROM evidence_events
                 WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4
                   AND event_seq<=?5 AND role='user' AND source_kind='user' LIMIT 1",
                params![scope.tenant_id, scope.user_id, host_id, session_id, through_event_seq],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !has_user {
            return Ok(FlushOutcome::NothingToExtract);
        }
        // flush 小于已排最大 through_seq 且非同一 window_key → 409（doc/13 §3）。
        let max_scheduled: Option<i64> = self
            .conn()
            .query_row(
                "SELECT MAX(through_event_seq) FROM extraction_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4",
                params![scope.tenant_id, scope.user_id, host_id, session_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        let window_key = format!("v1:{through_event_seq}");
        if let Some(m) = max_scheduled {
            if through_event_seq < m {
                return Err(StoreError::StateConflict);
            }
        }
        // 幂等：同 window_key 返回原 job。
        let existing: Option<(String, String)> = self
            .conn()
            .query_row(
                "SELECT id, status FROM extraction_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4 AND window_key=?5",
                params![scope.tenant_id, scope.user_id, host_id, session_id, window_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((job_id, status)) = existing {
            return Ok(FlushOutcome::Existing { job_id, status });
        }
        let job_id = Uuid::now_v7().to_string();
        let now = now_rfc3339()?;
        self.conn_mut().execute(
            "INSERT INTO extraction_jobs
             (id, tenant_id, user_id, host_id, session_id, window_key, through_event_seq,
              status, attempts, run_after, created_at, updated_at, prompt_version)
             VALUES (?1,?2,?3,?4,?5,?6,?7,'queued',0,?8,?9,?10,?11)",
            params![
                job_id, scope.tenant_id, scope.user_id, host_id, session_id, window_key,
                through_event_seq, now, now, now, memory_contract::EXTRACT_PROMPT_VERSION
            ],
        )?;
        Ok(FlushOutcome::Created { job_id })
    }

    /// 本窗口下界：同 scope/host/session 中 `through_event_seq` 小于当前窗口、且
    /// succeeded 或显式 skipped 的最大数值；无前窗 -1（doc4/02 §3、doc4/03 §4）。
    /// 按 through_event_seq 数值比较，不用 window_key 字符串序；未跳过的 dead
    /// 不计入下界（其窗口不可视为已越过）。
    pub fn window_lower_bound(
        &self,
        scope: &ScopeKey,
        host_id: &str,
        session_id: &str,
        through_event_seq: i64,
    ) -> Result<i64, StoreError> {
        let v: Option<i64> = self
            .conn()
            .query_row(
                "SELECT MAX(through_event_seq) FROM extraction_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4
                   AND through_event_seq<?5
                   AND (status='succeeded' OR EXISTS (
                     SELECT 1 FROM extraction_job_skips s
                     WHERE s.tenant_id=extraction_jobs.tenant_id
                       AND s.user_id=extraction_jobs.user_id
                       AND s.job_id=extraction_jobs.id))",
                params![scope.tenant_id, scope.user_id, host_id, session_id, through_event_seq],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        Ok(v.unwrap_or(-1))
    }

    /// 每轮一个 SQLite 事务（doc4/02 §3—4）：先条件化恢复 lease 过期的 running，
    /// 再原子领取一条"没有未完成前窗"的 due 作业。
    ///
    /// 前窗阻断按 (scope,host,session) 内 `through_event_seq` 数值判定：存在更小
    /// through、既非 succeeded 又无 skip 行的作业时，该候选行被排除、继续尝试其他
    /// 行——一个 session 的坏窗口不让其他 session 饥饿。到期条件对 queued 与
    /// retryable_failed 统一为 `run_after<=now`（退避不被绕过）。领取原子递增
    /// claim_generation 并写 lease。无可执行作业返回 None。
    pub fn claim_next_ordered_job(&mut self, now: &str) -> Result<Option<JobRow>, StoreError> {
        let tx = self.conn_mut().transaction()?;
        Self::recover_expired_running_tx(&tx, now)?;
        let mut job: Option<JobRow> = tx
            .query_row(
                &format!(
                    "SELECT a.{cols}
                     FROM extraction_jobs a
                     WHERE a.status IN ('queued','retryable_failed') AND a.run_after<=?1
                       AND NOT EXISTS (
                         SELECT 1 FROM extraction_jobs b
                         WHERE b.tenant_id=a.tenant_id AND b.user_id=a.user_id
                           AND b.host_id=a.host_id AND b.session_id=a.session_id
                           AND b.through_event_seq<a.through_event_seq
                           AND b.status<>'succeeded'
                           AND NOT EXISTS (SELECT 1 FROM extraction_job_skips s
                                           WHERE s.tenant_id=b.tenant_id AND s.user_id=b.user_id
                                             AND s.job_id=b.id)
                       )
                     ORDER BY a.run_after, a.created_at, a.id LIMIT 1",
                    cols = JOB_ROW_COLUMNS
                ),
                params![now],
                job_row_mapper(),
            )
            .optional()?;
        let Some(mut job) = job.as_mut().map(|j| j.clone()) else {
            // 无候选也要提交：同事务内的过期恢复必须落库。
            tx.commit()?;
            return Ok(None);
        };
        // 原子领取：仅当仍为 due 原状态才置 running；generation 原子 +1 并写 lease。
        let lease = (chrono::DateTime::parse_from_rfc3339(now)
            .map_err(|e| StoreError::Time(e.to_string()))?
            .with_timezone(&chrono::Utc)
            + chrono::Duration::seconds(memory_contract::JOB_LEASE_SECS as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let n = tx.execute(
            "UPDATE extraction_jobs SET status='running', lease_until=?1, updated_at=?2,
               claim_generation=claim_generation+1
             WHERE id=?3 AND status IN ('queued','retryable_failed') AND run_after<=?4",
            params![lease, now, job.id, now],
        )?;
        if n == 0 {
            tx.commit()?;
            return Ok(None);
        }
        job.status = "running".into();
        job.claim_generation += 1;
        tx.commit()?;
        Ok(Some(job))
    }

    /// 条件化恢复 lease 过期（或异常 NULL）的 running 行（doc4/02 §4）：
    /// generation +1、attempts +1（失去所有权的执行计入次数）；未达
    /// `JOB_MAX_ATTEMPTS` 按本次丢失尝试对应延迟回 `retryable_failed`，达上限写
    /// `dead`，错误码均为 `WORKER_LEASE_EXPIRED`。恢复 UPDATE 带
    /// `status='running' AND claim_generation=旧值` 条件，不覆盖已被成功提交的行。
    /// 只在调用方事务内执行；全部时间取自 `now` 参数，保证确定性。
    fn recover_expired_running_tx(
        tx: &rusqlite::Transaction<'_>,
        now: &str,
    ) -> Result<usize, StoreError> {
        let expired: Vec<(String, i64, i32)> = {
            let mut stmt = tx.prepare(
                "SELECT id, claim_generation, attempts FROM extraction_jobs
                 WHERE status='running' AND (lease_until IS NULL OR lease_until<=?1)",
            )?;
            let rows = stmt.query_map(params![now], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i32>(2)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let now_dt = chrono::DateTime::parse_from_rfc3339(now)
            .map_err(|e| StoreError::Time(e.to_string()))?
            .with_timezone(&chrono::Utc);
        let max = memory_contract::JOB_MAX_ATTEMPTS as i32;
        let mut recovered = 0;
        for (id, gen, attempts) in expired {
            let new_attempts = attempts + 1;
            let n = tx.execute(
                "UPDATE extraction_jobs SET claim_generation=?1, attempts=?2
                 WHERE id=?3 AND status='running' AND claim_generation=?4",
                params![gen + 1, new_attempts, id, gen],
            )?;
            if n == 0 {
                continue;
            }
            if new_attempts >= max {
                tx.execute(
                    "UPDATE extraction_jobs SET status='dead', error_code='WORKER_LEASE_EXPIRED',
                       lease_until=NULL, updated_at=?1 WHERE id=?2",
                    params![now, id],
                )?;
            } else {
                let delay = memory_contract::JOB_RETRY_DELAYS_SECS
                    .get(new_attempts as usize - 1)
                    .copied()
                    .unwrap_or(45);
                let run_after = (now_dt + chrono::Duration::seconds(delay as i64))
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
                tx.execute(
                    "UPDATE extraction_jobs SET status='retryable_failed',
                       error_code='WORKER_LEASE_EXPIRED', run_after=?1, lease_until=NULL,
                       updated_at=?2 WHERE id=?3",
                    params![run_after, now, id],
                )?;
            }
            recovered += 1;
        }
        Ok(recovered)
    }

    /// 读取窗口事件（下界, through]。
    pub fn load_window_events(
        &self,
        scope: &ScopeKey,
        job: &JobRow,
        lower_bound: i64,
    ) -> Result<Vec<WindowEvent>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, role, source_kind, occurred_at, content FROM evidence_events
             WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4
               AND event_seq>?5 AND event_seq<=?6
             ORDER BY event_seq",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, job.host_id, job.session_id, lower_bound, job.through_event_seq],
            |r| {
                Ok(WindowEvent {
                    id: r.get(0)?,
                    role: r.get(1)?,
                    source_kind: r.get(2)?,
                    occurred_at: r.get(3)?,
                    content: r.get(4)?,
                })
            },
        )?;
        let events = rows.collect::<Result<Vec<_>, _>>()?;
        // 窗口上限（doc/13 §3）：100 事件 / 32 KiB，超限不悄悄截断。
        if events.len() > memory_contract::EXTRACTION_WINDOW_MAX_EVENTS {
            return Err(StoreError::StateConflict);
        }
        let total: usize = events.iter().map(|e| e.content.len()).sum();
        if total > memory_contract::EXTRACTION_INPUT_MAX_BYTES {
            return Err(StoreError::StateConflict);
        }
        Ok(events)
    }

    pub fn complete_job(
        &mut self,
        job_id: &str,
        attempts: i32,
        model_name: &str,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
    ) -> Result<(), StoreError> {
        let now = now_rfc3339()?;
        self.conn_mut().execute(
            "UPDATE extraction_jobs SET status='succeeded', attempts=?1, model_name=?2,
             input_tokens=?3, output_tokens=?4, lease_until=NULL, updated_at=?5 WHERE id=?6",
            params![attempts, model_name, input_tokens, output_tokens, now, job_id],
        )?;
        Ok(())
    }

    pub fn fail_job(&mut self, job_id: &str, attempts: i32, error_code: &str) -> Result<FailOutcome, StoreError> {
        let max = memory_contract::JOB_MAX_ATTEMPTS as i32;
        let now_dt = chrono::Utc::now();
        let now = now_dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        if attempts >= max {
            self.conn_mut().execute(
                "UPDATE extraction_jobs SET status='dead', attempts=?1, error_code=?2, lease_until=NULL, updated_at=?3 WHERE id=?4",
                params![attempts, error_code, now, job_id],
            )?;
            return Ok(FailOutcome::Dead);
        }
        let delay = memory_contract::JOB_RETRY_DELAYS_SECS
            .get(attempts as usize - 1)
            .copied()
            .unwrap_or(45);
        let run_after = (now_dt + chrono::Duration::seconds(delay as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        self.conn_mut().execute(
            "UPDATE extraction_jobs SET status='retryable_failed', attempts=?1, error_code=?2,
             run_after=?3, lease_until=NULL, updated_at=?4 WHERE id=?5",
            params![attempts, error_code, run_after, now, job_id],
        )?;
        Ok(FailOutcome::Retryable { run_after })
    }

    pub fn get_job(&self, scope: &ScopeKey, job_id: &str) -> Result<Option<JobRow>, StoreError> {
        let row = self
            .conn()
            .query_row(
                &format!(
                    "SELECT {JOB_ROW_COLUMNS} FROM extraction_jobs
                     WHERE tenant_id=?1 AND user_id=?2 AND id=?3"
                ),
                params![scope.tenant_id, scope.user_id, job_id],
                job_row_mapper(),
            )
            .optional()?;
        Ok(row)
    }

    pub fn retry_dead_job(&mut self, scope: &ScopeKey, job_id: &str) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let n = self.conn_mut().execute(
            "UPDATE extraction_jobs SET status='queued', run_after=?1, lease_until=NULL, updated_at=?2
             WHERE tenant_id=?3 AND user_id=?4 AND id=?5 AND status='dead'",
            params![now, now, scope.tenant_id, scope.user_id, job_id],
        )?;
        Ok(n > 0)
    }

    /// 诊断：统计指定 reason 的候选数（不返回正文）。
    pub fn count_candidates_by_reason(&self, scope: &ScopeKey, reason: &str) -> Result<i64, StoreError> {
        let n = self.conn().query_row(
            "SELECT count(*) FROM memory_candidates
             WHERE tenant_id=?1 AND user_id=?2 AND reason_code=?3",
            params![scope.tenant_id, scope.user_id, reason],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// 候选落库：写候选行（含证据关联），Active 且通过查库规则时建 active 记忆。
    /// doc/13 §5 规则 8（去重/抑制源）与 9（属性冲突）在此查库判定。
    pub fn save_candidate(
        &mut self,
        scope: &ScopeKey,
        job: &JobRow,
        origin: &Origin,
        c: &memory_extract::ModelCandidate,
        admission: Admission,
    ) -> Result<CandidateOutcome, StoreError> {
        let quote = fold_whitespace(&c.quote);
        let quote_hash = hex::encode(sha2::Sha256::digest(quote.as_bytes()));
        let kind = match c.kind.as_str() {
            "fact" => MemoryKind::Fact,
            "preference" => MemoryKind::Preference,
            "instruction" => MemoryKind::Instruction,
            "episode" => MemoryKind::Episode,
            _ => return Ok(CandidateOutcome::Rejected { reason: "BAD_KIND" }),
        };
        let now = now_rfc3339()?;
        let candidate_id = Uuid::now_v7().to_string();

        // 先决：来源事件（含原文 span 验证）。
        let (ev_host, ev_session, ev_role, ev_source, ev_content) = self
            .get_evidence(scope, &c.source_event_id)?
            .ok_or(StoreError::EvidenceNotFound)?;
        if ev_role != "user" || ev_source != "user" {
            return Ok(CandidateOutcome::Rejected { reason: "BAD_SOURCE" });
        }
        let (start, end) = match memory_domain::find_quote_span(&ev_content, &c.quote) {
            Some(v) => v,
            None => return Ok(CandidateOutcome::Rejected { reason: "QUOTE_MISMATCH" }),
        };
        let _ = (ev_host, ev_session);

        let (status, reason_code, source_class): (&str, Option<&str>, &str) = match &admission {
            Admission::Active => ("candidate", None, "user_explicit"),
            Admission::Held(r) => ("held", Some(*r), "model_inferred"),
            Admission::Rejected(r) => ("rejected", Some(*r), "model_inferred"),
        };
        // 幂等：同 (job, evidence, kind, quote_sha) 已存在则跳过。
        let dup: bool = self
            .conn()
            .query_row(
                "SELECT 1 FROM memory_candidates
                 WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3 AND primary_evidence_id=?4
                   AND kind=?5 AND quote_sha256=?6 LIMIT 1",
                params![scope.tenant_id, scope.user_id, job.id, c.source_event_id, kind.as_str(), quote_hash],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if dup {
            return Ok(CandidateOutcome::Rejected { reason: "DUPLICATE_CANDIDATE" });
        }
        let tx = self.conn_mut().transaction()?;
        tx.execute(
            "INSERT INTO memory_candidates
             (id, tenant_id, user_id, job_id, primary_evidence_id, kind, quote, quote_sha256, claim,
              source_class, status, reason_code, model_confidence, occurred_at, valid_until, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                candidate_id,
                scope.tenant_id,
                scope.user_id,
                job.id,
                c.source_event_id,
                kind.as_str(),
                c.quote,
                quote_hash,
                quote,
                source_class,
                status,
                reason_code,
                c.confidence,
                c.occurred_at,
                c.valid_until,
                now
            ],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO candidate_evidence (id, tenant_id, user_id, candidate_id, evidence_id, start_byte, end_byte)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![Uuid::now_v7().to_string(), scope.tenant_id, scope.user_id, candidate_id, c.source_event_id, start as i64, end as i64],
        )?;

        let mut outcome = CandidateOutcome::Rejected { reason: reason_code.unwrap_or("REJECTED") };
        if admission == Admission::Active {
            let claim_hash = claim_sha256(kind, &quote);
            // 规则 8a：同 kind+hash 的 active → 仅加证据。
            let existing_active: Option<(String, i64)> = tx
                .query_row(
                    "SELECT id, version FROM memories
                     WHERE tenant_id=?1 AND user_id=?2 AND kind=?3 AND claim_sha256=?4 AND status='active'",
                    params![scope.tenant_id, scope.user_id, kind.as_str(), claim_hash],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((memory_id, _version)) = existing_active {
                tx.execute(
                    "INSERT OR IGNORE INTO memory_evidence (id, tenant_id, user_id, memory_id, evidence_id, start_byte, end_byte)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![Uuid::now_v7().to_string(), scope.tenant_id, scope.user_id, memory_id, c.source_event_id, start as i64, end as i64],
                )?;
                outcome = CandidateOutcome::Rejected { reason: "DUPLICATE_ACTIVE" };
            } else {
                // 规则 8b：同旧证据+hash 的 forgotten → 抑制源，不复活。
                let suppressed: bool = tx
                    .query_row(
                        "SELECT 1 FROM suppressed_sources
                         WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3 AND claim_sha256=?4 LIMIT 1",
                        params![scope.tenant_id, scope.user_id, c.source_event_id, claim_hash],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);
                if suppressed {
                    outcome = CandidateOutcome::Rejected { reason: "SUPPRESSED_SOURCE" };
                } else {
                    // 规则 9：属性键相同而值不同的 active → held:POSSIBLE_CONFLICT。
                    if let Some(conflict_id) = Self::attribute_conflict(&tx, scope, kind, &quote)? {
                        let _ = conflict_id;
                        tx.execute(
                            "UPDATE memory_candidates SET status='held', reason_code='POSSIBLE_CONFLICT' WHERE id=?1",
                            params![candidate_id],
                        )?;
                        outcome = CandidateOutcome::Held { reason: "POSSIBLE_CONFLICT" };
                    } else {
                        // 通过：建 active memory（与 remember 同一事务模式）。
                        let memory_id = Uuid::now_v7().to_string();
                        tx.execute(
                            "INSERT INTO memories
                             (id, tenant_id, user_id, kind, claim, normalized_claim, claim_sha256, source_class,
                              status, version, occurred_at, valid_from, valid_until, origin_host_id, origin_agent_id,
                              created_at, updated_at)
                             VALUES (?1,?2,?3,?4,?5,?6,?7,'user_explicit','active',1,NULL,NULL,NULL,?8,?9,?10,?11)",
                            params![
                                memory_id, scope.tenant_id, scope.user_id, kind.as_str(), quote,
                                normalize_v1(&quote), claim_hash, origin.host_id, origin.agent_id, now, now
                            ],
                        )?;
                        tx.execute(
                            "INSERT INTO memory_evidence (id, tenant_id, user_id, memory_id, evidence_id, start_byte, end_byte)
                             VALUES (?1,?2,?3,?4,?5,?6,?7)",
                            params![Uuid::now_v7().to_string(), scope.tenant_id, scope.user_id, memory_id, c.source_event_id, start as i64, end as i64],
                        )?;
                        tx.execute(
                            "INSERT INTO memory_revisions
                             (tenant_id, user_id, memory_id, version, previous_claim, new_claim, previous_status, new_status,
                              actor_kind, actor_id, reason_code, changed_at)
                             VALUES (?1,?2,?3,1,NULL,?4,NULL,'active','system',?5,'extract_v1',?6)",
                            params![scope.tenant_id, scope.user_id, memory_id, quote, job.id, now],
                        )?;
                        tx.execute(
                            "UPDATE memory_candidates SET status='rejected', reason_code='PROMOTED' WHERE id=?1",
                            params![candidate_id],
                        )?;
                        outcome = CandidateOutcome::Active { memory_id };
                    }
                }
            }
        }
        Self::mark_index_dirty(&tx)?;
        tx.commit()?;
        // 索引事务（失败保留 dirty）。
        if let CandidateOutcome::Active { memory_id } = &outcome {
            if let Err(e) = self.reindex_memory(scope, memory_id, &quote, true) {
                eprintln!("[memoryd] 索引更新失败 memory_id={memory_id}: {e}");
            }
        }
        Ok(outcome)
    }

    /// 规则 9 的属性冲突识别（doc/13 §5.9）：对已有 active claim 与新 quote 提取同一套
    /// 属性键；key 相同而值不同 → POSSIBLE_CONFLICT。无法识别的内容不运行语义冲突识别。
    fn attribute_conflict(
        conn: &rusqlite::Connection,
        scope: &ScopeKey,
        kind: MemoryKind,
        quote: &str,
    ) -> Result<Option<String>, StoreError> {
        let new_key = match extract_attr_key(quote, kind) {
            Some(k) => k,
            None => return Ok(None),
        };
        let mut stmt = conn.prepare(
            "SELECT id, claim FROM memories
             WHERE tenant_id=?1 AND user_id=?2 AND kind=?3 AND status='active'",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, kind.as_str()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        for row in rows {
            let (id, claim) = row?;
            if extract_attr_key(&claim, kind) == Some(new_key)
                && normalize_v1(&claim) != normalize_v1(quote)
            {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }
}

/// 属性键提取（NFKC+大小写折叠后的固定前后缀，doc/13 §5.9）。
fn extract_attr_key(quote: &str, kind: MemoryKind) -> Option<&'static str> {
    let q = quote.to_lowercase();
    if (q.starts_with("我叫") || q.starts_with("my name is")) && kind == MemoryKind::Fact {
        return Some("name");
    }
    if (q.starts_with("我住在") || q.starts_with("i live in")) && kind == MemoryKind::Fact {
        return Some("residence");
    }
    if q.starts_with("我在")
        && q.chars().take(20).collect::<String>().contains("工作")
        && kind == MemoryKind::Fact
    {
        return Some("occupation");
    }
    if q.starts_with("以后用") && q.contains("回答") && kind == MemoryKind::Instruction {
        return Some("response_language");
    }
    None
}

fn job_row_mapper() -> impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<JobRow> {
    |r: &rusqlite::Row<'_>| {
        Ok(JobRow {
            id: r.get(0)?,
            tenant_id: r.get(1)?,
            user_id: r.get(2)?,
            host_id: r.get(3)?,
            session_id: r.get(4)?,
            window_key: r.get(5)?,
            through_event_seq: r.get(6)?,
            status: r.get(7)?,
            attempts: r.get(8)?,
            run_after: r.get(9)?,
            created_at: r.get(10)?,
            updated_at: r.get(11)?,
            prompt_version: r.get(12)?,
            claim_generation: r.get(13)?,
        })
    }
}

/// JobRow 查询列（与 job_row_mapper 的列序一一对应）。
const JOB_ROW_COLUMNS: &str = "id, tenant_id, user_id, host_id, session_id, window_key, \
     through_event_seq, status, attempts, run_after, created_at, updated_at, prompt_version, \
     claim_generation";

use sha2::Digest;

#[cfg(test)]
mod tests {
    //! doc4/02 §6 的确定性检查：固定相对时钟 + 临时 SQLite；不涉及模型调用。
    use super::*;
    use crate::{Store, StoreError};
    use memory_domain::{Origin, ScopeKey};

    fn migrations_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join("migrations")
    }

    /// 以某 RFC3339 时刻为基准加秒（全部时间断言均相对已落库时间戳推导，保证确定性）。
    fn plus_secs(rfc3339: &str, secs: i64) -> String {
        (chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Utc)
            + chrono::Duration::seconds(secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    }

    fn setup(tag: &str) -> Store {
        let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
        let dir = std::env::temp_dir().join(format!("am-jobs-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        store.principal_add("t", "u", &dir.join("u.token")).unwrap();
        store
    }

    fn scope_of(store: &Store, tenant: &str, user: &str) -> ScopeKey {
        let _ = store;
        ScopeKey { tenant_id: tenant.into(), user_id: user.into() }
    }

    fn ingest(store: &mut Store, scope: &ScopeKey, session: &str, seq: i64, content: &str) {
        let t = chrono::Utc::now();
        let origin = Origin { host_id: "dsh".into(), agent_id: "agent-a".into(), session_id: session.into() };
        store
            .record_evidence(scope, &origin, seq, "user", "user", &t, content)
            .unwrap();
    }

    fn flush(store: &mut Store, scope: &ScopeKey, session: &str, through: i64) -> String {
        match store.flush_window(scope, "dsh", session, through).unwrap() {
            FlushOutcome::Created { job_id } => job_id,
            other => panic!("应创建作业，实际 {other:?}"),
        }
    }

    fn job_field(store: &Store, job_id: &str, field: &str) -> String {
        // field 仅由测试常量传入，不来自外部输入。
        store
            .conn()
            .query_row(
                &format!("SELECT {field} FROM extraction_jobs WHERE id=?1"),
                params![job_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
            .unwrap_or_else(|| "<NULL>".into())
    }

    #[test]
    fn backoff_retryable_requires_run_after_expiry() {
        // doc4/02 §2：retryable_failed 的到期只看 run_after，退避不可被绕过。
        let mut store = setup("backoff");
        let scope = scope_of(&store, "t", "u");
        ingest(&mut store, &scope, "s1", 1, "我叫洛溪");
        let job_id = flush(&mut store, &scope, "s1", 1);
        assert!(matches!(
            store.fail_job(&job_id, 1, "MODEL_TIMEOUT").unwrap(),
            FailOutcome::Retryable { .. }
        ));
        let run_after = job_field(&store, &job_id, "run_after");

        // 退避未到：不可领取，状态不变、无 lease。
        let claimed = store.claim_next_ordered_job(&plus_secs(&run_after, -1)).unwrap();
        assert!(claimed.is_none(), "退避期内不得领取");
        assert_eq!(job_field(&store, &job_id, "status"), "retryable_failed");
        assert_eq!(job_field(&store, &job_id, "lease_until"), "<NULL>");

        // 到期：可领取，running + generation 0→1。
        let job = store.claim_next_ordered_job(&plus_secs(&run_after, 1)).unwrap().unwrap();
        assert_eq!(job.id, job_id);
        assert_eq!(job.status, "running");
        assert_eq!(job.claim_generation, 1);
        assert_eq!(job.attempts, 1, "领取不加 attempts，由执行结束写入");
    }

    #[test]
    fn expired_running_recovered_then_dead_at_third() {
        // doc4/02 §4：lease 过期恢复计一次 attempts；第三次过期 dead。
        let mut store = setup("recover");
        let scope = scope_of(&store, "t", "u");
        ingest(&mut store, &scope, "s1", 1, "我叫洛溪");
        let job_id = flush(&mut store, &scope, "s1", 1);
        let run_after = job_field(&store, &job_id, "run_after");
        let t0 = plus_secs(&run_after, 1);

        // 第 1 次执行：claim 后 lease 过期 → 恢复为 retryable，attempts 0→1，generation +1。
        let job = store.claim_next_ordered_job(&t0).unwrap().unwrap();
        assert_eq!(job.claim_generation, 1);
        assert!(store.claim_next_ordered_job(&plus_secs(&t0, 91)).unwrap().is_none());
        assert_eq!(job_field(&store, &job_id, "status"), "retryable_failed");
        assert_eq!(job_field(&store, &job_id, "error_code"), "WORKER_LEASE_EXPIRED");
        let (gen, attempts): (i64, i32) = store.conn().query_row(
            "SELECT claim_generation, attempts FROM extraction_jobs WHERE id=?1",
            params![job_id], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!((gen, attempts), (2, 1));

        // 第 2 次执行 → 过期恢复：attempts 2。
        let t1 = plus_secs(&job_field(&store, &job_id, "run_after"), 1);
        assert!(store.claim_next_ordered_job(&t1).unwrap().is_some());
        assert!(store.claim_next_ordered_job(&plus_secs(&t1, 91)).unwrap().is_none());
        let (gen, attempts): (i64, i32) = store.conn().query_row(
            "SELECT claim_generation, attempts FROM extraction_jobs WHERE id=?1",
            params![job_id], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!((gen, attempts), (4, 2));

        // 第 3 次执行 → 过期恢复：达上限，dead。
        let t2 = plus_secs(&job_field(&store, &job_id, "run_after"), 1);
        assert!(store.claim_next_ordered_job(&t2).unwrap().is_some());
        assert!(store.claim_next_ordered_job(&plus_secs(&t2, 91)).unwrap().is_none());
        assert_eq!(job_field(&store, &job_id, "status"), "dead");
        assert_eq!(job_field(&store, &job_id, "error_code"), "WORKER_LEASE_EXPIRED");
        let attempts: i32 = store.conn().query_row(
            "SELECT attempts FROM extraction_jobs WHERE id=?1", params![job_id], |r| r.get(0),
        ).unwrap();
        assert_eq!(attempts, 3);
    }

    #[test]
    fn same_session_claims_in_through_order() {
        // doc4/02 §3：同 session 按前驱链推进；前窗未 succeeded 时不领后窗。
        let mut store = setup("order");
        let scope = scope_of(&store, "t", "u");
        for seq in 1..=10 {
            ingest(&mut store, &scope, "s1", seq, &format!("事件{seq}"));
        }
        let job_a = flush(&mut store, &scope, "s1", 5);
        let _job_b = flush(&mut store, &scope, "s1", 10);
        let run_after = job_field(&store, &job_a, "run_after");
        let now = plus_secs(&run_after, 1);

        // 前窗（through 5）先领。
        let first = store.claim_next_ordered_job(&now).unwrap().unwrap();
        assert_eq!(first.through_event_seq, 5);
        // 前窗 running 未完成：后窗不可领，返回 None（不是还原占位循环）。
        assert!(store.claim_next_ordered_job(&plus_secs(&now, 1)).unwrap().is_none());
        // 前窗成功后后窗可领。
        store.complete_job(&job_a, 1, "mock", None, None).unwrap();
        let second = store.claim_next_ordered_job(&plus_secs(&now, 2)).unwrap().unwrap();
        assert_eq!(second.through_event_seq, 10);
    }

    #[test]
    fn blocked_session_does_not_starve_other_sessions() {
        // doc4/02 §3：s1 前窗 dead 未跳过 → s1 后窗被排除，但 s2 照常领取。
        let mut store = setup("fair");
        let scope = scope_of(&store, "t", "u");
        ingest(&mut store, &scope, "s1", 1, "s1-事件1");
        let dead_job = flush(&mut store, &scope, "s1", 1);
        assert!(matches!(
            store.fail_job(&dead_job, 3, "MODEL_TIMEOUT").unwrap(),
            FailOutcome::Dead
        ));
        ingest(&mut store, &scope, "s1", 2, "s1-事件2");
        let blocked_job = flush(&mut store, &scope, "s1", 2);
        ingest(&mut store, &scope, "s2", 1, "s2-事件1");
        let other_job = flush(&mut store, &scope, "s2", 1);

        let now = plus_secs(&job_field(&store, &other_job, "run_after"), 1);
        let claimed = store.claim_next_ordered_job(&now).unwrap().unwrap();
        assert_eq!(claimed.session_id, "s2", "受阻 session 被跳过，其他 session 前进");
        store.complete_job(&other_job, 1, "mock", None, None).unwrap();

        // 只剩受阻作业：返回 None 且不反复取出、不写 lease。
        assert!(store.claim_next_ordered_job(&plus_secs(&now, 2)).unwrap().is_none());
        assert_eq!(job_field(&store, &blocked_job, "status"), "queued");
        assert_eq!(job_field(&store, &blocked_job, "lease_until"), "<NULL>");
    }

    #[test]
    fn cross_scope_predecessor_isolation() {
        // doc4/02 §3：前窗阻断只看同一 (tenant,user,host,session)；u1 的 dead 不影响 u2。
        let mut store = setup("scope");
        let dir = std::env::temp_dir().join(format!("am-jobs-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        store.principal_add("t", "u2", &dir.join("u2.token")).unwrap();

        let scope1 = scope_of(&store, "t", "u");
        let scope2 = scope_of(&store, "t", "u2");
        ingest(&mut store, &scope1, "s1", 1, "u1-事件");
        let dead_job = flush(&mut store, &scope1, "s1", 1);
        assert!(matches!(
            store.fail_job(&dead_job, 3, "MODEL_TIMEOUT").unwrap(),
            FailOutcome::Dead
        ));
        ingest(&mut store, &scope2, "s1", 1, "u2-事件");
        let job2 = flush(&mut store, &scope2, "s1", 1);

        let now = plus_secs(&job_field(&store, &job2, "run_after"), 1);
        let claimed = store.claim_next_ordered_job(&now).unwrap().unwrap();
        assert_eq!(claimed.user_id, "u2", "跨用户前窗不得串扰");
    }

    #[test]
    fn lower_bound_is_numeric_not_lexicographic() {
        // doc4/02 §3/§6：v1:99 → v1:100 的下界按数值 = 99（字典序会漏掉 99 得 -1）。
        let mut store = setup("lowerbound");
        let scope = scope_of(&store, "t", "u");
        for seq in 1..=100 {
            ingest(&mut store, &scope, "s1", seq, &format!("事件{seq}"));
        }
        let job_99 = flush(&mut store, &scope, "s1", 99);
        assert_eq!(store.window_lower_bound(&scope, "dsh", "s1", 99).unwrap(), -1);
        store.complete_job(&job_99, 1, "mock", None, None).unwrap();
        let _job_100 = flush(&mut store, &scope, "s1", 100);
        assert_eq!(
            store.window_lower_bound(&scope, "dsh", "s1", 100).unwrap(),
            99,
            "下界必须是 99，不得回退到 -1 或更早窗口"
        );

        // 未跳过的 dead 不计入下界；succeeded 计入。
        ingest(&mut store, &scope, "s2", 1, "s2-事件1");
        let dead_job = flush(&mut store, &scope, "s2", 1);
        assert!(matches!(store.fail_job(&dead_job, 3, "MODEL_TIMEOUT").unwrap(), FailOutcome::Dead));
        ingest(&mut store, &scope, "s2", 2, "s2-事件2");
        let _job_2 = flush(&mut store, &scope, "s2", 2);
        assert_eq!(
            store.window_lower_bound(&scope, "dsh", "s2", 2).unwrap(),
            -1,
            "未跳过的 dead 前窗不推进下界"
        );
    }
}
