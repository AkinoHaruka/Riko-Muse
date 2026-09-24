//! 提取作业生命周期与候选落库（doc/11 §3.7、doc/13 §3/§5）。
//!
//! 窗口次序：worker 只领取同一 session 中之前窗口全部 succeeded 的最小待办窗口；
//! 本窗口下界 = 前一成功窗口的 through_event_seq（首窗为 -1），无需额外 cursor 表。

use memory_domain::{claim_sha256, fold_whitespace, normalize_v1, MemoryKind, Origin, ScopeKey};
use memory_extract::{Admission, WindowEvent};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

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

    /// 本窗口下界：前一成功窗口的 through_event_seq；首窗 -1（doc/13 §3）。
    pub fn window_lower_bound(
        &self,
        scope: &ScopeKey,
        host_id: &str,
        session_id: &str,
        current_window_key: &str,
    ) -> Result<i64, StoreError> {
        let v: Option<i64> = self
            .conn()
            .query_row(
                "SELECT MAX(through_event_seq) FROM extraction_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4
                   AND status='succeeded' AND window_key<?5",
                params![scope.tenant_id, scope.user_id, host_id, session_id, current_window_key],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        Ok(v.unwrap_or(-1))
    }

    /// 领取全局最早 due 作业（queued 且到点，或 retryable_failed 且 lease 已过）。
    pub fn claim_due_job(&mut self, now: &str) -> Result<Option<JobRow>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT id, tenant_id, user_id, host_id, session_id, window_key, through_event_seq,
                        status, attempts, run_after, created_at, updated_at, prompt_version
                 FROM extraction_jobs
                 WHERE (status='queued' AND run_after<=?1)
                    OR (status='retryable_failed' AND (lease_until IS NULL OR lease_until<=?1))
                 ORDER BY created_at LIMIT 1",
                params![now],
                job_row_mapper(),
            )
            .optional()?;
        let Some(job) = row else { return Ok(None) };
        // 原子 claim：仅当状态未变才置 running。
        let lease = (chrono::DateTime::parse_from_rfc3339(now)
            .map_err(|e| StoreError::Time(e.to_string()))?
            .with_timezone(&chrono::Utc)
            + chrono::Duration::seconds(memory_contract::JOB_LEASE_SECS as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let n = self.conn_mut().execute(
            "UPDATE extraction_jobs SET status='running', lease_until=?1, updated_at=?2
             WHERE id=?3 AND status IN ('queued','retryable_failed')",
            params![lease, now, job.id],
        )?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(job))
    }

    /// 只领取同一 session 之前窗口全部 succeeded 的最小待办窗口（doc/13 §3）。
    pub fn claim_next_ordered_job(&mut self, now: &str) -> Result<Option<JobRow>, StoreError> {
        // 找所有有未完成前窗的 session（存在比某 queued job 更早的非 succeeded 窗口）。
        let blocked: std::collections::HashSet<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT DISTINCT a.tenant_id, a.user_id, a.host_id, a.session_id
                 FROM extraction_jobs a
                 WHERE a.status IN ('queued','running','retryable_failed')
                   AND EXISTS (
                     SELECT 1 FROM extraction_jobs b
                     WHERE b.tenant_id=a.tenant_id AND b.user_id=a.user_id
                       AND b.host_id=a.host_id AND b.session_id=a.session_id
                       AND b.status IN ('dead','running')
                       AND b.created_at <= a.created_at AND b.id != a.id
                   )",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(format!("{}/{}/{}/{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
            })?;
            rows.collect::<Result<std::collections::HashSet<_>, _>>()?
        };
        let job = self.claim_due_job(now)?;
        let Some(job) = job else { return Ok(None) };
        let key = format!("{}/{}/{}/{}", job.tenant_id, job.user_id, job.host_id, job.session_id);
        if blocked.contains(&key) {
            // 前窗未完成：还原为 queued，本轮跳过（v1 单 worker，下轮再 Claim）。
            self.conn_mut().execute(
                "UPDATE extraction_jobs SET status='queued', lease_until=NULL, updated_at=?1 WHERE id=?2",
                params![now, job.id],
            )?;
            return Ok(None);
        }
        Ok(Some(job))
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
                "SELECT id, tenant_id, user_id, host_id, session_id, window_key, through_event_seq,
                        status, attempts, run_after, created_at, updated_at, prompt_version
                 FROM extraction_jobs WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
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
        })
    }
}

use sha2::Digest;
