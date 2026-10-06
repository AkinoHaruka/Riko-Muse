//! 提取作业生命周期与候选落库（doc/11 §3.7、doc/13 §3/§5、doc4/02）。
//!
//! 窗口次序与公平调度（doc4/02 §3）：每轮在单个 SQLite 事务内先条件化恢复 lease 过期的
//! running，再原子领取一条"没有未完成前窗"的 due 作业；同一 session 的前窗受阻只阻断
//! 该 session，不阻塞其他 session。前驱与下界一律按 `through_event_seq` 数值比较，
//! 不用 window_key 字符串序。claim_generation 在每次领取/恢复时原子递增，用于隔离
//! 失去所有权的旧执行者。

use memory_domain::{
    claim_sha256, fold_whitespace, normalize_v1, DomainScope, MemoryKind, Origin, ScopeKey,
};
use memory_extract::{Admission, WindowEvent};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

#[derive(Debug)]
pub enum FlushOutcome {
    /// 新范围内没有任何可提取内容：只生成/返回零模型调用 checkpoint。
    NothingToExtract {
        job_id: String,
    },
    Created {
        job_id: String,
        status: String,
    },
    Existing {
        job_id: String,
        status: String,
    },
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
    /// 生成该作业时的准入规则版本（doc5/03 §1，迁移 0004）；与 prompt_version 分工：
    /// worker 按两列分别分派提示词与 Rust 准入规则，未知版本显式失败。
    pub admission_version: String,
    /// 当前执行权代际：每次领取/过期恢复原子 +1；旧代际的提交一律失效（doc4/02 §4—5）。
    pub claim_generation: i64,
    /// V2-S1：作业归属记忆域；worker 按此构造 DomainScope（doc7/04 §2.4）。
    pub domain_id: String,
}

/// 候选落库结果（审计/诊断用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateOutcome {
    Active { memory_id: String },
    Held { reason: &'static str },
    Rejected { reason: &'static str },
}

impl Store {
    /// POST /v1/extraction/flush（doc/12 §4、doc4/03 §2）。服务端分窗为权威：
    ///
    /// - 同 (scope,host,session,through) 已有作业 → 原样返回（先于乱序检查，幂等）；
    /// - `through` 越过已收最大 seq 或小于已排最大 through → 409；
    /// - 读取 `(已排最大 through, 请求 through]` 的事件，按**实际序列化字节**
    ///   （与 worker 共用 memory_extract builder）贪心分组，超 100 事件或 32 KiB
    ///   即在上一事件 seq 处封闭窗口；
    /// - 单事件自身超限 → 单独 `dead/WINDOW_TOO_LARGE` 作业（attempts=0，不调用模型）；
    /// - 无 user/user 事件的组与空范围（seq 空洞）→ `succeeded` 零模型调用 checkpoint
    ///   推进下界；
    /// - 全部作业与审计在同一事务内提交，返回最后作业的 ID/状态。
    pub fn flush_window(
        &mut self,
        scope: &ScopeKey,
        host_id: &str,
        session_id: &str,
        through_event_seq: i64,
        dom: &DomainScope,
    ) -> Result<FlushOutcome, StoreError> {
        struct PendingWindow {
            through: i64,
            oversized: bool,
            has_user: bool,
        }
        let tx = self.conn_mut().transaction()?;
        // 幂等（doc4/03 §2：乱序检查前）：同 through 的既有作业按原 ID/状态返回。
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT id, status FROM extraction_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4
                   AND through_event_seq=?5",
                params![
                    scope.tenant_id,
                    scope.user_id,
                    host_id,
                    session_id,
                    through_event_seq
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((job_id, status)) = existing {
            tx.commit()?;
            return Ok(FlushOutcome::Existing { job_id, status });
        }
        // through 越过已收到最大 seq → 409（旧协议；空 session 同样拒绝）。
        let max_seq: Option<i64> = tx.query_row(
            "SELECT MAX(event_seq) FROM evidence_events
             WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4",
            params![scope.tenant_id, scope.user_id, host_id, session_id],
            |r| r.get::<_, Option<i64>>(0),
        )?;
        if through_event_seq > max_seq.unwrap_or(-1) {
            return Err(StoreError::StateConflict);
        }
        let max_scheduled: Option<i64> = tx.query_row(
            "SELECT MAX(through_event_seq) FROM extraction_jobs
             WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4",
            params![scope.tenant_id, scope.user_id, host_id, session_id],
            |r| r.get::<_, Option<i64>>(0),
        )?;
        if let Some(m) = max_scheduled {
            if through_event_seq < m {
                return Err(StoreError::StateConflict);
            }
        }
        let last = max_scheduled.unwrap_or(-1);
        // 事务内重新读取范围事件（doc4/03 §2：避免旧读引发重复窗口）。
        let mut stmt = tx.prepare(
            "SELECT id, role, source_kind, occurred_at, content, event_seq FROM evidence_events
             WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4
               AND event_seq>?5 AND event_seq<=?6
             ORDER BY event_seq",
        )?;
        let events: Vec<(i64, memory_extract::WindowEvent)> = stmt
            .query_map(
                params![
                    scope.tenant_id,
                    scope.user_id,
                    host_id,
                    session_id,
                    last,
                    through_event_seq
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(5)?,
                        memory_extract::WindowEvent {
                            id: r.get(0)?,
                            role: r.get(1)?,
                            source_kind: r.get(2)?,
                            occurred_at: r.get(3)?,
                            content: r.get(4)?,
                        },
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        // V2-S1（doc7/04 §2.2）：窗口内事件映射域必须与写域一致（无映射按 user_main）；
        // 提取窗口不得横跨未经授权的域。
        let domain_mismatch: i64 = tx.query_row(
            "SELECT COUNT(*) FROM evidence_events e
             WHERE e.tenant_id=?1 AND e.user_id=?2 AND e.host_id=?3 AND e.session_id=?4
               AND e.event_seq>?5 AND e.event_seq<=?6
               AND COALESCE((SELECT d.domain_id FROM evidence_domain_map d
                             WHERE d.tenant_id=e.tenant_id AND d.user_id=e.user_id
                               AND d.evidence_id=e.id), 'user_main') != ?7",
            params![
                scope.tenant_id,
                scope.user_id,
                host_id,
                session_id,
                last,
                through_event_seq,
                dom.write
            ],
            |r| r.get(0),
        )?;
        if domain_mismatch > 0 {
            return Err(StoreError::StateConflict);
        }

        // 贪心分组：按 seq 递增；加入下一事件会超 100 事件或 32 KiB 时先封闭当前组。
        let mut pending: Vec<PendingWindow> = Vec::new();
        let mut cur_count = 0usize;
        let mut cur_bytes = 0usize;
        let mut cur_last_seq: Option<i64> = None;
        let mut cur_has_user = false;
        for (seq, ev) in &events {
            let size = memory_extract::serialized_event_size(ev);
            let is_user = ev.role == "user" && ev.source_kind == "user";
            if size > memory_contract::EXTRACTION_INPUT_MAX_BYTES {
                // 单事件超限：先封闭已有组，再插入该 seq 的 dead/WINDOW_TOO_LARGE 作业。
                if let Some(s) = cur_last_seq.take() {
                    pending.push(PendingWindow {
                        through: s,
                        oversized: false,
                        has_user: cur_has_user,
                    });
                }
                pending.push(PendingWindow {
                    through: *seq,
                    oversized: true,
                    has_user: false,
                });
                cur_count = 0;
                cur_bytes = 0;
                cur_has_user = false;
            } else if cur_count + 1 > memory_contract::EXTRACTION_WINDOW_MAX_EVENTS
                || cur_bytes + size > memory_contract::EXTRACTION_INPUT_MAX_BYTES
            {
                let s = cur_last_seq.take().expect("组非空才会触发分窗");
                pending.push(PendingWindow {
                    through: s,
                    oversized: false,
                    has_user: cur_has_user,
                });
                cur_count = 1;
                cur_bytes = size;
                cur_last_seq = Some(*seq);
                cur_has_user = is_user;
            } else {
                cur_count += 1;
                cur_bytes += size;
                cur_last_seq = Some(*seq);
                cur_has_user |= is_user;
            }
        }
        // 末组非空且尚无 requested_through 的作业：封闭末组，through=requested_through。
        if cur_last_seq.is_some() {
            pending.push(PendingWindow {
                through: through_event_seq,
                oversized: false,
                has_user: cur_has_user,
            });
        }

        let now = now_rfc3339()?;
        let insert_job = |tx: &rusqlite::Transaction<'_>,
                          through: i64,
                          status: &str,
                          error_code: Option<&str>|
         -> Result<String, StoreError> {
            let job_id = Uuid::now_v7().to_string();
            tx.execute(
                "INSERT INTO extraction_jobs
                 (id, tenant_id, user_id, host_id, session_id, window_key, through_event_seq,
                  status, attempts, run_after, created_at, updated_at, prompt_version,
                  admission_version, error_code, domain_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?10,?11,?12,?13,?14,?15)",
                params![
                    job_id,
                    scope.tenant_id,
                    scope.user_id,
                    host_id,
                    session_id,
                    format!("v1:{through}"),
                    through,
                    status,
                    now,
                    now,
                    now,
                    memory_contract::EXTRACT_PROMPT_VERSION,
                    memory_contract::ADMISSION_VERSION,
                    error_code,
                    dom.write
                ],
            )?;
            Ok(job_id)
        };
        let mut last_job: Option<(String, String)> = None;
        for w in &pending {
            let (status, error_code) = if w.oversized {
                ("dead", Some("WINDOW_TOO_LARGE"))
            } else if !w.has_user {
                // 无 user/user 的组：直接 succeeded，不调用模型（doc4/03 §2）。
                ("succeeded", None)
            } else {
                ("queued", None)
            };
            let job_id = insert_job(&tx, w.through, status, error_code)?;
            last_job = Some((job_id, status.to_string()));
        }
        if pending.is_empty() {
            // 空范围（如 through 落在 seq 空洞）：succeeded 空 checkpoint 推进下界，
            // 不捏造事件（doc4/03 §2）。
            let job_id = insert_job(&tx, through_event_seq, "succeeded", None)?;
            last_job = Some((job_id, "succeeded".into()));
        }
        // 审计与作业同一事务；audit_events 无 CHECK 约束，actor_kind 按来源如实记录。
        let (last_id, last_status) = last_job.expect("至少生成 checkpoint");
        let detail = serde_json::json!({
            "host_id": host_id, "session_id": session_id,
            "through_event_seq": through_event_seq, "jobs": pending.len().max(1),
        });
        tx.execute(
            "INSERT INTO audit_events
             (id, tenant_id, user_id, actor_kind, actor_id, action, target_id, occurred_at, detail_json)
             VALUES (?1,?2,?3,'system','flush','extraction_flush',?4,?5,?6)",
            params![
                Uuid::now_v7().to_string(), scope.tenant_id, scope.user_id,
                last_id, now, detail.to_string()
            ],
        )?;
        tx.commit()?;
        let actionable = pending.iter().any(|w| w.oversized || w.has_user);
        if !actionable {
            return Ok(FlushOutcome::NothingToExtract { job_id: last_id });
        }
        Ok(FlushOutcome::Created {
            job_id: last_id,
            status: last_status,
        })
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
                params![
                    scope.tenant_id,
                    scope.user_id,
                    host_id,
                    session_id,
                    through_event_seq
                ],
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
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i32>(2)?,
                ))
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
            params![
                scope.tenant_id,
                scope.user_id,
                job.host_id,
                job.session_id,
                lower_bound,
                job.through_event_seq
            ],
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
        // 窗口上限（doc/13 §3、doc4/03）：100 事件 / 32 KiB（按与 worker 共用的
        // 实际序列化字节计），超限不悄悄截断。返回 WindowTooLarge：同输入重试不会
        // 改变，worker 侧确定性 dead，不空转重试。服务端分窗正常时不应触发。
        if events.len() > memory_contract::EXTRACTION_WINDOW_MAX_EVENTS {
            return Err(StoreError::WindowTooLarge);
        }
        let total: usize = events
            .iter()
            .map(memory_extract::serialized_event_size)
            .sum();
        if total > memory_contract::EXTRACTION_INPUT_MAX_BYTES {
            return Err(StoreError::WindowTooLarge);
        }
        Ok(events)
    }

    /// 心跳续租（doc4/02 §4）：仅当前代际的 running 可续；返回 false 表示失去
    /// 所有权，worker 应停止心跳并忽略后续模型结果。只做一次条件 UPDATE，
    /// 短暂持有 DB 锁；模型网络调用绝不持锁。
    pub fn renew_job_lease(&mut self, job_id: &str, generation: i64) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let lease = (chrono::Utc::now()
            + chrono::Duration::seconds(memory_contract::JOB_LEASE_SECS as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let n = self.conn_mut().execute(
            "UPDATE extraction_jobs SET lease_until=?1, updated_at=?2
             WHERE id=?3 AND status='running' AND claim_generation=?4",
            params![lease, now, job_id, generation],
        )?;
        Ok(n > 0)
    }

    /// 快查：作业是否仍处于指定代际的 running（只读；事务内校验由 save_candidate 承担）。
    pub fn job_generation_current(
        &self,
        job_id: &str,
        generation: i64,
    ) -> Result<bool, StoreError> {
        let ok: bool = self
            .conn()
            .query_row(
                "SELECT 1 FROM extraction_jobs
                 WHERE id=?1 AND status='running' AND claim_generation=?2",
                params![job_id, generation],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        Ok(ok)
    }

    /// doc7/03（D.6）：extract_v4 rewrite 全部条目进审计（quote/claims/notes/reason），
    /// 满足"这条 claim 的主语是哪来的"可追溯；失败仅告警，不阻断候选提交。
    pub fn record_extraction_rewrite_audit(
        &self,
        scope: &ScopeKey,
        job_id: &str,
        detail: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let now = now_rfc3339()?;
        self.conn().execute(
            "INSERT INTO audit_events
             (id, tenant_id, user_id, actor_kind, actor_id, action, target_id, occurred_at, detail_json)
             VALUES (?1,?2,?3,'system','extract-worker','extraction_rewrite',?4,?5,?6)",
            params![
                uuid::Uuid::now_v7().to_string(),
                scope.tenant_id,
                scope.user_id,
                job_id,
                now,
                detail.to_string()
            ],
        )?;
        Ok(())
    }

    /// 完成提交（doc4/02 §2/§5）：generation 匹配才生效；影响 0 行返回
    /// `StaleClaim`——旧执行者不得覆盖新执行者，也不能假装成功。
    pub fn complete_job(
        &mut self,
        job_id: &str,
        generation: i64,
        attempts: i32,
        model_name: &str,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
    ) -> Result<(), StoreError> {
        let now = now_rfc3339()?;
        let n = self.conn_mut().execute(
            "UPDATE extraction_jobs SET status='succeeded', attempts=?1, model_name=?2,
             input_tokens=?3, output_tokens=?4, lease_until=NULL, updated_at=?5
             WHERE id=?6 AND status='running' AND claim_generation=?7",
            params![
                attempts,
                model_name,
                input_tokens,
                output_tokens,
                now,
                job_id,
                generation
            ],
        )?;
        if n == 0 {
            return Err(StoreError::StaleClaim);
        }
        Ok(())
    }

    /// 失败落状态（doc4/02 §2/§5）：generation 匹配才生效；影响 0 行返回
    /// `StaleClaim`。`attempts` 为本次执行结束后的累计次数（领取时 attempts+1）。
    pub fn fail_job(
        &mut self,
        job_id: &str,
        generation: i64,
        attempts: i32,
        error_code: &str,
    ) -> Result<FailOutcome, StoreError> {
        let max = memory_contract::JOB_MAX_ATTEMPTS as i32;
        let now_dt = chrono::Utc::now();
        let now = now_dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        if attempts >= max {
            let n = self.conn_mut().execute(
                "UPDATE extraction_jobs SET status='dead', attempts=?1, error_code=?2,
                 lease_until=NULL, updated_at=?3
                 WHERE id=?4 AND status='running' AND claim_generation=?5",
                params![attempts, error_code, now, job_id, generation],
            )?;
            if n == 0 {
                return Err(StoreError::StaleClaim);
            }
            return Ok(FailOutcome::Dead);
        }
        let delay = memory_contract::JOB_RETRY_DELAYS_SECS
            .get(attempts as usize - 1)
            .copied()
            .unwrap_or(45);
        let run_after = (now_dt + chrono::Duration::seconds(delay as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let n = self.conn_mut().execute(
            "UPDATE extraction_jobs SET status='retryable_failed', attempts=?1, error_code=?2,
             run_after=?3, lease_until=NULL, updated_at=?4
             WHERE id=?5 AND status='running' AND claim_generation=?6",
            params![attempts, error_code, run_after, now, job_id, generation],
        )?;
        if n == 0 {
            return Err(StoreError::StaleClaim);
        }
        Ok(FailOutcome::Retryable { run_after })
    }

    /// 确定性失败立即 dead（doc4/02 §2 状态机、doc4/03 §5）：`WINDOW_TOO_LARGE`、
    /// `UNKNOWN_PROMPT_VERSION` 等同输入重试不会改变的错误不进退避阶梯——不空转
    /// 重试、不延迟 dead、attempts 记本次执行结束后的实际次数。generation 匹配才
    /// 生效；影响 0 行返回 `StaleClaim`。
    pub fn fail_job_deterministic(
        &mut self,
        job_id: &str,
        generation: i64,
        attempts: i32,
        error_code: &str,
    ) -> Result<(), StoreError> {
        let now = now_rfc3339()?;
        let n = self.conn_mut().execute(
            "UPDATE extraction_jobs SET status='dead', attempts=?1, error_code=?2,
             lease_until=NULL, updated_at=?3
             WHERE id=?4 AND status='running' AND claim_generation=?5",
            params![attempts, error_code, now, job_id, generation],
        )?;
        if n == 0 {
            return Err(StoreError::StaleClaim);
        }
        Ok(())
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

    /// 本地管理员显式跳过（doc4/03 §4）：仅 `dead + WINDOW_TOO_LARGE` 作业可跳。
    /// 写 `extraction_job_skips` 与 audit_events（actor_kind='admin_cli'、
    /// actor_id='local_admin'）；重复相同 skip 幂等返回既有记录。跳过只表示
    /// 跳过该窗口的自动提取，L0 原文保留。其他 dead 原因、其他 scope 或缺失 ID
    /// 不变更任何行。
    pub fn skip_dead_job(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        reason: &str,
    ) -> Result<bool, StoreError> {
        let tx = self.conn_mut().transaction()?;
        let job: Option<(String, String)> = tx
            .query_row(
                "SELECT status, COALESCE(error_code,'') FROM extraction_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, job_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match job {
            None => return Err(StoreError::JobNotFound),
            Some((status, error_code)) => {
                if status != "dead" || error_code != "WINDOW_TOO_LARGE" {
                    return Err(StoreError::StateConflict);
                }
            }
        }
        let already: bool = tx
            .query_row(
                "SELECT 1 FROM extraction_job_skips WHERE job_id=?1",
                params![job_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if already {
            tx.commit()?;
            return Ok(false);
        }
        let now = now_rfc3339()?;
        tx.execute(
            "INSERT INTO extraction_job_skips
             (job_id, tenant_id, user_id, reason_code, actor_kind, actor_id, created_at)
             VALUES (?1,?2,?3,?4,'admin_cli','local_admin',?5)",
            params![job_id, scope.tenant_id, scope.user_id, reason, now],
        )?;
        tx.execute(
            "INSERT INTO audit_events
             (id, tenant_id, user_id, actor_kind, actor_id, action, target_id, occurred_at, detail_json)
             VALUES (?1,?2,?3,'admin_cli','local_admin','job_skip',?4,?5,?6)",
            params![
                Uuid::now_v7().to_string(), scope.tenant_id, scope.user_id,
                job_id, now,
                serde_json::json!({"reason": reason, "error_code": "WINDOW_TOO_LARGE"}).to_string()
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// 诊断：统计指定 reason 的候选数（不返回正文）。
    pub fn count_candidates_by_reason(
        &self,
        scope: &ScopeKey,
        reason: &str,
    ) -> Result<i64, StoreError> {
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
        // V2-S1：候选与派生记忆归属作业域；准入查库按写域过滤（doc7/04 §2.4/§3）。
        let dom = DomainScope::for_job(&job.domain_id);
        let quote = fold_whitespace(&c.quote);
        // doc7/03：extract_v4 rewrite 产出的命题成为记忆正文；quote 保持逐字原文。
        let claim_text = fold_whitespace(c.claim.as_deref().unwrap_or(&c.quote));
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
            return Ok(CandidateOutcome::Rejected {
                reason: "BAD_SOURCE",
            });
        }
        let (start, end) = match memory_domain::find_quote_span(&ev_content, &c.quote) {
            Some(v) => v,
            None => {
                return Ok(CandidateOutcome::Rejected {
                    reason: "QUOTE_MISMATCH",
                })
            }
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
                params![
                    scope.tenant_id,
                    scope.user_id,
                    job.id,
                    c.source_event_id,
                    kind.as_str(),
                    quote_hash
                ],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if dup {
            return Ok(CandidateOutcome::Rejected {
                reason: "DUPLICATE_CANDIDATE",
            });
        }
        let tx = self.conn_mut().transaction()?;
        // 事务内核对作业仍为当前代际的 running（doc4/02 §5）——仅函数入口检查不够；
        // 校验失败在此返回，事务不写任何行。
        let (job_status, job_gen): (String, i64) = tx.query_row(
            "SELECT status, claim_generation FROM extraction_jobs
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            params![scope.tenant_id, scope.user_id, job.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if job_status != "running" || job_gen != job.claim_generation {
            return Err(StoreError::StaleClaim);
        }
        tx.execute(
            "INSERT INTO memory_candidates
             (id, tenant_id, user_id, job_id, primary_evidence_id, kind, quote, quote_sha256, claim,
              source_class, status, reason_code, model_confidence, occurred_at, valid_until, created_at, domain_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                candidate_id,
                scope.tenant_id,
                scope.user_id,
                job.id,
                c.source_event_id,
                kind.as_str(),
                c.quote,
                quote_hash,
                claim_text,
                source_class,
                status,
                reason_code,
                c.confidence,
                c.occurred_at,
                c.valid_until,
                now,
                dom.write
            ],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO candidate_evidence (id, tenant_id, user_id, candidate_id, evidence_id, start_byte, end_byte)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![Uuid::now_v7().to_string(), scope.tenant_id, scope.user_id, candidate_id, c.source_event_id, start as i64, end as i64],
        )?;

        // Held 准入的行已是 held；返回标签同样如实标 Held（此前误标 Rejected，仅诊断输出受影响）。
        let mut outcome = match admission {
            Admission::Held(r) => CandidateOutcome::Held { reason: r },
            _ => CandidateOutcome::Rejected {
                reason: reason_code.unwrap_or("REJECTED"),
            },
        };
        if admission == Admission::Active {
            let claim_hash = claim_sha256(kind, &claim_text);
            // 规则 8a：同 kind+hash 的 active → 仅加证据。
            let existing_active: Option<(String, i64)> = tx
                .query_row(
                    "SELECT id, version FROM memories
                     WHERE tenant_id=?1 AND user_id=?2 AND kind=?3 AND claim_sha256=?4 AND status='active'
                       AND domain_id=?5",
                    params![scope.tenant_id, scope.user_id, kind.as_str(), claim_hash, dom.write],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((memory_id, _version)) = existing_active {
                tx.execute(
                    "INSERT OR IGNORE INTO memory_evidence (id, tenant_id, user_id, memory_id, evidence_id, start_byte, end_byte)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![Uuid::now_v7().to_string(), scope.tenant_id, scope.user_id, memory_id, c.source_event_id, start as i64, end as i64],
                )?;
                outcome = CandidateOutcome::Rejected {
                    reason: "DUPLICATE_ACTIVE",
                };
            } else {
                // 规则 8b：同旧证据+hash 的 forgotten → 抑制源，不复活。
                let suppressed: bool = tx
                    .query_row(
                        "SELECT 1 FROM suppressed_sources
                         WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?5 AND evidence_id=?3 AND claim_sha256=?4 LIMIT 1",
                        params![scope.tenant_id, scope.user_id, c.source_event_id, claim_hash, dom.write],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);
                if suppressed {
                    outcome = CandidateOutcome::Rejected {
                        reason: "SUPPRESSED_SOURCE",
                    };
                } else {
                    // 规则 9：属性键相同而值不同的 active → held:POSSIBLE_CONFLICT。
                    if let Some(conflict_id) =
                        Self::attribute_conflict(&tx, scope, kind, &claim_text, &dom)?
                    {
                        let _ = conflict_id;
                        tx.execute(
                            "UPDATE memory_candidates SET status='held', reason_code='POSSIBLE_CONFLICT' WHERE id=?1",
                            params![candidate_id],
                        )?;
                        outcome = CandidateOutcome::Held {
                            reason: "POSSIBLE_CONFLICT",
                        };
                    } else {
                        // 通过：建 active memory（与 remember 同一事务模式）。
                        let memory_id = Uuid::now_v7().to_string();
                        tx.execute(
                            "INSERT INTO memories
                             (id, tenant_id, user_id, kind, claim, normalized_claim, claim_sha256, source_class,
                              status, version, occurred_at, valid_from, valid_until, origin_host_id, origin_agent_id,
                              created_at, updated_at, domain_id)
                             VALUES (?1,?2,?3,?4,?5,?6,?7,'user_explicit','active',1,NULL,NULL,NULL,?8,?9,?10,?11,?12)",
                            params![
                                memory_id, scope.tenant_id, scope.user_id, kind.as_str(),
                                claim_text, normalize_v1(&claim_text), claim_hash,
                                origin.host_id, origin.agent_id, now, now, dom.write
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
                             VALUES (?1,?2,?3,1,NULL,?4,NULL,'active','system',?5,?6,?7)",
                            params![
                                scope.tenant_id, scope.user_id, memory_id, claim_text, job.id,
                                format!("{}:{}", job.prompt_version, job.admission_version),
                                now
                            ],
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
        // 索引事务（失败保留 dirty）。doc7/03：索引内容跟随记忆正文（改写后的
        // claim_text），quote 已存 evidence 锚，不再作为 FTS/grams 的检索文本。
        if let CandidateOutcome::Active { memory_id } = &outcome {
            if let Err(e) = self.reindex_memory(scope, memory_id, &claim_text, true) {
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
        dom: &DomainScope,
    ) -> Result<Option<String>, StoreError> {
        let new_key = match extract_attr_key(quote, kind) {
            Some(k) => k,
            None => return Ok(None),
        };
        let mut stmt = conn.prepare(
            "SELECT id, claim FROM memories
             WHERE tenant_id=?1 AND user_id=?2 AND kind=?3 AND status='active' AND domain_id=?4",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, kind.as_str(), dom.write],
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

/// 属性键提取（NFKC+大小写折叠后的固定前后缀，doc/13 §5.9；doc5/03 §7 扩展）。
/// 扩展键 occupation 同时识别「我在X工作」与「我在X做<职业尾词>」（复用
/// memory_extract::explicit_shape，不跨层复制名单）；primary_practice 识别
/// 「我主要写X/我平时主要写X」。旧前缀逻辑保留，不削弱历史 active 的冲突检测。
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
    if kind == MemoryKind::Fact {
        match memory_extract::explicit_shape(quote) {
            Some(memory_extract::ExplicitShape::FactOccupation) => return Some("occupation"),
            Some(memory_extract::ExplicitShape::FactPrimaryPractice) => {
                return Some("primary_practice")
            }
            _ => {}
        }
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
            admission_version: r.get(13)?,
            claim_generation: r.get(14)?,
            domain_id: r.get(15)?,
        })
    }
}

/// JobRow 查询列（与 job_row_mapper 的列序一一对应）。
const JOB_ROW_COLUMNS: &str = "id, tenant_id, user_id, host_id, session_id, window_key, \
     through_event_seq, status, attempts, run_after, created_at, updated_at, prompt_version, \
     admission_version, claim_generation, domain_id";

use sha2::Digest;

#[cfg(test)]
mod tests {
    //! doc4/02 §6 的确定性检查：固定相对时钟 + 临时 SQLite；不涉及模型调用。
    use super::*;
    use crate::{Store, StoreError};
    use memory_domain::{DomainScope, Origin, ScopeKey};

    fn migrations_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("migrations")
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
        ScopeKey {
            tenant_id: tenant.into(),
            user_id: user.into(),
        }
    }

    fn ingest(store: &mut Store, scope: &ScopeKey, session: &str, seq: i64, content: &str) {
        let t = chrono::Utc::now();
        let origin = Origin {
            host_id: "dsh".into(),
            agent_id: "agent-a".into(),
            session_id: session.into(),
        };
        store
            .record_evidence(
                scope,
                &origin,
                seq,
                "user",
                "user",
                &t,
                content,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap();
    }

    fn flush(store: &mut Store, scope: &ScopeKey, session: &str, through: i64) -> String {
        match store
            .flush_window(
                scope,
                "dsh",
                session,
                through,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            FlushOutcome::Created { job_id, .. } => job_id,
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

    /// 夹具助手：直接布置作业状态，仅用于构造 dead 前驱等"被测行为之外"的初始态；
    /// 被测行为本身（领取/退避/恢复）仍走真实路径。
    fn force_status(store: &mut Store, job_id: &str, status: &str, error_code: &str) {
        store
            .conn_mut()
            .execute(
                "UPDATE extraction_jobs SET status=?1, error_code=?2, lease_until=NULL WHERE id=?3",
                params![status, error_code, job_id],
            )
            .unwrap();
    }

    #[test]
    fn backoff_retryable_requires_run_after_expiry() {
        // doc4/02 §2：retryable_failed 的到期只看 run_after，退避不可被绕过。
        let mut store = setup("backoff");
        let scope = scope_of(&store, "t", "u");
        ingest(&mut store, &scope, "s1", 1, "我叫洛溪");
        let job_id = flush(&mut store, &scope, "s1", 1);
        let run_after0 = job_field(&store, &job_id, "run_after");
        // 真实领取后失败落 retryable（fail 仅在 running + generation 匹配时生效）。
        let claimed = store
            .claim_next_ordered_job(&plus_secs(&run_after0, 1))
            .unwrap()
            .unwrap();
        assert_eq!(claimed.claim_generation, 1);
        assert!(matches!(
            store
                .fail_job(&job_id, claimed.claim_generation, 1, "MODEL_TIMEOUT")
                .unwrap(),
            FailOutcome::Retryable { .. }
        ));
        let run_after = job_field(&store, &job_id, "run_after");

        // 退避未到：不可领取，状态不变、无 lease。
        let claimed = store
            .claim_next_ordered_job(&plus_secs(&run_after, -1))
            .unwrap();
        assert!(claimed.is_none(), "退避期内不得领取");
        assert_eq!(job_field(&store, &job_id, "status"), "retryable_failed");
        assert_eq!(job_field(&store, &job_id, "lease_until"), "<NULL>");

        // 到期：可领取，running + generation 1→2。
        let job = store
            .claim_next_ordered_job(&plus_secs(&run_after, 1))
            .unwrap()
            .unwrap();
        assert_eq!(job.id, job_id);
        assert_eq!(job.status, "running");
        assert_eq!(job.claim_generation, 2);
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
        assert!(store
            .claim_next_ordered_job(&plus_secs(&t0, 91))
            .unwrap()
            .is_none());
        assert_eq!(job_field(&store, &job_id, "status"), "retryable_failed");
        assert_eq!(
            job_field(&store, &job_id, "error_code"),
            "WORKER_LEASE_EXPIRED"
        );
        let (gen, attempts): (i64, i32) = store
            .conn()
            .query_row(
                "SELECT claim_generation, attempts FROM extraction_jobs WHERE id=?1",
                params![job_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((gen, attempts), (2, 1));

        // 第 2 次执行 → 过期恢复：attempts 2。
        let t1 = plus_secs(&job_field(&store, &job_id, "run_after"), 1);
        assert!(store.claim_next_ordered_job(&t1).unwrap().is_some());
        assert!(store
            .claim_next_ordered_job(&plus_secs(&t1, 91))
            .unwrap()
            .is_none());
        let (gen, attempts): (i64, i32) = store
            .conn()
            .query_row(
                "SELECT claim_generation, attempts FROM extraction_jobs WHERE id=?1",
                params![job_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((gen, attempts), (4, 2));

        // 第 3 次执行 → 过期恢复：达上限，dead。
        let t2 = plus_secs(&job_field(&store, &job_id, "run_after"), 1);
        assert!(store.claim_next_ordered_job(&t2).unwrap().is_some());
        assert!(store
            .claim_next_ordered_job(&plus_secs(&t2, 91))
            .unwrap()
            .is_none());
        assert_eq!(job_field(&store, &job_id, "status"), "dead");
        assert_eq!(
            job_field(&store, &job_id, "error_code"),
            "WORKER_LEASE_EXPIRED"
        );
        let attempts: i32 = store
            .conn()
            .query_row(
                "SELECT attempts FROM extraction_jobs WHERE id=?1",
                params![job_id],
                |r| r.get(0),
            )
            .unwrap();
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
        assert!(store
            .claim_next_ordered_job(&plus_secs(&now, 1))
            .unwrap()
            .is_none());
        // 前窗成功后后窗可领。
        store
            .complete_job(&job_a, first.claim_generation, 1, "mock", None, None)
            .unwrap();
        let second = store
            .claim_next_ordered_job(&plus_secs(&now, 2))
            .unwrap()
            .unwrap();
        assert_eq!(second.through_event_seq, 10);
    }

    #[test]
    fn blocked_session_does_not_starve_other_sessions() {
        // doc4/02 §3：s1 前窗 dead 未跳过 → s1 后窗被排除，但 s2 照常领取。
        let mut store = setup("fair");
        let scope = scope_of(&store, "t", "u");
        ingest(&mut store, &scope, "s1", 1, "s1-事件1");
        let dead_job = flush(&mut store, &scope, "s1", 1);
        force_status(&mut store, &dead_job, "dead", "MODEL_TIMEOUT");
        ingest(&mut store, &scope, "s1", 2, "s1-事件2");
        let blocked_job = flush(&mut store, &scope, "s1", 2);
        ingest(&mut store, &scope, "s2", 1, "s2-事件1");
        let other_job = flush(&mut store, &scope, "s2", 1);

        let now = plus_secs(&job_field(&store, &other_job, "run_after"), 1);
        let claimed = store.claim_next_ordered_job(&now).unwrap().unwrap();
        assert_eq!(
            claimed.session_id, "s2",
            "受阻 session 被跳过，其他 session 前进"
        );
        store
            .complete_job(&other_job, claimed.claim_generation, 1, "mock", None, None)
            .unwrap();

        // 只剩受阻作业：返回 None 且不反复取出、不写 lease。
        assert!(store
            .claim_next_ordered_job(&plus_secs(&now, 2))
            .unwrap()
            .is_none());
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
        store
            .principal_add("t", "u2", &dir.join("u2.token"))
            .unwrap();

        let scope1 = scope_of(&store, "t", "u");
        let scope2 = scope_of(&store, "t", "u2");
        ingest(&mut store, &scope1, "s1", 1, "u1-事件");
        let dead_job = flush(&mut store, &scope1, "s1", 1);
        force_status(&mut store, &dead_job, "dead", "MODEL_TIMEOUT");
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
        assert_eq!(
            store.window_lower_bound(&scope, "dsh", "s1", 99).unwrap(),
            -1
        );
        // 真实领取并成功，使 through 99 成为已越过前窗。
        let run_after = job_field(&store, &job_99, "run_after");
        let claimed = store
            .claim_next_ordered_job(&plus_secs(&run_after, 1))
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, job_99);
        store
            .complete_job(&job_99, claimed.claim_generation, 1, "mock", None, None)
            .unwrap();
        let _job_100 = flush(&mut store, &scope, "s1", 100);
        assert_eq!(
            store.window_lower_bound(&scope, "dsh", "s1", 100).unwrap(),
            99,
            "下界必须是 99，不得回退到 -1 或更早窗口"
        );

        // 未跳过的 dead 不计入下界；succeeded 计入。
        ingest(&mut store, &scope, "s2", 1, "s2-事件1");
        let dead_job = flush(&mut store, &scope, "s2", 1);
        force_status(&mut store, &dead_job, "dead", "MODEL_TIMEOUT");
        ingest(&mut store, &scope, "s2", 2, "s2-事件2");
        let _job_2 = flush(&mut store, &scope, "s2", 2);
        assert_eq!(
            store.window_lower_bound(&scope, "dsh", "s2", 2).unwrap(),
            -1,
            "未跳过的 dead 前窗不推进下界"
        );
    }

    #[test]
    fn flush_splits_101_events_and_replay_is_idempotent() {
        // doc4/03 §6：101 个短事件一次 flush → 至少两个递增窗口，均不超 100 事件；
        // 重放同一 flush 不产生新窗口。
        let mut store = setup("split101");
        let scope = scope_of(&store, "t", "u");
        for seq in 1..=101 {
            ingest(&mut store, &scope, "s1", seq, &format!("事件{seq}"));
        }
        let outcome = store
            .flush_window(
                &scope,
                "dsh",
                "s1",
                101,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap();
        let last_id = match &outcome {
            FlushOutcome::Created { job_id, status } => {
                assert_eq!(status, "queued", "最后作业应 queued");
                job_id.clone()
            }
            other => panic!("应 Created，实际 {other:?}"),
        };
        let (jobs, max_events): (i64, i64) = store
            .conn()
            .query_row(
                "SELECT count(*), MAX(through_event_seq) FROM extraction_jobs
             WHERE tenant_id='t' AND user_id='u' AND host_id='dsh' AND session_id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(jobs >= 2, "101 事件至少两个窗口，实际 {jobs}");
        assert_eq!(max_events, 101, "最后窗口 through = 请求 through");
        // 逐窗按序领取、加载（真实流程：前窗成功后下界推进），事件数 ≤ 100。
        let rows: Vec<(String, i64)> = {
            let mut stmt = store
                .conn()
                .prepare(
                    "SELECT id, through_event_seq FROM extraction_jobs
                 WHERE tenant_id='t' AND user_id='u' AND host_id='dsh' AND session_id='s1'
                 ORDER BY through_event_seq",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        for (id, through) in &rows {
            let now = plus_secs(&now_rfc3339().unwrap(), *through + 1);
            let claimed = store.claim_next_ordered_job(&now).unwrap().unwrap();
            assert_eq!(claimed.id, *id, "窗口必须按 through 顺序领取");
            let lower = store
                .window_lower_bound(&scope, "dsh", "s1", *through)
                .unwrap();
            let events = store.load_window_events(&scope, &claimed, lower).unwrap();
            assert!(
                events.len() <= memory_contract::EXTRACTION_WINDOW_MAX_EVENTS,
                "窗口 {through} 事件数 {} 超限",
                events.len()
            );
            store
                .complete_job(id, claimed.claim_generation, 1, "mock", None, None)
                .unwrap();
        }
        assert_eq!(
            rows.last().unwrap().0,
            last_id,
            "最后作业 ID = 请求 through 对应作业"
        );
        // 重放：同 ID/状态返回，不新增窗口。
        match store
            .flush_window(
                &scope,
                "dsh",
                "s1",
                101,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            FlushOutcome::Existing { job_id, .. } => assert_eq!(job_id, last_id),
            other => panic!("重放应 Existing，实际 {other:?}"),
        }
        let jobs2: i64 = store
            .conn()
            .query_row(
                "SELECT count(*) FROM extraction_jobs
             WHERE tenant_id='t' AND user_id='u' AND host_id='dsh' AND session_id='s1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(jobs2, jobs, "重放不得产生新窗口");
    }

    #[test]
    fn flush_budget_uses_serialized_bytes_not_raw_content() {
        // doc4/03 §1—2：预算按实际 JSON 输入字节。10 000 个引号字符原始 10 KB，
        // JSON 转义后约 20 KB；两条原始合计 20 KB < 32 KiB，转义后 > 32 KiB → 必分窗。
        let mut store = setup("bytes");
        let scope = scope_of(&store, "t", "u");
        let quoted = "\"".repeat(10_000);
        ingest(&mut store, &scope, "s1", 1, &quoted);
        ingest(&mut store, &scope, "s1", 2, &quoted);
        store
            .flush_window(
                &scope,
                "dsh",
                "s1",
                2,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap();
        let (jobs, statuses): (i64, String) = store
            .conn()
            .query_row(
                "SELECT count(*), group_concat(status) FROM extraction_jobs
             WHERE tenant_id='t' AND user_id='u' AND host_id='dsh' AND session_id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            jobs, 2,
            "按序列化字节应分两窗（原始 content.len() 合计仅 20 KB）"
        );
        assert_eq!(statuses, "queued,queued");
        // 逐窗按序领取、加载：实际序列化输入不超 32 KiB，L0 原文完整。
        let rows: Vec<(String, i64)> = {
            let mut stmt = store
                .conn()
                .prepare(
                    "SELECT id, through_event_seq FROM extraction_jobs
                 WHERE tenant_id='t' AND user_id='u' AND host_id='dsh' AND session_id='s1'
                 ORDER BY through_event_seq",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        for (id, through) in &rows {
            let now = plus_secs(&now_rfc3339().unwrap(), *through + 1);
            let claimed = store.claim_next_ordered_job(&now).unwrap().unwrap();
            assert_eq!(claimed.id, *id);
            let lower = store
                .window_lower_bound(&scope, "dsh", "s1", *through)
                .unwrap();
            let events = store.load_window_events(&scope, &claimed, lower).unwrap();
            let input = memory_extract::serialize_window_events(&events).unwrap();
            assert!(
                input.len() <= memory_contract::EXTRACTION_INPUT_MAX_BYTES,
                "窗口 {through} 序列化 {} 超限",
                input.len()
            );
            assert!(
                events.iter().all(|e| e.content == quoted),
                "L0 原文不得截断"
            );
            store
                .complete_job(id, claimed.claim_generation, 1, "mock", None, None)
                .unwrap();
        }
    }

    #[test]
    fn flush_oversized_event_dead_then_skip_unblocks() {
        // doc4/03 §3/§4：单条约 40 KiB 事件 → L0 保留；dead/WINDOW_TOO_LARGE、
        // attempts=0；skip 前后窗被阻断，skip 后可领；重复 skip 幂等。
        let mut store = setup("oversize");
        let scope = scope_of(&store, "t", "u");
        let big = "是".repeat(13_500); // 序列化（含 JSON 结构）> 32 KiB
        ingest(&mut store, &scope, "s1", 1, &big);
        ingest(&mut store, &scope, "s1", 2, "正常事件2");
        ingest(&mut store, &scope, "s1", 3, "正常事件3");
        store
            .flush_window(
                &scope,
                "dsh",
                "s1",
                3,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap();
        // L0 保留。
        let ev1: String = store
            .conn()
            .query_row(
                "SELECT content FROM evidence_events WHERE tenant_id='t' AND user_id='u'
             AND host_id='dsh' AND session_id='s1' AND event_seq=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ev1.len(), big.len(), "超限事件 L0 原文不删不改");
        // dead/WINDOW_TOO_LARGE 作业存在，attempts=0；后窗 queued。
        let (dead_id, attempts): (String, i32) = store
            .conn()
            .query_row(
                "SELECT id, attempts FROM extraction_jobs
             WHERE tenant_id='t' AND user_id='u' AND host_id='dsh' AND session_id='s1'
               AND status='dead' AND error_code='WINDOW_TOO_LARGE'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(attempts, 0);
        let queued: String = store.conn().query_row(
            "SELECT id FROM extraction_jobs
             WHERE tenant_id='t' AND user_id='u' AND host_id='dsh' AND session_id='s1' AND status='queued'",
            [], |r| r.get(0),
        ).unwrap();
        // skip 前：后窗被 dead 前窗阻断。
        assert!(store
            .claim_next_ordered_job(&plus_secs(&now_rfc3339().unwrap(), 1))
            .unwrap()
            .is_none());
        // 非 WINDOW_TOO_LARGE 的 dead 拒绝 skip；未变更任何行。
        ingest(&mut store, &scope, "s2", 1, "s2-事件1");
        let other = flush(&mut store, &scope, "s2", 1);
        force_status(&mut store, &other, "dead", "MODEL_TIMEOUT");
        assert!(matches!(
            store.skip_dead_job(&scope, &other, "测试"),
            Err(StoreError::StateConflict)
        ));
        assert_eq!(
            job_field(&store, &other, "status"),
            "dead",
            "被拒 skip 不得改变状态"
        );
        // 正式 skip：后窗可领；重复 skip 幂等。
        assert!(store
            .skip_dead_job(&scope, &dead_id, "运维确认单事件超限")
            .unwrap());
        assert!(
            !store.skip_dead_job(&scope, &dead_id, "重复请求").unwrap(),
            "重复 skip 幂等"
        );
        let claimed = store
            .claim_next_ordered_job(&plus_secs(&now_rfc3339().unwrap(), 2))
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, queued, "skip 后同 session 后窗可领");
        // 跨 scope skip：报 JobNotFound，不写行。
        let scope2 = ScopeKey {
            tenant_id: "t".into(),
            user_id: "u2".into(),
        };
        assert!(matches!(
            store.skip_dead_job(&scope2, &dead_id, "越权"),
            Err(StoreError::JobNotFound)
        ));
        let skips: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM extraction_job_skips", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(skips, 1, "跨 scope 请求不得新增 skip 行");
    }

    #[test]
    fn deterministic_failure_dead_immediately_without_retry() {
        // doc5 卡 D5-0（doc-handoff/08 发现 1）：确定性错误码立即 dead——
        // 不进 5/15s 退避、attempts 只记本次执行一次、不留 retryable。
        let mut store = setup("detdead");
        let scope = scope_of(&store, "t", "u");
        ingest(&mut store, &scope, "s1", 1, "我叫洛溪");
        let job_id = flush(&mut store, &scope, "s1", 1);
        let run_after = job_field(&store, &job_id, "run_after");
        let claimed = store
            .claim_next_ordered_job(&plus_secs(&run_after, 1))
            .unwrap()
            .unwrap();
        store
            .fail_job_deterministic(&job_id, claimed.claim_generation, 1, "WINDOW_TOO_LARGE")
            .unwrap();
        assert_eq!(job_field(&store, &job_id, "status"), "dead");
        assert_eq!(job_field(&store, &job_id, "error_code"), "WINDOW_TOO_LARGE");
        let attempts: i32 = store
            .conn()
            .query_row(
                "SELECT attempts FROM extraction_jobs WHERE id=?1",
                params![job_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 1, "本次 attempt 只记一次");
        assert_eq!(job_field(&store, &job_id, "lease_until"), "<NULL>");
        // dead 后不因 run_after 重新可领取。
        assert!(store
            .claim_next_ordered_job(&plus_secs(&run_after, 3600))
            .unwrap()
            .is_none());
        // 旧代际的 deterministic 提交不生效。
        ingest(&mut store, &scope, "s2", 1, "s2-事件");
        let job2 = flush(&mut store, &scope, "s2", 1);
        let claimed2 = store
            .claim_next_ordered_job(&plus_secs(&run_after, 3601))
            .unwrap()
            .unwrap();
        assert_eq!(claimed2.id, job2);
        assert!(matches!(
            store.fail_job_deterministic(
                &job2,
                claimed2.claim_generation + 5,
                1,
                "WINDOW_TOO_LARGE"
            ),
            Err(StoreError::StaleClaim)
        ));
        assert_eq!(
            job_field(&store, &job2, "status"),
            "running",
            "旧代际写入不得生效"
        );
    }

    #[test]
    fn skip_audit_uses_admin_cli_actor() {
        // doc5 卡 D5-0（doc-handoff/08 发现 2）：skip 审计 actor_kind='admin_cli'（doc4/03 §4）。
        let mut store = setup("skipaudit");
        let scope = scope_of(&store, "t", "u");
        let big = "是".repeat(13_500);
        ingest(&mut store, &scope, "s1", 1, &big);
        store
            .flush_window(
                &scope,
                "dsh",
                "s1",
                1,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap();
        let dead_id: String = store.conn().query_row(
            "SELECT id FROM extraction_jobs WHERE status='dead' AND error_code='WINDOW_TOO_LARGE'",
            [], |r| r.get(0),
        ).unwrap();
        assert!(store
            .skip_dead_job(&scope, &dead_id, "运维确认超限")
            .unwrap());
        let (actor_kind, actor_id, action): (String, String, String) = store
            .conn()
            .query_row(
                "SELECT actor_kind, actor_id, action FROM audit_events WHERE action='job_skip'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(actor_kind, "admin_cli");
        assert_eq!(actor_id, "local_admin");
        let _ = action;
        // skip 表内 actor_kind 同口径。
        let skip_actor: String = store
            .conn()
            .query_row(
                "SELECT actor_kind FROM extraction_job_skips WHERE job_id=?1",
                params![dead_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(skip_actor, "admin_cli");
    }

    #[test]
    fn revision_reason_shows_policy_version() {
        // doc5/03 §1：新 active 的 revision reason 表明 extract_v3:admit_v2，
        // 不把老作业的结果标为新策略。
        let mut store = setup("revreason");
        let scope = scope_of(&store, "t", "u");
        ingest(
            &mut store,
            &scope,
            "s1",
            1,
            "我在杭州做后端开发。我主要写 Rust。",
        );
        let job_id = flush(&mut store, &scope, "s1", 1);
        let run_after = job_field(&store, &job_id, "run_after");
        let job = store
            .claim_next_ordered_job(&plus_secs(&run_after, 1))
            .unwrap()
            .unwrap();
        let origin = Origin {
            host_id: "dsh".into(),
            agent_id: "extract".into(),
            session_id: "s1".into(),
        };
        let c = memory_extract::ModelCandidate {
            source_event_id: /* 取窗口首事件 */ {
                let lower = store.window_lower_bound(&scope, "dsh", "s1", job.through_event_seq).unwrap();
                store.load_window_events(&scope, &job, lower).unwrap()[0].id.clone()
            },
            quote: "我在杭州做后端开发".into(),
            kind: "fact".into(),
            occurred_at: None,
            valid_until: None,
            confidence: None,
            claim: None,
        };
        let out = store
            .save_candidate(&scope, &job, &origin, &c, memory_extract::Admission::Active)
            .unwrap();
        let memory_id = match out {
            CandidateOutcome::Active { memory_id } => memory_id,
            other => panic!("应 active：{other:?}"),
        };
        let reason: String = store
            .conn()
            .query_row(
                "SELECT reason_code FROM memory_revisions
                 WHERE tenant_id='t' AND user_id='u' AND memory_id=?1 AND version=1",
                params![memory_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason, "extract_v4:admit_v4");
    }

    #[test]
    fn attribute_conflict_covers_new_occupation_and_practice_keys() {
        // doc5/03 §7 / 样本 A25/A26：新职业句式与主要实践参与属性冲突；
        // 同 scope 同键不同值 → held:POSSIBLE_CONFLICT，不自动覆盖。
        let mut store = setup("attrconf");
        let scope = scope_of(&store, "t", "u");
        let origin = Origin {
            host_id: "dsh".into(),
            agent_id: "extract".into(),
            session_id: "s1".into(),
        };
        let run_active = |store: &mut Store, session: &str, seq: i64, quote: &str, kind: &str| {
            ingest(store, &scope, session, seq, quote);
            let job_id = flush(store, &scope, session, seq);
            let run_after = job_field(store, &job_id, "run_after");
            let job = store
                .claim_next_ordered_job(&plus_secs(&run_after, 1))
                .unwrap()
                .unwrap();
            let ev_id = {
                let lower = store
                    .window_lower_bound(&scope, "dsh", session, job.through_event_seq)
                    .unwrap();
                store.load_window_events(&scope, &job, lower).unwrap()[0]
                    .id
                    .clone()
            };
            let c = memory_extract::ModelCandidate {
                source_event_id: ev_id,
                quote: quote.to_string(),
                kind: kind.to_string(),
                occurred_at: None,
                valid_until: None,
                confidence: None,
                claim: None,
            };
            store
                .save_candidate(&scope, &job, &origin, &c, memory_extract::Admission::Active)
                .unwrap()
        };
        // 第一条职业 active（新句式）。
        let out1 = run_active(&mut store, "s1", 1, "我在杭州做后端开发", "fact");
        assert!(matches!(out1, CandidateOutcome::Active { .. }));
        // A26：另一职业句式（老形状）→ occupation 键冲突。
        let out2 = run_active(&mut store, "s2", 1, "我在腾讯工作", "fact");
        assert_eq!(
            out2,
            CandidateOutcome::Held {
                reason: "POSSIBLE_CONFLICT"
            },
            "A26"
        );
        // A02 主要实践：无冲突 → active；同键不同值 → 冲突。
        let out3 = run_active(&mut store, "s3", 1, "我主要写 Rust", "fact");
        assert!(matches!(out3, CandidateOutcome::Active { .. }));
        let out4 = run_active(&mut store, "s4", 1, "我平时主要写 Go", "fact");
        assert_eq!(
            out4,
            CandidateOutcome::Held {
                reason: "POSSIBLE_CONFLICT"
            }
        );
        // A25：residence 冲突沿既有键。
        let out5 = run_active(&mut store, "s5", 1, "我住在杭州", "fact");
        assert!(matches!(out5, CandidateOutcome::Active { .. }));
        let out6 = run_active(&mut store, "s6", 1, "我住在成都", "fact");
        assert_eq!(
            out6,
            CandidateOutcome::Held {
                reason: "POSSIBLE_CONFLICT"
            },
            "A25"
        );
    }

    #[test]
    fn suppressed_source_rejected_on_replay() {
        // 样本 A23 / doc5/07 C：forget 后同一旧证据+同 hash 的候选重放 → SUPPRESSED_SOURCE。
        let mut store = setup("suppress2");
        let scope = scope_of(&store, "t", "u");
        let origin = Origin {
            host_id: "dsh".into(),
            agent_id: "extract".into(),
            session_id: "s1".into(),
        };
        ingest(&mut store, &scope, "s1", 1, "我喜欢Rust");
        let job_id = flush(&mut store, &scope, "s1", 1);
        let run_after = job_field(&store, &job_id, "run_after");
        let job = store
            .claim_next_ordered_job(&plus_secs(&run_after, 1))
            .unwrap()
            .unwrap();
        let ev_id = {
            let lower = store
                .window_lower_bound(&scope, "dsh", "s1", job.through_event_seq)
                .unwrap();
            store.load_window_events(&scope, &job, lower).unwrap()[0]
                .id
                .clone()
        };
        let c = memory_extract::ModelCandidate {
            source_event_id: ev_id.clone(),
            quote: "我喜欢Rust".into(),
            kind: "preference".into(),
            occurred_at: None,
            valid_until: None,
            confidence: None,
            claim: None,
        };
        let out1 = store
            .save_candidate(&scope, &job, &origin, &c, memory_extract::Admission::Active)
            .unwrap();
        let memory_id = match out1 {
            CandidateOutcome::Active { memory_id } => memory_id,
            other => panic!("应 active：{other:?}"),
        };
        store
            .complete_job(&job_id, job.claim_generation, 1, "mock", None, None)
            .unwrap();
        // 用户遗忘。
        let t = chrono::Utc::now();
        let forget_evid = match store
            .record_evidence(
                &scope,
                &origin,
                2,
                "user",
                "user",
                &t,
                "忘记我喜欢Rust",
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            crate::IngestOutcome::Recorded(id) => id,
            _ => panic!(),
        };
        store
            .forget_memory(
                &scope,
                &memory_id,
                &crate::ForgetRequest {
                    expected_version: 1,
                    origin: origin.clone(),
                    user_evidence_id: forget_evid,
                    target_quote: "我喜欢Rust".into(),
                },
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap();
        // 同一旧证据的新作业重放同 quote → SUPPRESSED_SOURCE，不复活。
        let job2 = flush(&mut store, &scope, "s1", 2);
        let run_after2 = job_field(&store, &job2, "run_after");
        let claimed2 = store
            .claim_next_ordered_job(&plus_secs(&run_after2, 1))
            .unwrap()
            .unwrap();
        let out2 = store
            .save_candidate(
                &scope,
                &claimed2,
                &origin,
                &c,
                memory_extract::Admission::Active,
            )
            .unwrap();
        assert_eq!(
            out2,
            CandidateOutcome::Rejected {
                reason: "SUPPRESSED_SOURCE"
            },
            "A23"
        );
        let (hits, _) = store
            .search_memories(
                &scope,
                "Rust",
                5,
                false,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap();
        assert!(hits.is_empty(), "遗忘后不得复活");
    }

    #[test]
    fn flush_checkpoint_advances_lower_bound_without_model() {
        // doc4/03 §2：seq 空洞内的 flush 与无 user/user 事件的组 → succeeded
        // checkpoint（零模型调用），推进下界，HTTP 语义仍 nothing_to_extract+job_id。
        let mut store = setup("checkpoint");
        let scope = scope_of(&store, "t", "u");
        ingest(&mut store, &scope, "s1", 1, "事件1");
        ingest(&mut store, &scope, "s1", 3, "事件3"); // 空洞：seq 2
        match store
            .flush_window(
                &scope,
                "dsh",
                "s1",
                1,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            FlushOutcome::Created { job_id, status } => {
                assert_eq!(status, "queued");
                let _ = job_id;
            }
            other => panic!("应 Created，实际 {other:?}"),
        }
        // flush through 2 落在空洞：无事件 → succeeded checkpoint。
        match store
            .flush_window(
                &scope,
                "dsh",
                "s1",
                2,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            FlushOutcome::NothingToExtract { job_id } => {
                assert_eq!(job_field(&store, &job_id, "status"), "succeeded");
            }
            other => panic!("空洞 flush 应 NothingToExtract+checkpoint，实际 {other:?}"),
        }
        // 无 user/user 事件的范围：checkpoint，不排队。
        let t = chrono::Utc::now();
        let o = Origin {
            host_id: "dsh".into(),
            agent_id: "agent-a".into(),
            session_id: "s2".into(),
        };
        store
            .record_evidence(
                &scope,
                &o,
                1,
                "assistant",
                "assistant",
                &t,
                "助手消息不算",
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap();
        match store
            .flush_window(
                &scope,
                "dsh",
                "s2",
                1,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            FlushOutcome::NothingToExtract { job_id } => {
                assert_eq!(job_field(&store, &job_id, "status"), "succeeded");
            }
            other => panic!("assistant-only 范围应 checkpoint，实际 {other:?}"),
        }
        // 后续真实范围（through 3）可正常创建且包含事件 3。
        match store
            .flush_window(
                &scope,
                "dsh",
                "s1",
                3,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            FlushOutcome::Created { status, .. } => assert_eq!(status, "queued"),
            other => panic!("应 Created，实际 {other:?}"),
        }
    }
}
