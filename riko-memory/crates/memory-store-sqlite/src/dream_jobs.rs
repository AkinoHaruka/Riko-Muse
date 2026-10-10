//! Dream 作业存储（doc6/02 §5、doc6/10）：trigger 持久入队（快照事务：选择待处理
//! L0 → 固化 job/inputs/evidence_state assigned）、lease/generation 状态机、
//! 候选接收（Rust 核验 evidence/byte span 后落 dream_candidates）。
//! runner 与 DSH 的持久 dispatch 通过 dream_runners / claim_generation 管理；
//! 触发器与输入始终先由 memoryd 持久化。

use memory_domain::{DomainScope, ScopeKey};
use rusqlite::{params, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::adjudication::AdjudicationJobRow;
use crate::consolidation_jobs::ConsolidationJobRow;
use crate::{now_rfc3339, Store, StoreError};

/// extract 版本（doc6/10）：独立版本化，旧 extract_v1/v2/v3 不用于 Dream。
pub const DREAM_EXTRACT_V1: &str = "dream_extract_v1";
pub const DREAM_EXTRACT_V2: &str = "dream_extract_v2";
pub const DREAM_PIPELINE_V1: &str = "dream_pipeline_v1";
pub const DREAM_POLICY_V1: &str = "dream_policy_v1";

/// 单批输入上限（doc6/10 §4.2 初值：80 事件）。
pub const DREAM_MAX_EVENTS: usize = 80;
/// 单批序列化字节上限（doc6/10 §4.2：24 KiB）。
pub const DREAM_MAX_INPUT_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone)]
pub struct DreamJobRow {
    pub id: String,
    pub purpose: String,
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

#[derive(Debug, Clone)]
pub struct DreamRedecisionRecord {
    pub candidate_id: String,
    pub evidence_id: String,
    pub redecision_kind: String,
    pub strategy_fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct DreamRunnerClaim {
    pub dream_job: DreamJobRow,
    pub adjudication_job: Option<AdjudicationJobRow>,
    pub consolidation_job: Option<ConsolidationJobRow>,
}

fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

impl Store {
    /// 当前 scope 中 lease 尚有效的 DSH runner 数量（doctor readiness）。
    pub fn dream_live_runner_count(&self, now: &str) -> Result<usize, StoreError> {
        let count: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM dream_runners WHERE lease_until>?1",
            params![now],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Count live DSH runners advertising one required execution capability.
    /// Capabilities are parsed from the persisted heartbeat, never inferred
    /// from memoryd's unrelated model-client configuration.
    pub fn dream_live_runner_capability_count(
        &self,
        now: &str,
        capability: &str,
    ) -> Result<usize, StoreError> {
        let mut stmt = self
            .conn()
            .prepare("SELECT capabilities_json FROM dream_runners WHERE lease_until>?1")?;
        let rows = stmt.query_map(params![now], |r| r.get::<_, String>(0))?;
        let mut count = 0;
        for row in rows {
            let value: serde_json::Value = match serde_json::from_str(&row?) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if value
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(capability)))
            {
                count += 1;
            }
        }
        Ok(count)
    }

    pub fn dream_runner_has_capability(
        &self,
        scope: &ScopeKey,
        runner_id: &str,
        capability: &str,
        now: &str,
    ) -> Result<bool, StoreError> {
        let capabilities: Option<String> = self
            .conn()
            .query_row(
                "SELECT capabilities_json FROM dream_runners
                 WHERE tenant_id=?1 AND user_id=?2 AND runner_id=?3 AND lease_until>?4",
                params![scope.tenant_id, scope.user_id, runner_id, now],
                |r| r.get(0),
            )
            .optional()?;
        let Some(capabilities) = capabilities else {
            return Ok(false);
        };
        let parsed: serde_json::Value = match serde_json::from_str(&capabilities) {
            Ok(value) => value,
            Err(_) => return Ok(false),
        };
        Ok(parsed
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(capability))))
    }

    /// Verifies that the currently live DSH runner owns this exact parent claim.
    pub fn dream_runner_owns(
        &self,
        scope: &ScopeKey,
        runner_id: &str,
        dream_job_id: &str,
        generation: i64,
    ) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        self.conn()
            .query_row(
                "SELECT EXISTS(
               SELECT 1 FROM dream_jobs j JOIN dream_runners r
                 ON r.tenant_id=j.tenant_id AND r.user_id=j.user_id AND r.runner_id=j.runner_id
               WHERE j.tenant_id=?1 AND j.user_id=?2 AND j.id=?3 AND j.runner_id=?4
                 AND j.status='running' AND j.claim_generation=?5
                 AND j.lease_until>?6 AND r.lease_until>?6)",
                params![
                    scope.tenant_id,
                    scope.user_id,
                    dream_job_id,
                    runner_id,
                    generation,
                    now
                ],
                |r| r.get(0),
            )
            .map_err(StoreError::from)
    }

    pub fn dream_consolidation_linked(
        &self,
        scope: &ScopeKey,
        dream_job_id: &str,
        consolidation_job_id: &str,
    ) -> Result<bool, StoreError> {
        self.conn()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM dream_consolidation_links
             WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND consolidation_job_id=?4)",
                params![
                    scope.tenant_id,
                    scope.user_id,
                    dream_job_id,
                    consolidation_job_id
                ],
                |r| r.get(0),
            )
            .map_err(StoreError::from)
    }

    /// 由 DSH 插件注册/续租常驻 runner；scope 完全来自已验证 Bearer token。
    pub fn dream_runner_heartbeat(
        &mut self,
        scope: &ScopeKey,
        runner_id: &str,
        host_id: &str,
        agent_id: &str,
        capabilities_json: &str,
        lease_secs: u64,
    ) -> Result<(), StoreError> {
        for value in [runner_id, host_id, agent_id] {
            if value.trim().is_empty() || value.chars().count() > 256 {
                return Err(StoreError::InvalidPageField);
            }
        }
        let capabilities: serde_json::Value =
            serde_json::from_str(capabilities_json).map_err(|_| StoreError::InvalidPageField)?;
        if !capabilities.is_array() || capabilities.as_array().unwrap().len() > 16 {
            return Err(StoreError::InvalidPageField);
        }
        let now = now_rfc3339()?;
        let lease_until = lease_from(&now, lease_secs)?;
        self.conn_mut().execute(
            "INSERT INTO dream_runners
               (tenant_id,user_id,runner_id,host_id,agent_id,capabilities_json,heartbeat_at,
                lease_until,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?7,?7)
             ON CONFLICT (tenant_id,user_id,runner_id) DO UPDATE SET
               host_id=excluded.host_id, agent_id=excluded.agent_id,
               capabilities_json=excluded.capabilities_json, heartbeat_at=excluded.heartbeat_at,
               lease_until=excluded.lease_until, updated_at=excluded.updated_at",
            params![
                scope.tenant_id,
                scope.user_id,
                runner_id,
                host_id,
                agent_id,
                capabilities_json,
                now,
                lease_until
            ],
        )?;
        Ok(())
    }

    /// 原子领取 DSH runner 工作：同 scope/runner 同时最多一个 Dream job；
    /// 若该 job 已有冻结裁决输入，则直接返回裁决阶段，不重新抽取。
    pub fn dream_runner_claim(
        &mut self,
        scope: &ScopeKey,
        runner_id: &str,
        now: &str,
        lease_secs: u64,
    ) -> Result<Option<DreamRunnerClaim>, StoreError> {
        let lease = lease_from(now, lease_secs)?;
        let tx = self.conn_mut().transaction()?;
        let runner_live: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM dream_runners
             WHERE tenant_id=?1 AND user_id=?2 AND runner_id=?3 AND lease_until>?4)",
            params![scope.tenant_id, scope.user_id, runner_id, now],
            |r| r.get(0),
        )?;
        if !runner_live {
            return Err(StoreError::StateConflict);
        }
        tx.execute(
            "UPDATE dream_jobs SET status='queued', runner_id=NULL, lease_until=NULL,
                    attempts=attempts+1, updated_at=?3
             WHERE tenant_id=?1 AND user_id=?2 AND status='running'
               AND (lease_until IS NULL OR lease_until<=?4)",
            params![scope.tenant_id, scope.user_id, now, now],
        )?;
        tx.execute(
            "UPDATE adjudication_jobs SET status='queued', lease_until=NULL,
                    attempts=attempts+1, updated_at=?3
             WHERE tenant_id=?1 AND user_id=?2 AND status='running'
               AND (lease_until IS NULL OR lease_until<=?4)",
            params![scope.tenant_id, scope.user_id, now, now],
        )?;
        // Recovery for a crash after page publication/consolidation completion but
        // before the parent Dream receipt was written.
        tx.execute(
            "UPDATE dream_jobs SET status='succeeded',runner_id=NULL,lease_until=NULL,updated_at=?3
             WHERE tenant_id=?1 AND user_id=?2 AND purpose='consolidation'
               AND status IN ('queued','provider_wait')
               AND EXISTS (SELECT 1 FROM dream_consolidation_links l
                 JOIN consolidation_jobs c ON c.tenant_id=l.tenant_id AND c.user_id=l.user_id
                   AND c.id=l.consolidation_job_id
                 WHERE l.tenant_id=dream_jobs.tenant_id AND l.user_id=dream_jobs.user_id
                   AND l.dream_job_id=dream_jobs.id AND c.status='succeeded')",
            params![scope.tenant_id, scope.user_id, now],
        )?;

        let active: Option<(String, String)> = tx
            .query_row(
                "SELECT id,runner_id FROM dream_jobs WHERE tenant_id=?1 AND user_id=?2
             AND status='running' AND lease_until>?3 LIMIT 1",
                params![scope.tenant_id, scope.user_id, now],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let dream_id = if let Some((id, owner)) = active {
            if owner != runner_id {
                tx.commit()?;
                return Ok(None);
            }
            let has_due_adjudication: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM adjudication_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3
                   AND status IN ('queued','retryable_failed','provider_wait') AND run_after<=?4)",
                params![scope.tenant_id, scope.user_id, id, now],
                |r| r.get(0),
            )?;
            if !has_due_adjudication {
                tx.commit()?;
                return Ok(None);
            }
            tx.execute(
                "UPDATE dream_jobs SET lease_until=?5, updated_at=?6
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3
                   AND runner_id=?4 AND status='running'",
                params![scope.tenant_id, scope.user_id, id, runner_id, lease, now],
            )?;
            id
        } else {
            let next: Option<String> = tx
                .query_row(
                    "SELECT id FROM dream_jobs WHERE tenant_id=?1 AND user_id=?2
                 AND status IN ('queued','provider_wait') AND run_after<=?3
                 ORDER BY run_after,created_at,id LIMIT 1",
                    params![scope.tenant_id, scope.user_id, now],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(id) = next else {
                tx.commit()?;
                return Ok(None);
            };
            tx.execute(
                "UPDATE dream_jobs SET status='running', runner_id=?4, lease_until=?5,
                        claim_generation=claim_generation+1, attempts=attempts+1, updated_at=?6
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3
                   AND status IN ('queued','provider_wait') AND run_after<=?7",
                params![
                    scope.tenant_id,
                    scope.user_id,
                    id,
                    runner_id,
                    lease,
                    now,
                    now
                ],
            )?;
            id
        };

        let adjudication_id: Option<String> = tx
            .query_row(
                "SELECT id FROM adjudication_jobs WHERE tenant_id=?1 AND user_id=?2
             AND dream_job_id=?3 AND status IN ('queued','retryable_failed','provider_wait')
             AND run_after<=?4 ORDER BY run_after,created_at,id LIMIT 1",
                params![scope.tenant_id, scope.user_id, dream_id, now],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = &adjudication_id {
            tx.execute(
                "UPDATE adjudication_jobs SET status='running', lease_until=?5,
                        claim_generation=claim_generation+1, attempts=attempts+1, updated_at=?6
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3
                   AND status IN ('queued','retryable_failed','provider_wait') AND run_after<=?4",
                params![scope.tenant_id, scope.user_id, id, now, lease, now],
            )?;
        }
        let purpose: String = tx.query_row(
            "SELECT purpose FROM dream_jobs WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            params![scope.tenant_id, scope.user_id, dream_id],
            |r| r.get(0),
        )?;
        let consolidation_id: Option<String> = if purpose == "consolidation" {
            tx.execute(
                "UPDATE consolidation_jobs SET status='queued',lease_until=NULL,updated_at=?4
                 WHERE tenant_id=?1 AND user_id=?2 AND status='running'
                   AND lease_until<=?3 AND id IN (
                     SELECT consolidation_job_id FROM dream_consolidation_links
                     WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?5)",
                params![scope.tenant_id, scope.user_id, now, now, dream_id],
            )?;
            let id: Option<String> = tx
                .query_row(
                    "SELECT cj.id FROM consolidation_jobs cj
                 JOIN dream_consolidation_links l
                   ON l.tenant_id=cj.tenant_id AND l.user_id=cj.user_id
                  AND l.consolidation_job_id=cj.id
                 WHERE cj.tenant_id=?1 AND cj.user_id=?2 AND l.dream_job_id=?3
                   AND cj.status='queued' AND cj.run_after<=?4
                 ORDER BY cj.run_after,cj.created_at,cj.id LIMIT 1",
                    params![scope.tenant_id, scope.user_id, dream_id, now],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = &id {
                tx.execute(
                    "UPDATE consolidation_jobs SET status='running',lease_until=?5,
                       claim_generation=claim_generation+1,attempts=attempts+1,updated_at=?6
                     WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='queued' AND run_after<=?4",
                    params![scope.tenant_id, scope.user_id, id, now, lease, now],
                )?;
            }
            id
        } else {
            None
        };
        tx.commit()?;
        let dream_job = self
            .dream_get(scope, &dream_id)?
            .ok_or(StoreError::JobNotFound)?;
        let adjudication_job = match adjudication_id {
            Some(id) => self.adjudication_get(scope, &id)?,
            None => None,
        };
        let consolidation_job = match consolidation_id {
            Some(id) => self.consolidation_get(scope, &id)?,
            None => None,
        };
        Ok(Some(DreamRunnerClaim {
            dream_job,
            adjudication_job,
            consolidation_job,
        }))
    }

    /// 长模型调用期间续租 parent Dream 与可选的 adjudication lease。
    pub fn dream_runner_lease(
        &mut self,
        scope: &ScopeKey,
        runner_id: &str,
        dream_job_id: &str,
        dream_generation: i64,
        adjudication: Option<(&str, i64)>,
        consolidation: Option<(&str, i64)>,
        lease_secs: u64,
    ) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let lease = lease_from(&now, lease_secs)?;
        let tx = self.conn_mut().transaction()?;
        let n = tx.execute(
            "UPDATE dream_jobs SET lease_until=?6,updated_at=?7
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND runner_id=?4
               AND claim_generation=?5 AND status='running'",
            params![
                scope.tenant_id,
                scope.user_id,
                dream_job_id,
                runner_id,
                dream_generation,
                lease,
                now
            ],
        )?;
        if n == 0 {
            tx.commit()?;
            return Ok(false);
        }
        if let Some((adj_id, adj_generation)) = adjudication {
            let adj = tx.execute(
                "UPDATE adjudication_jobs SET lease_until=?6,updated_at=?7
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND dream_job_id=?4
                   AND claim_generation=?5 AND status='running'",
                params![
                    scope.tenant_id,
                    scope.user_id,
                    adj_id,
                    dream_job_id,
                    adj_generation,
                    lease,
                    now
                ],
            )?;
            if adj == 0 {
                tx.commit()?;
                return Ok(false);
            }
        }
        if let Some((job_id, generation)) = consolidation {
            let updated = tx.execute(
                "UPDATE consolidation_jobs SET lease_until=?6,updated_at=?7
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND claim_generation=?4
                   AND status='running' AND EXISTS (
                     SELECT 1 FROM dream_consolidation_links l
                     WHERE l.tenant_id=?1 AND l.user_id=?2 AND l.consolidation_job_id=?3
                       AND l.dream_job_id=?5)",
                params![
                    scope.tenant_id,
                    scope.user_id,
                    job_id,
                    generation,
                    dream_job_id,
                    lease,
                    now
                ],
            )?;
            if updated == 0 {
                tx.commit()?;
                return Ok(false);
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// 创建一个无 L0 输入的、持久化的手动整理 Dream trigger，并与已冻结的
    /// consolidation job 原子关联。普通 consolidation job 本身永远不能被 runner 领取。
    pub fn dream_link_manual_consolidation(
        &mut self,
        scope: &ScopeKey,
        consolidation_job_id: &str,
        trigger_key: &str,
        dom: &DomainScope,
    ) -> Result<DreamJobRow, StoreError> {
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let dream_id =
            link_manual_consolidation_tx(&tx, scope, consolidation_job_id, trigger_key, &now, dom)?;
        tx.commit()?;
        self.dream_get(scope, &dream_id)?
            .ok_or(StoreError::JobNotFound)
    }

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
        dom: &DomainScope,
    ) -> Result<Option<DreamJobRow>, StoreError> {
        if !matches!(trigger_kind, "compact" | "scheduled" | "custom" | "manual") {
            return Err(StoreError::StateConflict);
        }
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        // 幂等：同 scope 同 trigger_key 已存在 → 原样返回（不合并进 running 输入）。
        let existing: Option<DreamJobRow> = tx
            .query_row(
                "SELECT id, purpose, trigger_kind, trigger_key, extract_version, status, attempts, run_after,
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
                   AND COALESCE((SELECT d.domain_id FROM evidence_domain_map d
                                 WHERE d.tenant_id=e.tenant_id AND d.user_id=e.user_id
                                   AND d.evidence_id=e.id), 'user_main')
                       IN (SELECT value FROM json_each(?5))
                   AND NOT EXISTS (SELECT 1 FROM suppressed_sources ss
                     WHERE ss.tenant_id=e.tenant_id AND ss.user_id=e.user_id AND ss.evidence_id=e.id
                       AND ss.domain_id IN (SELECT value FROM json_each(?5)))
                   AND NOT EXISTS (SELECT 1 FROM purge_tombstones pt
                     WHERE pt.tenant_id=e.tenant_id AND pt.user_id=e.user_id
                       AND pt.source_kind='evidence' AND pt.source_id=e.content_sha256)
                   AND NOT EXISTS (SELECT 1 FROM memory_evidence me
                     JOIN memories m ON m.tenant_id=me.tenant_id AND m.user_id=me.user_id AND m.id=me.memory_id
                     WHERE me.tenant_id=e.tenant_id AND me.user_id=e.user_id AND me.evidence_id=e.id
                       AND (m.status<>'active' OR (m.valid_until IS NOT NULL AND m.valid_until<=?4)
                         OR EXISTS (SELECT 1 FROM memory_retirements r
                           WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)
                         OR EXISTS (SELECT 1 FROM purge_jobs pj
                           WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id
                             AND pj.target_id=m.id AND pj.status IN ('pending','running'))))
                   AND NOT EXISTS (
                     SELECT 1 FROM dream_evidence_state s
                     WHERE s.tenant_id=e.tenant_id AND s.user_id=e.user_id
                       AND s.evidence_id=e.id AND s.pipeline_version=?3
                       AND s.status IN ('assigned','processed')
                   )
                 ORDER BY e.host_id, e.session_id, e.event_seq",
            )?;
            let rows = stmt.query_map(
                params![
                    scope.tenant_id,
                    scope.user_id,
                    DREAM_PIPELINE_V1,
                    now,
                    dom.read_json()
                ],
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
        let fingerprint_src: Vec<String> = candidates
            .iter()
            .map(|(id, _h, _s, seq, sha)| format!("{id}:{seq}:{sha}"))
            .collect();
        let fingerprint = sha256_hex(&fingerprint_src.join("\u{0}"));
        tx.execute(
            "INSERT INTO dream_jobs
               (id, tenant_id, user_id, trigger_kind, trigger_key, agent_id, host_id, session_id,
                pipeline_version, extract_version, status, attempts, run_after,
                claim_generation, input_fingerprint, created_at, updated_at, domain_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'queued', 0, ?11, 0, ?12, ?13, ?13, ?14)",
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
                DREAM_EXTRACT_V2,
                now,
                fingerprint,
                now,
                dom.write
            ],
        )?;
        for (order, (eid, host, session, seq, sha)) in candidates.iter().enumerate() {
            tx.execute(
                "INSERT INTO dream_job_inputs
                   (tenant_id, user_id, job_id, input_order, evidence_id, role, host_id,
                    session_id, event_seq, content_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'user', ?6, ?7, ?8, ?9)",
                params![
                    scope.tenant_id,
                    scope.user_id,
                    id,
                    order as i64,
                    eid,
                    host,
                    session,
                    seq,
                    sha
                ],
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

    /// 用户明确要求重裁一条 Held candidate：复用其有效原始证据，创建独立持久 Dream
    /// job，不重置原 evidence_state，也不把摘要转成新证据。
    pub fn dream_redecision_trigger(
        &mut self,
        scope: &ScopeKey,
        candidate_id: &str,
        idempotency_key: &str,
        agent_id: Option<&str>,
        host_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<DreamJobRow, StoreError> {
        if idempotency_key.is_empty() || idempotency_key.chars().count() > 128 {
            return Err(StoreError::InvalidPageField);
        }
        let now = now_rfc3339()?;
        let trigger_key = format!(
            "rejudge-{}",
            sha256_hex(&format!("{}\0{}", candidate_id, idempotency_key))
        );
        let tx = self.conn_mut().transaction()?;
        if let Some(existing) = tx
            .query_row(
                "SELECT id, purpose, trigger_kind, trigger_key, extract_version, status, attempts,
                        run_after, lease_until, claim_generation, input_fingerprint, error_code,
                        created_at, updated_at
                 FROM dream_jobs WHERE tenant_id=?1 AND user_id=?2 AND trigger_key=?3",
                params![scope.tenant_id, scope.user_id, trigger_key],
                map_dream_row,
            )
            .optional()?
        {
            tx.commit()?;
            return Ok(existing);
        }

        let candidate: Option<(String, String, String)> = tx
            .query_row(
                "SELECT kind, quote, policy_version FROM dream_candidates
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='held'",
                params![scope.tenant_id, scope.user_id, candidate_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((_kind, quote, policy_version)) = candidate else {
            return Err(StoreError::JobNotFound);
        };
        let mut inputs = Vec::new();
        {
            let mut stmt = tx.prepare(
            "SELECT e.id,e.host_id,e.session_id,e.event_seq,e.content_sha256,e.content,
                    ce.start_byte,ce.end_byte
             FROM dream_candidate_evidence ce
             JOIN evidence_events e ON e.tenant_id=ce.tenant_id AND e.user_id=ce.user_id
                                    AND e.id=ce.evidence_id
             WHERE ce.tenant_id=?1 AND ce.user_id=?2 AND ce.candidate_id=?3
               AND e.role='user' AND e.source_kind='user'
               AND NOT EXISTS (SELECT 1 FROM suppressed_sources ss
                 WHERE ss.tenant_id=e.tenant_id AND ss.user_id=e.user_id AND ss.evidence_id=e.id)
               AND NOT EXISTS (SELECT 1 FROM purge_tombstones pt
                 WHERE pt.tenant_id=e.tenant_id AND pt.user_id=e.user_id
                   AND pt.source_kind='evidence' AND pt.source_id=e.content_sha256)
               AND NOT EXISTS (SELECT 1 FROM memory_evidence me
                 JOIN memories m ON m.tenant_id=me.tenant_id AND m.user_id=me.user_id AND m.id=me.memory_id
                 WHERE me.tenant_id=e.tenant_id AND me.user_id=e.user_id AND me.evidence_id=e.id
                   AND (m.status<>'active' OR (m.valid_until IS NOT NULL AND m.valid_until<=?4)
                     OR EXISTS (SELECT 1 FROM memory_retirements mr
                       WHERE mr.tenant_id=m.tenant_id AND mr.user_id=m.user_id AND mr.memory_id=m.id)
                     OR EXISTS (SELECT 1 FROM purge_jobs pj
                       WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id
                         AND pj.target_id=m.id AND pj.status IN ('pending','running'))))
             ORDER BY e.host_id,e.session_id,e.event_seq",
        )?;
            let rows = stmt.query_map(
                params![scope.tenant_id, scope.user_id, candidate_id, now],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                    ))
                },
            )?;
            for row in rows {
                let (evidence_id, host, session, seq, hash, content, start, end) = row?;
                if start < 0
                    || end <= start
                    || end > content.len() as i64
                    || !content.is_char_boundary(start as usize)
                    || !content.is_char_boundary(end as usize)
                    || content.get(start as usize..end as usize) != Some(quote.as_str())
                    || sha256_hex(&content) != hash
                {
                    continue;
                }
                inputs.push((evidence_id, host, session, seq, hash));
            }
        }
        if inputs.is_empty() {
            return Err(StoreError::StaleInput);
        }
        let strategy = format!(
            "{}:{}:{}",
            policy_version,
            memory_contract::ADMISSION_VERSION_V3,
            memory_contract::ADJUDICATION_VERSION_V1
        );
        // 明确重裁以候选、冻结证据和策略版本为唯一裁决指纹；换一个 HTTP
        // idempotency key 不能制造重复模型调用。
        let previous_job: Option<String> = tx
            .query_row(
                "SELECT dream_job_id FROM dream_candidate_redecisions
                 WHERE tenant_id=?1 AND user_id=?2 AND candidate_id=?3
                   AND strategy_fingerprint=?4 ORDER BY created_at LIMIT 1",
                params![scope.tenant_id, scope.user_id, candidate_id, strategy],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(previous_job) = previous_job {
            tx.commit()?;
            return self
                .dream_get(scope, &previous_job)?
                .ok_or(StoreError::JobNotFound);
        }
        let fingerprint_input = inputs
            .iter()
            .map(|(id, _, _, seq, sha)| format!("{id}:{seq}:{sha}"))
            .collect::<Vec<_>>()
            .join("\0");
        let fingerprint = sha256_hex(&format!(
            "redecision\0{candidate_id}\0{policy_version}\0{fingerprint_input}"
        ));
        let job_id = Uuid::now_v7().to_string();
        tx.execute(
            "INSERT INTO dream_jobs
               (id,tenant_id,user_id,trigger_kind,trigger_key,agent_id,host_id,session_id,
                pipeline_version,extract_version,status,attempts,run_after,claim_generation,
                input_fingerprint,created_at,updated_at,purpose)
             VALUES (?1,?2,?3,'manual',?4,?5,?6,?7,?8,?9,'queued',0,?10,0,?11,?10,?10,'redecision')",
            params![
                job_id,
                scope.tenant_id,
                scope.user_id,
                trigger_key,
                agent_id,
                host_id,
                session_id,
                DREAM_PIPELINE_V1,
                DREAM_EXTRACT_V2,
                now,
                fingerprint
            ],
        )?;
        for (order, (evidence_id, host, session, seq, hash)) in inputs.iter().enumerate() {
            tx.execute(
                "INSERT INTO dream_job_inputs
                   (tenant_id,user_id,job_id,input_order,evidence_id,role,host_id,session_id,
                    event_seq,content_sha256)
                 VALUES (?1,?2,?3,?4,?5,'user',?6,?7,?8,?9)",
                params![
                    scope.tenant_id,
                    scope.user_id,
                    job_id,
                    order as i64,
                    evidence_id,
                    host,
                    session,
                    seq,
                    hash
                ],
            )?;
            tx.execute(
                "INSERT INTO dream_candidate_redecisions
                   (tenant_id,user_id,candidate_id,dream_job_id,evidence_id,redecision_kind,
                    strategy_fingerprint,created_at)
                 VALUES (?1,?2,?3,?4,?5,'user_request',?6,?7)",
                params![
                    scope.tenant_id,
                    scope.user_id,
                    candidate_id,
                    job_id,
                    evidence_id,
                    strategy,
                    now
                ],
            )?;
        }
        tx.commit()?;
        self.dream_get(scope, &job_id)?
            .ok_or(StoreError::JobNotFound)
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
            params![
                scope.tenant_id,
                scope.user_id,
                id,
                lease,
                generation + 1,
                now_rfc3339()?
            ],
        )?;
        Ok(self.dream_get(scope, &id)?)
    }

    /// 跨 scope 领取（内置 worker 单循环，doc6/10 §8）。先恢复过期 running 回
    /// queued；领取 due 的 queued / provider_wait（provider_wait 到期即端点恢复
    /// 续作同一冻结输入，doc6/10 §5）。返回 (scope, 行)。
    pub fn dream_claim_next(
        &mut self,
        now: &str,
        lease_secs: u64,
    ) -> Result<Option<(ScopeKey, DreamJobRow)>, StoreError> {
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
        let scope = ScopeKey {
            tenant_id: tenant,
            user_id: user,
        };
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
            let source_current: bool = tx.query_row(
                "SELECT EXISTS (
                   SELECT 1 FROM evidence_events e
                   WHERE e.tenant_id=?1 AND e.user_id=?2 AND e.id=?3
                     AND NOT EXISTS (SELECT 1 FROM suppressed_sources ss
                       WHERE ss.tenant_id=e.tenant_id AND ss.user_id=e.user_id AND ss.evidence_id=e.id)
                     AND NOT EXISTS (SELECT 1 FROM purge_tombstones pt
                       WHERE pt.tenant_id=e.tenant_id AND pt.user_id=e.user_id
                         AND pt.source_kind='evidence' AND pt.source_id=e.content_sha256)
                     AND NOT EXISTS (SELECT 1 FROM memory_evidence me
                       JOIN memories m ON m.tenant_id=me.tenant_id AND m.user_id=me.user_id AND m.id=me.memory_id
                       WHERE me.tenant_id=e.tenant_id AND me.user_id=e.user_id AND me.evidence_id=e.id
                         AND (m.status<>'active' OR (m.valid_until IS NOT NULL AND m.valid_until<=?4)
                           OR EXISTS (SELECT 1 FROM memory_retirements r
                             WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)
                           OR EXISTS (SELECT 1 FROM purge_jobs pj
                             WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id
                               AND pj.target_id=m.id AND pj.status IN ('pending','running')))))",
                params![scope.tenant_id, scope.user_id, p.evidence_id, now],
                |r| r.get(0),
            )?;
            let span_ok = match (&in_job, &content, source_current) {
                (Some(_), Some(c), true) => {
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
                    params![
                        scope.tenant_id,
                        scope.user_id,
                        cid,
                        p.evidence_id,
                        p.start_byte,
                        p.end_byte
                    ],
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
            params![
                scope.tenant_id,
                scope.user_id,
                job_id,
                expected_generation,
                model_name,
                input_tokens,
                output_tokens,
                now
            ],
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
            params![
                scope.tenant_id,
                scope.user_id,
                DREAM_PIPELINE_V1,
                now,
                job_id
            ],
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
            params![
                scope.tenant_id,
                scope.user_id,
                job_id,
                expected_generation,
                error_code,
                run_after,
                now
            ],
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
            params![
                scope.tenant_id,
                scope.user_id,
                job_id,
                expected_generation,
                error_code,
                now_rfc3339()?
            ],
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
            params![
                scope.tenant_id,
                scope.user_id,
                job_id,
                expected_generation,
                error_code,
                now_rfc3339()?
            ],
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
                 AND NOT EXISTS (SELECT 1 FROM dream_candidate_evidence ce
                   JOIN evidence_events e ON e.tenant_id=ce.tenant_id AND e.user_id=ce.user_id AND e.id=ce.evidence_id
                   WHERE ce.tenant_id=c.tenant_id AND ce.user_id=c.user_id AND ce.candidate_id=c.id
                     AND (EXISTS (SELECT 1 FROM suppressed_sources ss
                           WHERE ss.tenant_id=e.tenant_id AND ss.user_id=e.user_id AND ss.evidence_id=e.id)
                       OR EXISTS (SELECT 1 FROM purge_tombstones pt
                           WHERE pt.tenant_id=e.tenant_id AND pt.user_id=e.user_id
                             AND pt.source_kind='evidence' AND pt.source_id=e.content_sha256)
                       OR EXISTS (SELECT 1 FROM memory_evidence me
                           JOIN memories m ON m.tenant_id=me.tenant_id AND m.user_id=me.user_id AND m.id=me.memory_id
                           WHERE me.tenant_id=e.tenant_id AND me.user_id=e.user_id AND me.evidence_id=e.id
                             AND (m.status<>'active' OR (m.valid_until IS NOT NULL AND m.valid_until<=?4)
                               OR EXISTS (SELECT 1 FROM memory_retirements r
                                 WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)
                               OR EXISTS (SELECT 1 FROM purge_jobs pj
                                 WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id
                                   AND pj.target_id=m.id AND pj.status IN ('pending','running'))))))
             )
             ORDER BY id",
        )?;
        let now = now_rfc3339()?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, job_id, now], |r| {
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

    /// 当前 scope 中仍有有效来源的 Held 候选；调用方必须再以新候选做相关性筛选。
    pub fn dream_held_candidates(
        &self,
        scope: &ScopeKey,
    ) -> Result<Vec<(DreamCandidateOut, Vec<(String, i64, i64)>)>, StoreError> {
        let now = now_rfc3339()?;
        let mut stmt = self.conn().prepare(
            "SELECT c.id, c.kind, c.claim, c.quote, c.status,
                    ce.start_byte, ce.end_byte, ce.evidence_id
             FROM dream_candidates c
             JOIN dream_candidate_evidence ce
               ON ce.tenant_id=c.tenant_id AND ce.user_id=c.user_id AND ce.candidate_id=c.id
             JOIN evidence_events ev
               ON ev.tenant_id=ce.tenant_id AND ev.user_id=ce.user_id AND ev.id=ce.evidence_id
             WHERE c.tenant_id=?1 AND c.user_id=?2 AND c.status='held'
               AND c.id IN (SELECT recent.id FROM dream_candidates recent
                 WHERE recent.tenant_id=?1 AND recent.user_id=?2 AND recent.status='held'
                 ORDER BY recent.created_at DESC, recent.id DESC LIMIT 100)
               AND NOT EXISTS (SELECT 1 FROM dream_candidate_evidence bad
                 JOIN evidence_events src ON src.tenant_id=bad.tenant_id AND src.user_id=bad.user_id
                                           AND src.id=bad.evidence_id
                 WHERE bad.tenant_id=c.tenant_id AND bad.user_id=c.user_id AND bad.candidate_id=c.id
                   AND (EXISTS (SELECT 1 FROM suppressed_sources ss
                         WHERE ss.tenant_id=src.tenant_id AND ss.user_id=src.user_id AND ss.evidence_id=src.id)
                     OR EXISTS (SELECT 1 FROM purge_tombstones pt
                         WHERE pt.tenant_id=src.tenant_id AND pt.user_id=src.user_id
                           AND pt.source_kind='evidence' AND pt.source_id=src.content_sha256)
                     OR EXISTS (SELECT 1 FROM memory_evidence me
                         JOIN memories m ON m.tenant_id=me.tenant_id AND m.user_id=me.user_id AND m.id=me.memory_id
                         WHERE me.tenant_id=src.tenant_id AND me.user_id=src.user_id AND me.evidence_id=src.id
                           AND (m.status<>'active' OR (m.valid_until IS NOT NULL AND m.valid_until<=?3)
                             OR EXISTS (SELECT 1 FROM memory_retirements mr
                               WHERE mr.tenant_id=m.tenant_id AND mr.user_id=m.user_id AND mr.memory_id=m.id)
                             OR EXISTS (SELECT 1 FROM purge_jobs pj
                               WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id
                                 AND pj.target_id=m.id AND pj.status IN ('pending','running'))))))
             ORDER BY c.id, ce.evidence_id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, now], |r| {
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
            let (id, kind, claim, quote, status, start, end, evidence_id) = row?;
            match out.last_mut() {
                Some((candidate, spans)) if candidate.candidate_id == id => {
                    spans.push((evidence_id, start, end));
                }
                _ => out.push((
                    DreamCandidateOut {
                        candidate_id: id,
                        kind,
                        claim,
                        quote,
                        status,
                        evidence_id: evidence_id.clone(),
                        start_byte: start,
                        end_byte: end,
                    },
                    vec![(evidence_id, start, end)],
                )),
            }
        }
        Ok(out)
    }

    /// 同一 held candidate、同一新证据与同一策略是否已经裁决过。
    pub fn dream_candidate_redecision_seen(
        &self,
        scope: &ScopeKey,
        candidate_id: &str,
        evidence_id: &str,
        strategy_fingerprint: &str,
    ) -> Result<bool, StoreError> {
        let exists: bool = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM dream_candidate_redecisions
             WHERE tenant_id=?1 AND user_id=?2 AND candidate_id=?3 AND evidence_id=?4
               AND strategy_fingerprint=?5)",
            params![
                scope.tenant_id,
                scope.user_id,
                candidate_id,
                evidence_id,
                strategy_fingerprint
            ],
            |r| r.get(0),
        )?;
        Ok(exists)
    }

    /// 显式重裁作业冻结的旧 Held candidate 与仍有效的来源 span。
    pub fn dream_redecision_candidates(
        &self,
        scope: &ScopeKey,
        dream_job_id: &str,
    ) -> Result<Vec<(DreamCandidateOut, Vec<(String, i64, i64)>)>, StoreError> {
        let now = now_rfc3339()?;
        let mut stmt = self.conn().prepare(
            "SELECT c.id,c.kind,c.claim,c.quote,c.status,ce.start_byte,ce.end_byte,ce.evidence_id
             FROM dream_candidate_redecisions d
             JOIN dream_candidates c ON c.tenant_id=d.tenant_id AND c.user_id=d.user_id
                                      AND c.id=d.candidate_id
             JOIN dream_candidate_evidence ce ON ce.tenant_id=c.tenant_id AND ce.user_id=c.user_id
                                               AND ce.candidate_id=c.id AND ce.evidence_id=d.evidence_id
             JOIN evidence_events ev ON ev.tenant_id=ce.tenant_id AND ev.user_id=ce.user_id
                                      AND ev.id=ce.evidence_id
             WHERE d.tenant_id=?1 AND d.user_id=?2 AND d.dream_job_id=?3
               AND d.redecision_kind='user_request' AND c.status='held'
               AND ev.role='user' AND ev.source_kind='user'
               AND NOT EXISTS (SELECT 1 FROM suppressed_sources ss
                 WHERE ss.tenant_id=ev.tenant_id AND ss.user_id=ev.user_id AND ss.evidence_id=ev.id)
               AND NOT EXISTS (SELECT 1 FROM purge_tombstones pt
                 WHERE pt.tenant_id=ev.tenant_id AND pt.user_id=ev.user_id
                   AND pt.source_kind='evidence' AND pt.source_id=ev.content_sha256)
               AND NOT EXISTS (SELECT 1 FROM memory_evidence me
                 JOIN memories m ON m.tenant_id=me.tenant_id AND m.user_id=me.user_id AND m.id=me.memory_id
                 WHERE me.tenant_id=ev.tenant_id AND me.user_id=ev.user_id AND me.evidence_id=ev.id
                   AND (m.status<>'active' OR (m.valid_until IS NOT NULL AND m.valid_until<=?4)
                     OR EXISTS (SELECT 1 FROM memory_retirements mr
                       WHERE mr.tenant_id=m.tenant_id AND mr.user_id=m.user_id AND mr.memory_id=m.id)
                     OR EXISTS (SELECT 1 FROM purge_jobs pj
                       WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id
                         AND pj.target_id=m.id AND pj.status IN ('pending','running'))))
             ORDER BY c.id,ce.evidence_id",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, dream_job_id, now],
            |r| {
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
            },
        )?;
        let mut out: Vec<(DreamCandidateOut, Vec<(String, i64, i64)>)> = Vec::new();
        for row in rows {
            let (id, kind, claim, quote, status, start, end, evidence_id) = row?;
            match out.last_mut() {
                Some((candidate, spans)) if candidate.candidate_id == id => {
                    spans.push((evidence_id, start, end));
                }
                _ => out.push((
                    DreamCandidateOut {
                        candidate_id: id,
                        kind,
                        claim,
                        quote,
                        status,
                        evidence_id: evidence_id.clone(),
                        start_byte: start,
                        end_byte: end,
                    },
                    vec![(evidence_id, start, end)],
                )),
            }
        }
        Ok(out)
    }

    pub fn dream_redecision_records(
        &self,
        scope: &ScopeKey,
        dream_job_id: &str,
    ) -> Result<Vec<DreamRedecisionRecord>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT candidate_id,evidence_id,redecision_kind,strategy_fingerprint
             FROM dream_candidate_redecisions
             WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3
             ORDER BY candidate_id,evidence_id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, dream_job_id], |r| {
            Ok(DreamRedecisionRecord {
                candidate_id: r.get(0)?,
                evidence_id: r.get(1)?,
                redecision_kind: r.get(2)?,
                strategy_fingerprint: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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
    pub fn dream_renew_running_leases(
        &mut self,
        now: &str,
        lease_secs: u64,
    ) -> Result<usize, StoreError> {
        let lease = lease_from(now, lease_secs)?;
        let n = self.conn_mut().execute(
            "UPDATE dream_jobs SET lease_until=?2, updated_at=?3 WHERE status='running'",
            params![now, lease, now_rfc3339()?],
        )?;
        Ok(n)
    }

    /// 读取单 job（scope 内）。
    pub fn dream_get(
        &self,
        scope: &ScopeKey,
        job_id: &str,
    ) -> Result<Option<DreamJobRow>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT id, purpose, trigger_kind, trigger_key, extract_version, status, attempts, run_after,
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
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, job_id], |r| {
            r.get(0)
        })?;
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
            "SELECT id, purpose, trigger_kind, trigger_key, extract_version, status, attempts, run_after,
                    lease_until, claim_generation, input_fingerprint, error_code, created_at, updated_at
             FROM dream_jobs
             WHERE tenant_id=?1 AND user_id=?2 AND (?3 IS NULL OR status=?3)
             ORDER BY created_at DESC, id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, status, limit as i64],
            map_dream_row,
        )?;
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
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
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

/// Add the manual Dream parent and its consolidation association within the
/// caller's transaction. Keeping this helper transaction-scoped lets the CLI
/// create both jobs atomically.
pub(crate) fn link_manual_consolidation_tx(
    tx: &Transaction<'_>,
    scope: &ScopeKey,
    consolidation_job_id: &str,
    trigger_key: &str,
    now: &str,
    dom: &DomainScope,
) -> Result<String, StoreError> {
    if trigger_key.is_empty() || trigger_key.len() > 128 {
        return Err(StoreError::InvalidPageField);
    }
    let job: Option<(String, String)> = tx
        .query_row(
            "SELECT input_fingerprint,generator_version FROM consolidation_jobs
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            params![scope.tenant_id, scope.user_id, consolidation_job_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((fingerprint, generator)) = job else {
        return Err(StoreError::JobNotFound);
    };
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT id,purpose FROM dream_jobs WHERE tenant_id=?1 AND user_id=?2 AND trigger_key=?3",
            params![scope.tenant_id, scope.user_id, trigger_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let dream_id = match existing {
        Some((id, purpose)) if purpose == "consolidation" => id,
        Some(_) => return Err(StoreError::StateConflict),
        None => {
            let id = Uuid::now_v7().to_string();
            tx.execute(
                "INSERT INTO dream_jobs
                   (id,tenant_id,user_id,trigger_kind,trigger_key,pipeline_version,extract_version,
                    status,attempts,run_after,claim_generation,input_fingerprint,created_at,updated_at,purpose,domain_id)
                 VALUES (?1,?2,?3,'manual',?4,?5,?6,'queued',0,?7,0,?8,?7,?7,'consolidation',?9)",
                params![id,scope.tenant_id,scope.user_id,trigger_key,DREAM_PIPELINE_V1,generator,now,fingerprint,dom.write],
            )?;
            id
        }
    };
    let by_dream: Option<String> = tx
        .query_row(
            "SELECT consolidation_job_id FROM dream_consolidation_links
             WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3",
            params![scope.tenant_id, scope.user_id, dream_id],
            |r| r.get(0),
        )
        .optional()?;
    if by_dream
        .as_deref()
        .is_some_and(|linked| linked != consolidation_job_id)
    {
        return Err(StoreError::StateConflict);
    }
    let by_consolidation: Option<String> = tx
        .query_row(
            "SELECT dream_job_id FROM dream_consolidation_links
             WHERE tenant_id=?1 AND user_id=?2 AND consolidation_job_id=?3",
            params![scope.tenant_id, scope.user_id, consolidation_job_id],
            |r| r.get(0),
        )
        .optional()?;
    if by_consolidation
        .as_deref()
        .is_some_and(|linked| linked != dream_id)
    {
        return Err(StoreError::StateConflict);
    }
    tx.execute(
        "INSERT OR IGNORE INTO dream_consolidation_links
           (tenant_id,user_id,dream_job_id,consolidation_job_id,purpose,created_at)
         VALUES (?1,?2,?3,?4,'manual',?5)",
        params![
            scope.tenant_id,
            scope.user_id,
            dream_id,
            consolidation_job_id,
            now
        ],
    )?;
    Ok(dream_id)
}

fn map_dream_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DreamJobRow> {
    Ok(DreamJobRow {
        id: r.get(0)?,
        purpose: r.get(1)?,
        trigger_kind: r.get(2)?,
        trigger_key: r.get(3)?,
        extract_version: r.get(4)?,
        status: r.get(5)?,
        attempts: r.get(6)?,
        run_after: r.get(7)?,
        lease_until: r.get(8)?,
        claim_generation: r.get(9)?,
        input_fingerprint: r.get(10)?,
        error_code: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
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
        if !matches!(
            c.kind.as_str(),
            "fact" | "preference" | "instruction" | "episode"
        ) {
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
pub const DREAM_EXTRACT_V1_PROMPT: &str =
    "你是记忆整理器。给你一批已冻结的用户原话事件（每条含 evidence_id 与正文）。\
请从中提取值得长期保留的原子记忆候选。规则：\
1) 每个候选只表达一个独立方面；2) quote 必须是某条事件正文中的逐字连续子串；\
3) claim 是对 quote 的规范化改写，不得引入新事实；4) 只输出 JSON，字段固定为 \
{\"candidates\":[{\"evidence_id\",\"kind\",\"quote\",\"claim\",\"occurred_at\"}]}，\
kind 只能是 fact/preference/instruction/episode，occurred_at 可为 null；最多 20 条。";

/// DSH Dream 子 Agent 受限读取版本。历史 dream_extract_v1 作业继续使用上面的冻结提示词。
pub const DREAM_EXTRACT_V2_PROMPT: &str =
    "你是记忆整理器。输入为本 job 已冻结的 evidence 清单和原文。\
只从 role=user 的冻结证据提取原子候选；assistant/tool/system 只能作语境，不能引用。\
quote 必须是单条用户事件中的最短充分逐字连续片段，不能跨事件、拼接或补写主语；\
claim 只能规范化该 quote 明确表达的一个方面。读取工具失败或证据不清时返回空 candidates。\
输出严格 JSON：{\"candidates\":[{\"evidence_id\":\"\",\"kind\":\"fact|preference|instruction|episode\",\"quote\":\"\",\"claim\":\"\",\"occurred_at\":null}]}，最多 20 条。";

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
