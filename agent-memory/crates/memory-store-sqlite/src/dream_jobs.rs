//! Dream 作业存储（doc6/02 §5、doc6/10）：trigger 持久入队（快照事务：选择待处理
//! L0 → 固化 job/inputs/evidence_state assigned）、lease/generation 状态机、
//! 候选接收（Rust 核验 evidence/byte span 后落 dream_candidates）。
//! runner = memoryd 内置受控 worker（doc6/10 §8 路径：由持久 trigger/jobs 驱动；
//! D6-0 已核实该 seam 可用；DSH continuable subagent 仅作未来宿主触发承载）。

use memory_domain::ScopeKey;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

/// extract 版本（doc6/10）：独立版本化，旧 extract_v1/v2/v3 不用于 Dream。
pub const DREAM_EXTRACT_V1: &str = "dream_extract_v1";
pub const DREAM_PIPELINE_V1: &str = "dream_pipeline_v1";
pub const DREAM_POLICY_V1: &str = "dream_policy_v1";

/// 单批输入上限（doc6/10 §4.2 初值：80 事件）。
pub const DREAM_MAX_EVENTS: usize = 80;
/// 单批序列化字节上限（doc6/10 §4.2：24 KiB）。
pub const DREAM_MAX_INPUT_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone)]
pub struct DreamJobRow {
    pub id: String,
    pub trigger_kind: String,
    pub trigger_key: String,
    pub extract_version: String,
    pub status: String,
    pub attempts: i64,
    pub run_after: String,
    pub lease_until: Option<String>,
    pub claim_generation: i64,
    pub input_fingerprint: String,
    pub error_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct DreamCandidateOut {
    pub candidate_id: String,
    pub kind: String,
    pub claim: String,
    pub quote: String,
    pub status: String,
    pub evidence_id: String,
    pub start_byte: i64,
    pub end_byte: i64,
}

fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

impl Store {
    /// trigger 入队（doc6/10 §4）：同 trigger_key 重放返回原 job（幂等 coalesce）。
    /// 快照事务边界（doc6/10 §5）：同一事务选择待处理 L0（user events，未
    /// processed/未 assigned，跨 session 按 (host,session,event_seq) 排序，上限
    /// DREAM_MAX_EVENTS/字节边界切窗）→ 固化 job/inputs → evidence_state assigned。
    /// 快照后入库的事件保持 pending 等待下一 job。
    /// 无待处理事件时返回既有 job 或 Ok(None)（不建空作业，doc6/10 §4.2）。
    pub fn dream_trigger(
        &mut self,
        scope: &ScopeKey,
        trigger_kind: &str,
        trigger_key: &str,
        agent_id: Option<&str>,
        host_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Option<DreamJobRow>, StoreError> {
        if !matches!(trigger_kind, "compact" | "scheduled" | "custom" | "manual") {
            return Err(StoreError::StateConflict);
        }
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        // 幂等：同 scope 同 trigger_key 已存在 → 原样返回（不合并进 running 输入）。
        let existing: Option<DreamJobRow> = tx
            .query_row(
                "SELECT id, trigger_kind, trigger_key, extract_version, status, attempts, run_after,
                        lease_until, claim_generation, input_fingerprint, error_code, created_at, updated_at
                 FROM dream_jobs WHERE tenant_id=?1 AND user_id=?2 AND trigger_key=?3",
                params![scope.tenant_id, scope.user_id, trigger_key],
                map_dream_row,
            )
            .optional()?;
        if let Some(job) = existing {
            tx.commit()?;
            return Ok(Some(job));
        }
        // 快照选择：user 事件中未 processed 且未 assigned 的（跨 session），
        // 按 (host_id, session_id, event_seq) 排序，字节/条数边界切窗。
        let mut candidates: Vec<(String, String, String, i64, String)> = Vec::new(); // (id,host,session,seq,sha)
        {
            let mut stmt = tx.prepare(
                "SELECT e.id, e.host_id, e.session_id, e.event_seq, e.content_sha256, e.content
                 FROM evidence_events e
                 WHERE e.tenant_id=?1 AND e.user_id=?2 AND e.role='user' AND e.source_kind='user'
                   AND NOT EXISTS (
                     SELECT 1 FROM dream_evidence_state s
                     WHERE s.tenant_id=e.tenant_id AND s.user_id=e.user_id
                       AND s.evidence_id=e.id AND s.pipeline_version=?3
                       AND s.status IN ('assigned','processed')
                   )
                 ORDER BY e.host_id, e.session_id, e.event_seq",
            )?;
            let rows = stmt.query_map(
                params![scope.tenant_id, scope.user_id, DREAM_PIPELINE_V1],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )?;
            let mut total_bytes = 0usize;
            for row in rows {
                let (id, host, session, seq, sha, content) = row?;
                total_bytes += content.len();
                if candidates.len() >= DREAM_MAX_EVENTS || total_bytes > DREAM_MAX_INPUT_BYTES {
                    break; // 完整事件前切窗；剩余事件保持 pending（doc6/10 §4.2）。
                }
                candidates.push((id, host, session, seq, sha));
            }
        }
        if candidates.is_empty() {
            tx.commit()?;
            return Ok(None); // 无待处理 L0：不建空作业。
        }
        let id = Uuid::now_v7().to_string();
        let fingerprint_src: Vec<String> =
            candidates.iter().map(|(id, _h, _s, seq, sha)| format!("{id}:{seq}:{sha}")).collect();
        let fingerprint = sha256_hex(&fingerprint_src.join("\u{0}"));
        tx.execute(
            "INSERT INTO dream_jobs
               (id, tenant_id, user_id, trigger_kind, trigger_key, agent_id, host_id, session_id,
                pipeline_version, extract_version, status, attempts, run_after,
                claim_generation, input_fingerprint, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'queued', 0, ?11, 0, ?12, ?13, ?13)",
            params![
                id,
                scope.tenant_id,
                scope.user_id,
                trigger_kind,
                trigger_key,
                agent_id,
                host_id,
                session_id,
                DREAM_PIPELINE_V1,
                DREAM_EXTRACT_V1,
                now,
                fingerprint,
                now
            ],
        )?;
        for (order, (eid, host, session, seq, sha)) in candidates.iter().enumerate() {
            tx.execute(
                "INSERT INTO dream_job_inputs
                   (tenant_id, user_id, job_id, input_order, evidence_id, role, host_id,
                    session_id, event_seq, content_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'user', ?6, ?7, ?8, ?9)",
                params![scope.tenant_id, scope.user_id, id, order as i64, eid, host, session, seq, sha],
            )?;
            // 账本 assigned（同事务；doc6/10 §5 失败不推进水位）。
            tx.execute(
                "INSERT INTO dream_evidence_state
                   (tenant_id, user_id, evidence_id, pipeline_version, status, active_job_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'assigned', ?5, ?6)
                 ON CONFLICT (tenant_id, user_id, evidence_id, pipeline_version)
                 DO UPDATE SET status='assigned', active_job_id=?5, updated_at=?6
                 WHERE dream_evidence_state.status='pending'",
                params![scope.tenant_id, scope.user_id, eid, DREAM_PIPELINE_V1, id, now],
            )?;
        }
        tx.commit()?;
        let job = self.dream_get(scope, &id)?.ok_or(StoreError::JobNotFound)?;
        Ok(Some(job))
    }

    /// 领取（doc4 契约）。
    pub fn dream_claim(
        &mut self,
        scope: &ScopeKey,
        now: &str,
        lease_secs: u64,
    ) -> Result<Option<DreamJobRow>, StoreError> {
        let (id, generation): (String, i64) = {
            let mut stmt = self.conn().prepare(
                "SELECT id, claim_generation FROM dream_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND status='queued' AND run_after<=?3
                 ORDER BY run_after, created_at LIMIT 1",
            )?;
            match stmt.query_row(params![scope.tenant_id, scope.user_id, now], |r| {
                Ok((r.get(0)?, r.get(1)?))
            }) {
                Ok(v) => v,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        };
        let lease = lease_from(now, lease_secs)?;
        self.conn_mut().execute(
            "UPDATE dream_jobs SET status='running', lease_until=?4, claim_generation=?5,
                    attempts=attempts+1, updated_at=?6
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='queued'",
            params![scope.tenant_id, scope.user_id, id, lease, generation + 1, now_rfc3339()?],
        )?;
        Ok(self.dream_get(scope, &id)?)
    }

    /// 跨 scope 领取（内置 worker 单循环，doc6/10 §8）。先恢复过期 running 回
    /// queued；领取 due 的 queued / provider_wait（provider_wait 到期即端点恢复
    /// 续作同一冻结输入，doc6/10 §5）。返回 (scope, 行)。
    pub fn dream_claim_next(&mut self, now: &str, lease_secs: u64) -> Result<Option<(ScopeKey, DreamJobRow)>, StoreError> {
        let tx = self.conn_mut().transaction()?;
        tx.execute(
            "UPDATE dream_jobs SET status='queued', lease_until=NULL, attempts=attempts+1, updated_at=?2
             WHERE status='running' AND (lease_until IS NULL OR lease_until<=?1)",
            params![now, now],
        )?;
        let next: Option<(String, String, String)> = tx
            .query_row(
                "SELECT tenant_id, user_id, id FROM dream_jobs
                 WHERE status IN ('queued','provider_wait') AND run_after<=?1
                 ORDER BY run_after, created_at, id LIMIT 1",
                params![now],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((tenant, user, id)) = next else {
            tx.commit()?;
            return Ok(None);
        };
        let lease = (chrono::DateTime::parse_from_rfc3339(now)
            .map_err(|e| StoreError::Time(e.to_string()))?
            + chrono::Duration::seconds(lease_secs as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let n = tx.execute(
            "UPDATE dream_jobs SET status='running', lease_until=?5,
                    claim_generation=claim_generation+1, attempts=attempts+1, updated_at=?6
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status IN ('queued','provider_wait') AND run_after<=?4",
            params![tenant, user, id, now, lease, now],
        )?;
        tx.commit()?;
        if n == 0 {
            return Ok(None);
        }
        let scope = ScopeKey { tenant_id: tenant, user_id: user };
        let job = self.dream_get(&scope, &id)?;
        Ok(job.filter(|j| j.status == "running").map(|j| (scope, j)))
    }

    /// 接收 Dream 提案（doc6/10 §6）：Rust 核验每个候选的 evidence 在冻结输入内、
    /// quote 为该事件原文连续 byte span → 落 dream_candidates（candidate ID 由
    /// Rust 生成）。任一非法候选不影响其他合法候选（非法者拒绝并记录）。
    /// 返回 (accepted, rejected) 计数。
    pub fn dream_submit_candidates(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        policy_version: &str,
        proposals: &[DreamProposal],
    ) -> Result<(usize, usize), StoreError> {
        // job 必须 running 且 generation 匹配。
        let (status, gen): (String, i64) = self
            .conn()
            .query_row(
                "SELECT status, claim_generation FROM dream_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, job_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::JobNotFound)?;
        if status != "running" || gen != expected_generation {
            return Err(StoreError::StaleClaim);
        }
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let mut accepted = 0usize;
        let mut rejected = 0usize;
        for p in proposals {
            // 核 evidence 在冻结输入内（doc6/10 §6：不得引用输入集合以外 evidence）。
            let in_job: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM dream_job_inputs
                     WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3 AND evidence_id=?4",
                    params![scope.tenant_id, scope.user_id, job_id, p.evidence_id],
                    |r| r.get(0),
                )
                .optional()?;
            // 核 quote 是该事件原文连续 byte span（doc6/10 §6 共同硬门）。
            let content: Option<String> = tx
                .query_row(
                    "SELECT content FROM evidence_events
                     WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                    params![scope.tenant_id, scope.user_id, p.evidence_id],
                    |r| r.get(0),
                )
                .optional()?;
            let span_ok = match (&in_job, &content) {
                (Some(_), Some(c)) => {
                    p.start_byte < p.end_byte
                        && p.end_byte <= c.len() as i64
                        && c.is_char_boundary(p.start_byte as usize)
                        && c.is_char_boundary(p.end_byte as usize)
                        && &c[p.start_byte as usize..p.end_byte as usize] == p.quote
                }
                _ => false,
            };
            if !span_ok {
                rejected += 1;
                continue;
            }
            let cid = Uuid::now_v7().to_string();
            // OR IGNORE：同 job 同 kind+quote_hash 重复提交（重放/续作）幂等。
            let n = tx.execute(
                "INSERT OR IGNORE INTO dream_candidates
                   (id, tenant_id, user_id, dream_job_id, kind, claim, quote, quote_sha256,
                    status, reason_code, policy_version, occurred_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    cid,
                    scope.tenant_id,
                    scope.user_id,
                    job_id,
                    p.kind,
                    p.claim,
                    p.quote,
                    sha256_hex(&p.quote),
                    p.status,
                    p.reason_code,
                    policy_version,
                    p.occurred_at,
                    now
                ],
            )?;
            if n > 0 {
                tx.execute(
                    "INSERT INTO dream_candidate_evidence
                       (tenant_id, user_id, candidate_id, evidence_id, start_byte, end_byte)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![scope.tenant_id, scope.user_id, cid, p.evidence_id, p.start_byte, p.end_byte],
                )?;
            }
            accepted += 1;
        }
        tx.commit()?;
        Ok((accepted, rejected))
    }

    /// 完成（成功）：冻结输入证据推进 processed（doc6/10 §5：processed 表示已被
    /// 成功判断过，含 not_memory/defer）。接受 running 或 provider_wait（裁决
    /// provider_wait 恢复后续作完成）。
    pub fn dream_succeed(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        model_name: Option<&str>,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
    ) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let n = self.conn_mut().execute(
            "UPDATE dream_jobs SET status='succeeded', model_name=?5, input_tokens=?6,
                    output_tokens=?7, updated_at=?8
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status IN ('running','provider_wait') AND claim_generation=?4",
            params![scope.tenant_id, scope.user_id, job_id, expected_generation,
                    model_name, input_tokens, output_tokens, now],
        )?;
        if n == 0 {
            return Ok(false);
        }
        // 冻结输入的证据推进 processed（doc6/10 §5：processed = 已被成功判断过，
        // 含 not_memory/defer；只推进本 job 的 assigned 项）。
        self.conn_mut().execute(
            "UPDATE dream_evidence_state SET status='processed', updated_at=?4
             WHERE tenant_id=?1 AND user_id=?2 AND pipeline_version=?3
               AND status='assigned' AND active_job_id=?5
               AND evidence_id IN (
                 SELECT evidence_id FROM dream_job_inputs
                 WHERE tenant_id=?1 AND user_id=?2 AND job_id=?5
               )",
            params![scope.tenant_id, scope.user_id, DREAM_PIPELINE_V1, now, job_id],
        )?;
        Ok(true)
    }

    /// provider 故障：provider_wait，保留冻结输入与 assigned（doc6/10 §5 不伪装
    /// defer）；retry_delay_secs 提供退避 run_after（端点恢复续作入口）。
    pub fn dream_provider_wait(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        error_code: &str,
        retry_delay_secs: Option<i64>,
    ) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let run_after = retry_delay_secs
            .map(|s| {
                let t = chrono::DateTime::parse_from_rfc3339(&now)
                    .map_err(|e| StoreError::Time(e.to_string()))?
                    + chrono::Duration::seconds(s);
                Ok::<String, StoreError>(t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
            })
            .transpose()?;
        let n = self.conn_mut().execute(
            "UPDATE dream_jobs SET status='provider_wait', error_code=?5,
                    run_after=COALESCE(?6, run_after), updated_at=?7
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status IN ('running','provider_wait') AND claim_generation=?4",
            params![scope.tenant_id, scope.user_id, job_id, expected_generation, error_code, run_after, now],
        )?;
        Ok(n > 0)
    }

    /// 坏 JSON/未知版本等确定性失败：dead，不自动循环（doc6/10 §5）。
    pub fn dream_dead(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        error_code: &str,
    ) -> Result<bool, StoreError> {
        let n = self.conn_mut().execute(
            "UPDATE dream_jobs SET status='dead', error_code=?5, updated_at=?6
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status IN ('running','provider_wait') AND claim_generation=?4",
            params![scope.tenant_id, scope.user_id, job_id, expected_generation, error_code, now_rfc3339()?],
        )?;
        Ok(n > 0)
    }

    /// 输入漂移（裁决 stale）：整批不提交，作业 stale_input；证据保持 assigned
    /// 由后续新作业处理（doc6/10 §5）。
    pub fn dream_stale_input(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        error_code: &str,
    ) -> Result<bool, StoreError> {
        let n = self.conn_mut().execute(
            "UPDATE dream_jobs SET status='stale_input', error_code=?5, updated_at=?6
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status IN ('running','provider_wait') AND claim_generation=?4",
            params![scope.tenant_id, scope.user_id, job_id, expected_generation, error_code, now_rfc3339()?],
        )?;
        Ok(n > 0)
    }

    /// 读取冻结输入的当前正文（doc6/10 §6：模型只看冻结输入）。返回
    /// (evidence_id, role, content)；hash 不符的行返回 Err（证据被篡改不可信）。
    pub fn dream_frozen_inputs(
        &self,
        scope: &ScopeKey,
        job_id: &str,
    ) -> Result<Vec<(String, String, String)>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT i.evidence_id, i.role, e.content, e.content_sha256
             FROM dream_job_inputs i
             JOIN evidence_events e
               ON e.tenant_id=i.tenant_id AND e.user_id=i.user_id AND e.id=i.evidence_id
             WHERE i.tenant_id=?1 AND i.user_id=?2 AND i.job_id=?3
             ORDER BY i.input_order",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, job_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, role, content, sha) = row?;
            if sha256_hex(&content) != sha {
                return Err(StoreError::StaleInput);
            }
            out.push((id, role, content));
        }
        Ok(out)
    }

    /// 已接收候选（extract 之后、裁决之前）：候选主字段 + 冻结 evidence span。
    pub fn dream_accepted_candidates(
        &self,
        scope: &ScopeKey,
        job_id: &str,
    ) -> Result<Vec<(DreamCandidateOut, Vec<(String, i64, i64)>)>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, kind, claim, quote, status, start_byte, end_byte, evidence_id
             FROM (
               SELECT c.id, c.kind, c.claim, c.quote, c.status,
                      e.start_byte, e.end_byte, e.evidence_id
               FROM dream_candidates c
               JOIN dream_candidate_evidence e
                 ON e.tenant_id=c.tenant_id AND e.user_id=c.user_id AND e.candidate_id=c.id
               WHERE c.tenant_id=?1 AND c.user_id=?2 AND c.dream_job_id=?3 AND c.status='candidate'
             )
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, job_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        let mut out: Vec<(DreamCandidateOut, Vec<(String, i64, i64)>)> = Vec::new();
        for row in rows {
            let (id, kind, claim, quote, status, sb, eb, eid) = row?;
            match out.last_mut() {
                Some((c, spans)) if c.candidate_id == id => {
                    spans.push((eid, sb, eb));
                }
                _ => out.push((
                    DreamCandidateOut {
                        candidate_id: id.clone(),
                        kind,
                        claim: claim.clone(),
                        quote,
                        status,
                        evidence_id: eid.clone(),
                        start_byte: sb,
                        end_byte: eb,
                    },
                    vec![(eid, sb, eb)],
                )),
            }
        }
        Ok(out)
    }

    /// 崩溃恢复：过期 running 回 queued（冻结输入与账本保留；doc6/10 §5）。
    pub fn dream_recover_expired(&mut self, now: &str) -> Result<i64, StoreError> {
        let n = self.conn_mut().execute(
            "UPDATE dream_jobs SET status='queued', lease_until=NULL, updated_at=?2
             WHERE status='running' AND (lease_until IS NULL OR lease_until < ?1)",
            params![now, now],
        )?;
        Ok(n as i64)
    }

    /// 心跳：所有 running dream job 续租（单 worker 内串行处理；跨迭代保持
    /// 所有权，防止 extract 与其裁决之间的租约过期重领）。不动 generation。
    pub fn dream_renew_running_leases(&mut self, now: &str, lease_secs: u64) -> Result<usize, StoreError> {
        let lease = lease_from(now, lease_secs)?;
        let n = self.conn_mut().execute(
            "UPDATE dream_jobs SET lease_until=?2, updated_at=?3 WHERE status='running'",
            params![now, lease, now_rfc3339()?],
        )?;
        Ok(n)
    }

    /// 读取单 job（scope 内）。
    pub fn dream_get(&self, scope: &ScopeKey, job_id: &str) -> Result<Option<DreamJobRow>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT id, trigger_kind, trigger_key, extract_version, status, attempts, run_after,
                        lease_until, claim_generation, input_fingerprint, error_code, created_at, updated_at
                 FROM dream_jobs WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, job_id],
                map_dream_row,
            )
            .optional()?;
        Ok(row)
    }

    /// 冻结输入 evidence IDs（按顺序）。
    pub fn dream_input_evidence_ids(
        &self,
        scope: &ScopeKey,
        job_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT evidence_id FROM dream_job_inputs
             WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3 ORDER BY input_order",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, job_id], |r| r.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 作业列表（诊断）。
    pub fn dream_list(
        &self,
        scope: &ScopeKey,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DreamJobRow>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, trigger_kind, trigger_key, extract_version, status, attempts, run_after,
                    lease_until, claim_generation, input_fingerprint, error_code, created_at, updated_at
             FROM dream_jobs
             WHERE tenant_id=?1 AND user_id=?2 AND (?3 IS NULL OR status=?3)
             ORDER BY created_at DESC, id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, status, limit as i64], map_dream_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Auto Dream 到期 scope 列表（doc6/10 §4.2 初值：每 scope 距上次 scheduled
    /// dream ≥ interval_hours 小时，且最近一条 user event 距今 ≥ min_idle_minutes）。
    /// 返回 (tenant, user, 最近 user event 时间)。供 scheduler 周期调用。
    pub fn dream_auto_due(
        &self,
        now: &str,
        interval_hours: i64,
        min_idle_minutes: i64,
    ) -> Result<Vec<(String, String, Option<String>)>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT p.tenant_id, p.user_id,
                (SELECT MAX(e.occurred_at) FROM evidence_events e
                 WHERE e.tenant_id=p.tenant_id AND e.user_id=p.user_id AND e.role='user')
             FROM principals p
             WHERE NOT EXISTS (
               SELECT 1 FROM dream_jobs j
               WHERE j.tenant_id=p.tenant_id AND j.user_id=p.user_id
                 AND j.trigger_kind='scheduled' AND j.trigger_key LIKE 'auto-%'
                 AND j.created_at >= datetime('now', '-' || ?2 || ' hours')
             )",
        )?;
        let rows = stmt.query_map(params![interval_hours], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
        })?;
        let idle_cutoff = chrono::DateTime::parse_from_rfc3339(now)
            .map_err(|e| StoreError::Time(e.to_string()))?
            - chrono::Duration::minutes(min_idle_minutes);
        let mut out = Vec::new();
        for row in rows {
            let (tenant, user, last_event) = row?;
            // 至少 1 条新 user event 且空闲达标（doc6/10 §4.2）；无事件的 scope 跳过。
            if let Some(ts) = last_event {
                if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&ts) {
                    if t <= idle_cutoff {
                        out.push((tenant, user, Some(ts)));
                    }
                }
            }
        }
        Ok(out)
    }
}

fn map_dream_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DreamJobRow> {
    Ok(DreamJobRow {
        id: r.get(0)?,
        trigger_kind: r.get(1)?,
        trigger_key: r.get(2)?,
        extract_version: r.get(3)?,
        status: r.get(4)?,
        attempts: r.get(5)?,
        run_after: r.get(6)?,
        lease_until: r.get(7)?,
        claim_generation: r.get(8)?,
        input_fingerprint: r.get(9)?,
        error_code: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

fn lease_from(now: &str, secs: u64) -> Result<String, StoreError> {
    let t = chrono::DateTime::parse_from_rfc3339(now)
        .map_err(|e| StoreError::Time(e.to_string()))?
        + chrono::Duration::seconds(secs as i64);
    Ok(t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}

/// Dream 提案（模型经受限 runner 提交；Rust 核验后落库）。
#[derive(Clone)]
pub struct DreamProposal {
    pub kind: String,
    pub claim: String,
    pub quote: String,
    pub evidence_id: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub status: String,
    pub reason_code: Option<String>,
    pub occurred_at: Option<String>,
}

// ---- dream_extract_v1 输出契约（doc6/05 §3 同法：严格 JSON、可剥围栏）----

use serde::Deserialize;

/// 模型提案的单候选输入：evidence_id 引用冻结输入；quote 为逐字原文；
/// byte span 由 Rust 用 find_quote_span 定位（模型不给 span）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DreamExtractCandidate {
    pub evidence_id: String,
    pub kind: String,
    pub quote: String,
    pub claim: String,
    #[serde(default)]
    pub occurred_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DreamExtractOutput {
    pub candidates: Vec<DreamExtractCandidate>,
}

/// 校验 `dream_extract_v1` 输出：字段集固定、kind 枚举、quote/claim 非空、
/// 单批 ≤ 20 候选（与 MAX_CANDIDATES_PER_RESPONSE 一致）。byte span 定位与
/// evidence 归属由 dream_submit_candidates 核验。
pub fn parse_dream_extract_v1(raw: &str) -> Result<Vec<DreamExtractCandidate>, StoreError> {
    let t = raw.trim();
    let t = if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.trim_start_matches("json").trim_start_matches('\n');
        rest.trim_end_matches("```").trim()
    } else {
        t
    };
    let out: DreamExtractOutput =
        serde_json::from_str(t).map_err(|_| StoreError::InvalidPageField)?;
    if out.candidates.len() > memory_contract::MAX_CANDIDATES_PER_RESPONSE {
        return Err(StoreError::InvalidPageField);
    }
    for c in &out.candidates {
        if !matches!(c.kind.as_str(), "fact" | "preference" | "instruction" | "episode") {
            return Err(StoreError::InvalidPageField);
        }
        if c.quote.is_empty() || c.claim.is_empty() || c.evidence_id.is_empty() {
            return Err(StoreError::InvalidPageField);
        }
    }
    Ok(out.candidates)
}

/// dream_extract_v1 的 system prompt 模板（doc6/10 §6：只看冻结输入；逐字引用；
/// 输出严格 JSON）。修改必须新建版本常量；旧 job 按保存版本分派。
pub const DREAM_EXTRACT_V1_PROMPT: &str = "你是记忆整理器。给你一批已冻结的用户原话事件（每条含 evidence_id 与正文）。\
请从中提取值得长期保留的原子记忆候选。规则：\
1) 每个候选只表达一个独立方面；2) quote 必须是某条事件正文中的逐字连续子串；\
3) claim 是对 quote 的规范化改写，不得引入新事实；4) 只输出 JSON，字段固定为 \
{\"candidates\":[{\"evidence_id\",\"kind\",\"quote\",\"claim\",\"occurred_at\"}]}，\
kind 只能是 fact/preference/instruction/episode，occurred_at 可为 null；最多 20 条。";

/// 把模型提案转成可核验的 DreamProposal：byte span 由 Rust 在事件原文中定位
/// （quote 非逐字 → None → dream_submit_candidates 拒绝）。
pub fn locate_quote_span(content: &str, quote: &str) -> Option<(i64, i64)> {
    let start = content.find(quote)? as i64;
    let end = start + quote.len() as i64;
    if !content.is_char_boundary(start as usize) || !content.is_char_boundary(end as usize) {
        return None;
    }
    Some((start, end))
}
