//! D6-8 语义裁决持久层（doc6/02 §6、doc6/09）：adjudication_jobs 生命周期、
//! 冻结输入/召回、批量裁决结果的 Rust 复核与原子应用（admit_v3/adjudicate_v1）。
//! 只在 D6-7 固化的 Dream job 内执行；dream_job_id 关联，绝无 extraction_job_id。

use memory_domain::{claim_sha256, fold_whitespace, MemoryKind, ScopeKey};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

pub const ADJUDICATE_V1: &str = memory_contract::ADJUDICATION_VERSION_V1;

/// adjudicate_v1 system prompt（doc6/09 §5）。版本独立：修改必须新建版本常量。
pub const ADJUDICATE_V1_PROMPT: &str = "你是记忆裁决器。给你若干候选记忆（含 candidate_id、kind、claim、逐字 quote）\
以及每个候选可能相关的已有记忆（target_memory_id、kind、claim、version）。请为每个候选判断：\
1) durability：durable（长期稳定）/ time_bound（有明确期限，需给出 valid_until）/ uncertain（不确定）/ not_memory（不是可保存的记忆主张）；\
2) action：create（独立稳定的新记忆）/ attach_evidence（与某 target 表达同一 claim，只补证据）/ update（同一方面的状态变化，必须给 expected_target_version）\
/ keep_separate（相近但主体或方面不同）/ conflict（同方面主张冲突，两边都保留）/ defer（语境或证据不足，暂不决定）/ not_memory；\
3) 只能引用请求中给出的 candidate_id 与 target_memory_id，不得自造 ID；\
4) 每个候选只产生一个动作；claim 不得拼接多个独立方面。\
只输出 JSON：{\"results\":[{\"candidate_id\":\"\",\"durability\":\"\",\"action\":\"\",\"reason_code\":\"\",\
\"target_memory_id\":null,\"expected_target_version\":null,\"model_confidence\":0.0,\"valid_until\":null}]}，\
reason_code/target_memory_id/expected_target_version/model_confidence/valid_until 可为 null。";

/// 裁决作业行。
#[derive(Debug, Clone)]
pub struct AdjudicationJobRow {
    pub id: String,
    pub dream_job_id: String,
    pub input_fingerprint: String,
    pub admission_version: String,
    pub adjudication_version: String,
    pub embedding_model_id: Option<String>,
    pub status: String,
    pub attempts: i64,
    pub run_after: String,
    pub claim_generation: i64,
    pub model_name: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub error_code: Option<String>,
}

/// 冻结候选输入（dream_candidates 行 + 其冻结 evidence span）。
#[derive(Debug, Clone)]
pub struct AdjudicationCandidate {
    pub candidate_id: String,
    pub kind: String,
    pub claim: String,
    pub quote: String,
    pub status: String,
    pub evidence_id: String,
    pub start_byte: i64,
    pub end_byte: i64,
}

/// 冻结召回 target（Rust 冻结时核验过 scope 与 active 状态）。
#[derive(Debug, Clone)]
pub struct AdjudicationRecall {
    pub candidate_id: String,
    pub target_memory_id: String,
    pub target_version: i64,
    pub channel: String,
}

/// 模型裁决建议（已过 adjudicate_v1 schema 校验；Rust 仍复核引用与版本）。
#[derive(Debug, Clone)]
pub struct AdjudicationProposal {
    pub candidate_id: String,
    pub durability: String,
    pub action: String,
    pub reason_code: Option<String>,
    pub target_memory_id: Option<String>,
    pub expected_target_version: Option<i64>,
    pub model_confidence: Option<f64>,
    pub valid_until: Option<String>,
}

/// 应用结果（诊断可还原每条建议的最终落点）。
#[derive(Debug, Default)]
pub struct AdjudicationApplyOutcome {
    pub applied: usize,
    pub rejected: usize,
    pub held: usize,
    /// (candidate_id, application_status, applied_memory_id, reason)
    pub rows: Vec<(String, String, Option<String>, String)>,
}

fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

fn map_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<AdjudicationJobRow> {
    Ok(AdjudicationJobRow {
        id: r.get(0)?,
        dream_job_id: r.get(1)?,
        input_fingerprint: r.get(2)?,
        admission_version: r.get(3)?,
        adjudication_version: r.get(4)?,
        embedding_model_id: r.get(5)?,
        status: r.get(6)?,
        attempts: r.get(7)?,
        run_after: r.get(8)?,
        claim_generation: r.get(9)?,
        model_name: r.get(10)?,
        input_tokens: r.get(11)?,
        output_tokens: r.get(12)?,
        error_code: r.get(13)?,
    })
}

const JOB_COLS: &str =
    "id, dream_job_id, input_fingerprint, admission_version, adjudication_version,
    embedding_model_id, status, attempts, run_after, claim_generation,
    model_name, input_tokens, output_tokens, error_code";

impl Store {
    /// 建裁决作业并冻结输入（doc6/02 §6：重试不能换输入；同 scope/指纹/版本幂等）。
    /// 候选与召回由调用方（worker）先行计算并核验 scope/active；本方法负责
    /// 幂等落库与指纹。无候选时返回 None（不建空作业）。
    pub fn adjudication_create(
        &mut self,
        scope: &ScopeKey,
        dream_job_id: &str,
        admission_version: &str,
        adjudication_version: &str,
        embedding_model_id: Option<&str>,
        candidates: &[AdjudicationCandidate],
        recalls: &[AdjudicationRecall],
    ) -> Result<Option<AdjudicationJobRow>, StoreError> {
        if candidates.is_empty() {
            return Ok(None);
        }
        // 输入指纹：候选（id+evidence+span）与召回（id+target+version+channel）
        // 排序后哈希；同输入不重复裁决（doc6/09 §7）。
        let mut parts: Vec<String> = candidates
            .iter()
            .map(|c| {
                format!(
                    "c|{}|{}|{}|{}",
                    c.candidate_id, c.evidence_id, c.start_byte, c.end_byte
                )
            })
            .collect();
        parts.extend(recalls.iter().map(|r| {
            format!(
                "r|{}|{}|{}|{}",
                r.candidate_id, r.target_memory_id, r.target_version, r.channel
            )
        }));
        parts.sort();
        parts.dedup();
        let fingerprint = sha256_hex(&parts.join("\u{0}"));

        let existing: Option<AdjudicationJobRow> = self
            .conn()
            .query_row(
                &format!(
                    "SELECT {JOB_COLS} FROM adjudication_jobs
                     WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3
                       AND input_fingerprint=?4 AND adjudication_version=?5"
                ),
                params![
                    scope.tenant_id,
                    scope.user_id,
                    dream_job_id,
                    fingerprint,
                    adjudication_version
                ],
                map_job,
            )
            .optional()?;
        if let Some(job) = existing {
            return Ok(Some(job));
        }
        let now = now_rfc3339()?;
        let id = Uuid::now_v7().to_string();
        let tx = self.conn_mut().transaction()?;
        tx.execute(
            "INSERT INTO adjudication_jobs
               (id, tenant_id, user_id, dream_job_id, input_fingerprint, admission_version,
                adjudication_version, embedding_model_id, status, attempts, run_after,
                claim_generation, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'queued',0,?9,0,?9,?9)",
            params![
                id,
                scope.tenant_id,
                scope.user_id,
                dream_job_id,
                fingerprint,
                admission_version,
                adjudication_version,
                embedding_model_id,
                now
            ],
        )?;
        // 冻结候选 evidence（同候选多 span 时按行展开；dream_candidates 每候选
        // 至少一条 evidence，dream_submit_candidates 已核验）。
        let mut order = 0i64;
        for c in candidates {
            tx.execute(
                "INSERT INTO adjudication_job_inputs
                   (tenant_id, user_id, job_id, candidate_id, evidence_id, start_byte, end_byte, input_order)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![scope.tenant_id, scope.user_id, id, c.candidate_id,
                        c.evidence_id, c.start_byte, c.end_byte, order],
            )?;
            order += 1;
        }
        for r in recalls {
            tx.execute(
                "INSERT INTO adjudication_job_recalls
                   (tenant_id, user_id, job_id, candidate_id, target_memory_id, target_version, recall_channel)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![scope.tenant_id, scope.user_id, id, r.candidate_id,
                        r.target_memory_id, r.target_version, r.channel],
            )?;
        }
        tx.commit()?;
        let job = self
            .adjudication_get(scope, &id)?
            .ok_or(StoreError::JobNotFound)?;
        Ok(Some(job))
    }

    /// 读取裁决作业。
    pub fn adjudication_get(
        &self,
        scope: &ScopeKey,
        job_id: &str,
    ) -> Result<Option<AdjudicationJobRow>, StoreError> {
        self.conn()
            .query_row(
                &format!("SELECT {JOB_COLS} FROM adjudication_jobs WHERE tenant_id=?1 AND user_id=?2 AND id=?3"),
                params![scope.tenant_id, scope.user_id, job_id],
                map_job,
            )
            .optional()
            .map_err(Into::into)
    }

    /// 按 Dream job 找最新裁决作业（worker 续作用）。
    pub fn adjudication_get_by_dream(
        &self,
        scope: &ScopeKey,
        dream_job_id: &str,
    ) -> Result<Option<AdjudicationJobRow>, StoreError> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {JOB_COLS} FROM adjudication_jobs
                     WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3
                     ORDER BY created_at DESC, id DESC LIMIT 1"
                ),
                params![scope.tenant_id, scope.user_id, dream_job_id],
                map_job,
            )
            .optional()
            .map_err(Into::into)
    }

    /// 领取（doc4 契约）：先恢复过期 running，再原子领取 due 的
    /// queued/retryable_failed/provider_wait（provider_wait 到期即自动续作，
    /// doc6/09 §7 端点恢复条件由调用方的退避 run_after 表达）。返回 (scope, 行)。
    pub fn adjudication_claim(
        &mut self,
        now: &str,
        lease_secs: u64,
    ) -> Result<Option<(ScopeKey, AdjudicationJobRow)>, StoreError> {
        let tx = self.conn_mut().transaction()?;
        // 过期 running 回原状态（输入冻结不变；attempts 计入丢失尝试）。
        tx.execute(
            "UPDATE adjudication_jobs SET status='queued', lease_until=NULL,
                    attempts=attempts+1, updated_at=?2
             WHERE status='running' AND (lease_until IS NULL OR lease_until<=?1)",
            params![now, now],
        )?;
        let next: Option<(String, String, String)> = tx
            .query_row(
                "SELECT tenant_id, user_id, id FROM adjudication_jobs
                 WHERE status IN ('queued','retryable_failed','provider_wait') AND run_after<=?1
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
            "UPDATE adjudication_jobs SET status='running', lease_until=?5,
                    claim_generation=claim_generation+1, attempts=attempts+1, updated_at=?6
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status IN ('queued','retryable_failed','provider_wait') AND run_after<=?4",
            params![tenant, user, id, now, lease, now],
        )?;
        if n == 0 {
            tx.commit()?;
            return Ok(None);
        }
        tx.commit()?;
        let scope = ScopeKey {
            tenant_id: tenant,
            user_id: user,
        };
        let job = self.adjudication_get(&scope, &id)?;
        Ok(job.filter(|j| j.status == "running").map(|j| (scope, j)))
    }

    /// 作业收尾（generation 校验）：succeeded/retryable_failed/provider_wait/dead/stale_input。
    pub fn adjudication_finish(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        status: &str,
        error_code: Option<&str>,
        model_name: Option<&str>,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
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
            "UPDATE adjudication_jobs SET status=?5, error_code=?6, model_name=COALESCE(?7, model_name),
                    input_tokens=COALESCE(?8, input_tokens), output_tokens=COALESCE(?9, output_tokens),
                    run_after=COALESCE(?10, run_after), lease_until=NULL, updated_at=?11
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status='running' AND claim_generation=?4",
            params![scope.tenant_id, scope.user_id, job_id, expected_generation,
                    status, error_code, model_name, input_tokens, output_tokens, run_after, now],
        )?;
        Ok(n > 0)
    }

    /// 冻结输入读取（候选 + 召回）。候选主字段从 dream_candidates 连接读取，
    /// evidence span 以冻结的 adjudication_job_inputs 为准。
    pub fn adjudication_inputs(
        &self,
        scope: &ScopeKey,
        job_id: &str,
    ) -> Result<(Vec<AdjudicationCandidate>, Vec<AdjudicationRecall>), StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT i.candidate_id, c.kind, c.claim, c.quote, c.status,
                    i.evidence_id, i.start_byte, i.end_byte
             FROM adjudication_job_inputs i
             JOIN dream_candidates c
               ON c.tenant_id=i.tenant_id AND c.user_id=i.user_id AND c.id=i.candidate_id
             WHERE i.tenant_id=?1 AND i.user_id=?2 AND i.job_id=?3
             ORDER BY i.input_order",
        )?;
        let mut candidates: Vec<AdjudicationCandidate> = Vec::new();
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, job_id], |r| {
            Ok(AdjudicationCandidate {
                candidate_id: r.get(0)?,
                kind: r.get(1)?,
                claim: r.get(2)?,
                quote: r.get(3)?,
                status: r.get(4)?,
                evidence_id: r.get(5)?,
                start_byte: r.get(6)?,
                end_byte: r.get(7)?,
            })
        })?;
        for row in rows {
            candidates.push(row?);
        }
        let mut stmt = self.conn().prepare(
            "SELECT candidate_id, target_memory_id, target_version, recall_channel
             FROM adjudication_job_recalls
             WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3",
        )?;
        let mut recalls: Vec<AdjudicationRecall> = Vec::new();
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, job_id], |r| {
            Ok(AdjudicationRecall {
                candidate_id: r.get(0)?,
                target_memory_id: r.get(1)?,
                target_version: r.get(2)?,
                channel: r.get(3)?,
            })
        })?;
        for row in rows {
            recalls.push(row?);
        }
        Ok((candidates, recalls))
    }

    /// 应用裁决结果（doc6/09 §4.B.4/§5）：单事务；先复核冻结输入漂移（任一召回
    /// target 版本变化或候选已不在可裁决状态 → 整批 StaleInput，不部分提交）；
    /// 再逐条核验模型建议（引用越权/未知 action → 该条 rejected）。成功路径：
    /// create/keep_separate → 新 L1（durable 即 Active；time_bound 须有有效期限；
    /// uncertain → held）；attach_evidence → 追加证据；update → CAS 换版+supersedes；
    /// conflict/defer → 候选 held；not_memory → 候选 rejected。
    pub fn adjudication_apply(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        proposals: &[AdjudicationProposal],
    ) -> Result<AdjudicationApplyOutcome, StoreError> {
        let (status, gen): (String, i64) = self
            .conn()
            .query_row(
                "SELECT status, claim_generation FROM adjudication_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, job_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::JobNotFound)?;
        if status != "running" || gen != expected_generation {
            return Err(StoreError::StaleClaim);
        }
        let (frozen_candidates, frozen_recalls) = self.adjudication_inputs(scope, job_id)?;
        let mut by_candidate: std::collections::HashMap<&str, Vec<&AdjudicationCandidate>> =
            std::collections::HashMap::new();
        for c in &frozen_candidates {
            by_candidate
                .entry(c.candidate_id.as_str())
                .or_default()
                .push(c);
        }
        // 召回 target 冻结版本（漂移检测基准）。
        let mut frozen_target_version: std::collections::HashMap<(&str, &str), i64> =
            std::collections::HashMap::new();
        for r in &frozen_recalls {
            frozen_target_version
                .entry((r.candidate_id.as_str(), r.target_memory_id.as_str()))
                .or_insert(r.target_version);
        }
        let now = now_rfc3339()?;

        // ---- 漂移检测（doc6/09 §4.B.4：任一输入在模型运行中变化 → 整批 stale）----
        for r in &frozen_recalls {
            let cur: Option<i64> = self
                .conn()
                .query_row(
                    "SELECT version FROM memories m
                     WHERE m.tenant_id=?1 AND m.user_id=?2 AND m.id=?3 AND m.status='active'
                       AND (m.valid_until IS NULL OR m.valid_until>?4)
                       AND NOT EXISTS (SELECT 1 FROM memory_retirements mr
                         WHERE mr.tenant_id=m.tenant_id AND mr.user_id=m.user_id AND mr.memory_id=m.id)",
                    params![scope.tenant_id, scope.user_id, r.target_memory_id, now],
                    |row| row.get(0),
                )
                .optional()?;
            match cur {
                Some(v) if v == r.target_version => {}
                // 目标被并发修改/失效：整批不做部分更新，由新输入重新排队。
                _ => return Err(StoreError::StaleInput),
            }
        }

        let mut outcome = AdjudicationApplyOutcome::default();
        let tx = self.conn_mut().transaction()?;
        let stale_sources: i64 = tx.query_row(
            "SELECT COUNT(*) FROM adjudication_job_inputs ai
             JOIN evidence_events e ON e.tenant_id=ai.tenant_id AND e.user_id=ai.user_id AND e.id=ai.evidence_id
             WHERE ai.tenant_id=?1 AND ai.user_id=?2 AND ai.job_id=?3 AND e.role='user'
               AND (EXISTS (SELECT 1 FROM suppressed_sources ss
                     WHERE ss.tenant_id=e.tenant_id AND ss.user_id=e.user_id AND ss.evidence_id=e.id)
                 OR EXISTS (SELECT 1 FROM purge_tombstones pt
                     WHERE pt.tenant_id=e.tenant_id AND pt.user_id=e.user_id
                       AND pt.source_kind='evidence' AND pt.source_id=e.content_sha256)
                 OR EXISTS (SELECT 1 FROM memory_evidence me
                     JOIN memories m ON m.tenant_id=me.tenant_id AND m.user_id=me.user_id AND m.id=me.memory_id
                     WHERE me.tenant_id=e.tenant_id AND me.user_id=e.user_id AND me.evidence_id=e.id
                       AND (m.status<>'active' OR (m.valid_until IS NOT NULL AND m.valid_until<=?4)
                         OR EXISTS (SELECT 1 FROM memory_retirements mr
                           WHERE mr.tenant_id=m.tenant_id AND mr.user_id=m.user_id AND mr.memory_id=m.id)
                         OR EXISTS (SELECT 1 FROM purge_jobs pj
                           WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id AND pj.target_id=m.id
                             AND pj.status IN ('pending','running')))))",
            params![scope.tenant_id, scope.user_id, job_id, now],
            |r| r.get(0),
        )?;
        if stale_sources > 0 {
            return Err(StoreError::StaleInput);
        }
        let mut l1_audits: Vec<(String, i64)> = Vec::new();
        let mut l2_deletes: Vec<(String, i64)> = Vec::new();
        for p in proposals {
            let mut record = |application_status: &str,
                              applied_id: &Option<String>,
                              reason: &str,
                              outcome: &mut AdjudicationApplyOutcome| {
                tx.execute(
                    "INSERT INTO adjudication_results
                       (tenant_id, user_id, job_id, candidate_id, durability, action, reason_code,
                        target_memory_id, expected_target_version, applied_result_memory_id,
                        application_status, model_confidence, valid_until, created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                     ON CONFLICT (tenant_id, user_id, job_id, candidate_id) DO UPDATE SET
                       durability=?5, action=?6, reason_code=?7, target_memory_id=?8,
                       expected_target_version=?9, applied_result_memory_id=?10,
                       application_status=?11, model_confidence=?12, valid_until=?13",
                    params![
                        scope.tenant_id,
                        scope.user_id,
                        job_id,
                        p.candidate_id,
                        p.durability,
                        p.action,
                        p.reason_code,
                        p.target_memory_id,
                        p.expected_target_version,
                        applied_id,
                        application_status,
                        p.model_confidence,
                        p.valid_until,
                        now
                    ],
                )?;
                outcome.rows.push((
                    p.candidate_id.clone(),
                    application_status.to_string(),
                    applied_id.clone(),
                    reason.to_string(),
                ));
                match application_status {
                    "applied" => {
                        if p.action == "conflict" || p.action == "defer" {
                            outcome.held += 1;
                        } else {
                            outcome.applied += 1;
                        }
                    }
                    _ => outcome.rejected += 1,
                }
                Ok::<(), StoreError>(())
            };

            // 候选必须在冻结输入内。
            let Some(spans) = by_candidate.get(p.candidate_id.as_str()).cloned() else {
                record(
                    "rejected",
                    &None,
                    "candidate_not_in_frozen_inputs",
                    &mut outcome,
                )?;
                continue;
            };
            // 候选仍处于可裁决状态（candidate 或 held 重裁）；committed/rejected 不再动。
            let cand_status = spans[0].status.as_str();
            if !matches!(cand_status, "candidate" | "held") {
                record("rejected", &None, "candidate_not_adjudicable", &mut outcome)?;
                continue;
            }
            let Some(kind) = (match spans[0].kind.as_str() {
                "preference" => Some(MemoryKind::Preference),
                "instruction" => Some(MemoryKind::Instruction),
                "episode" => Some(MemoryKind::Episode),
                "fact" => Some(MemoryKind::Fact),
                _ => None,
            }) else {
                record("rejected", &None, "bad_kind", &mut outcome)?;
                continue;
            };
            // 引用核验：attach/update/conflict 的 target 必须是本候选的冻结召回。
            let target_in_recalls = p.target_memory_id.as_deref().map(|t| {
                frozen_recalls
                    .iter()
                    .any(|r| r.candidate_id == p.candidate_id && r.target_memory_id == t)
            });
            let claim = fold_whitespace(&spans[0].claim);
            match p.action.as_str() {
                "create" | "keep_separate" => {
                    if p.target_memory_id.is_some() {
                        record("rejected", &None, "unexpected_target", &mut outcome)?;
                        continue;
                    }
                    match p.durability.as_str() {
                        "durable" | "time_bound" => {
                            // time_bound 须有可验证期限（doc6/09 §5）。
                            if p.durability == "time_bound" && p.valid_until.is_none() {
                                Self::hold_candidate_tx(&tx, scope, &p.candidate_id)?;
                                record(
                                    "applied",
                                    &None,
                                    "held_time_bound_without_valid_until",
                                    &mut outcome,
                                )?;
                                continue;
                            }
                            // 精确快速路径（doc6/09 §4.B.1）：完全相同 active 只加证据。
                            let hash = claim_sha256(kind, &claim);
                            let existing: Option<(String, i64)> = tx
                                .query_row(
                                    "SELECT id, version FROM memories m
                                     WHERE tenant_id=?1 AND user_id=?2 AND kind=?3
                                       AND claim_sha256=?4 AND status='active'
                                       AND (valid_until IS NULL OR valid_until>?5)
                                       AND NOT EXISTS (SELECT 1 FROM memory_retirements mr
                                         WHERE mr.tenant_id=m.tenant_id AND mr.user_id=m.user_id AND mr.memory_id=m.id)",
                                    params![scope.tenant_id, scope.user_id, kind.as_str(), hash, now],
                                    |r| Ok((r.get(0)?, r.get(1)?)),
                                )
                                .optional()?;
                            if let Some((mid, ver)) = existing {
                                Self::attach_frozen_evidence_tx(&tx, scope, &mid, &spans)?;
                                l1_audits.push((mid.clone(), ver));
                                Self::commit_candidate_tx(&tx, scope, &p.candidate_id)?;
                                record("applied", &Some(mid), "exact_dedup_attach", &mut outcome)?;
                                continue;
                            }
                            let mid = Uuid::now_v7().to_string();
                            tx.execute(
                                "INSERT INTO memories
                                   (id, tenant_id, user_id, kind, claim, normalized_claim, claim_sha256,
                                    source_class, status, version, occurred_at, valid_from, valid_until,
                                    origin_host_id, origin_agent_id, created_at, updated_at)
                                 VALUES (?1,?2,?3,?4,?5,?6,?7,'user_explicit','active',1,NULL,NULL,?8,
                                         'dream','dream',?9,?9)",
                                params![mid, scope.tenant_id, scope.user_id, kind.as_str(), claim,
                                        memory_domain::normalize_v1(&claim), hash,
                                        p.valid_until, now],
                            )?;
                            Self::attach_frozen_evidence_tx(&tx, scope, &mid, &spans)?;
                            tx.execute(
                                "INSERT INTO memory_revisions
                                   (tenant_id, user_id, memory_id, version, previous_claim, new_claim,
                                    previous_status, new_status, actor_kind, actor_id, reason_code, changed_at)
                                 VALUES (?1,?2,?3,1,NULL,?4,NULL,'active','system',?5,'dream_adjudicate',?6)",
                                params![scope.tenant_id, scope.user_id, mid, claim, p.candidate_id, now],
                            )?;
                            Self::commit_candidate_tx(&tx, scope, &p.candidate_id)?;
                            record("applied", &Some(mid), "created", &mut outcome)?;
                        }
                        "uncertain" => {
                            Self::hold_candidate_tx(&tx, scope, &p.candidate_id)?;
                            record("applied", &None, "held_uncertain", &mut outcome)?;
                        }
                        "not_memory" => {
                            tx.execute(
                                "UPDATE dream_candidates SET status='rejected', reason_code=?4
                                 WHERE tenant_id=?1 AND user_id=?2 AND id=?4",
                                params![scope.tenant_id, scope.user_id, now, p.candidate_id],
                            )?;
                            record("applied", &None, "rejected_not_memory", &mut outcome)?;
                        }
                        _ => {
                            record("rejected", &None, "bad_durability", &mut outcome)?;
                        }
                    }
                }
                "attach_evidence" => {
                    let Some(target) = p.target_memory_id.clone() else {
                        record("rejected", &None, "missing_target", &mut outcome)?;
                        continue;
                    };
                    if target_in_recalls != Some(true) {
                        record(
                            "rejected",
                            &None,
                            "target_not_in_frozen_recalls",
                            &mut outcome,
                        )?;
                        continue;
                    }
                    let frozen_version = frozen_target_version
                        .get(&(p.candidate_id.as_str(), target.as_str()))
                        .copied()
                        .ok_or(StoreError::StaleInput)?;
                    let target_current: Option<i64> = tx
                        .query_row(
                            "SELECT m.version FROM memories m
                             WHERE m.tenant_id=?1 AND m.user_id=?2 AND m.id=?3 AND m.status='active'
                               AND m.version=?4 AND (m.valid_until IS NULL OR m.valid_until>?5)
                               AND NOT EXISTS (SELECT 1 FROM memory_retirements mr
                                 WHERE mr.tenant_id=m.tenant_id AND mr.user_id=m.user_id AND mr.memory_id=m.id)",
                            params![scope.tenant_id, scope.user_id, target, frozen_version, now],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if target_current.is_none() {
                        return Err(StoreError::StaleInput);
                    }
                    Self::attach_frozen_evidence_tx(&tx, scope, &target, &spans)?;
                    l1_audits.push((target.clone(), frozen_version));
                    Self::commit_candidate_tx(&tx, scope, &p.candidate_id)?;
                    record("applied", &Some(target), "evidence_attached", &mut outcome)?;
                }
                "update" => {
                    let (Some(target), Some(expected_v)) =
                        (p.target_memory_id.clone(), p.expected_target_version)
                    else {
                        record(
                            "rejected",
                            &None,
                            "update_requires_target_and_version",
                            &mut outcome,
                        )?;
                        continue;
                    };
                    if target_in_recalls != Some(true) {
                        record(
                            "rejected",
                            &None,
                            "target_not_in_frozen_recalls",
                            &mut outcome,
                        )?;
                        continue;
                    }
                    // CAS 换版（doc6/09 §5：更新必须带 expected_target_version）。
                    let cur: Option<(i64, String, String)> = tx
                        .query_row(
                            "SELECT version, claim, kind FROM memories m
                             WHERE m.tenant_id=?1 AND m.user_id=?2 AND m.id=?3 AND m.status='active'
                               AND (m.valid_until IS NULL OR m.valid_until>?4)
                               AND NOT EXISTS (SELECT 1 FROM memory_retirements mr
                                 WHERE mr.tenant_id=m.tenant_id AND mr.user_id=m.user_id AND mr.memory_id=m.id)",
                            params![scope.tenant_id, scope.user_id, target, now],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()?;
                    let Some((cur_v, _old_claim, _cur_kind)) = cur else {
                        record("rejected", &None, "target_not_active", &mut outcome)?;
                        continue;
                    };
                    if cur_v != expected_v {
                        // 模型给的版本与冻结召回已不一致：按确定性错误拒绝该条。
                        record("rejected", &None, "target_version_mismatch", &mut outcome)?;
                        continue;
                    }
                    // 旧版 superseded + 新版独立行（doc6/09 §5：旧内容和来源可审计）。
                    let new_id = Uuid::now_v7().to_string();
                    tx.execute(
                        "UPDATE memories SET status='superseded', version=version+1, updated_at=?4
                         WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND version=?5",
                        params![scope.tenant_id, scope.user_id, target, now, expected_v],
                    )?;
                    l1_audits.push((target.clone(), expected_v + 1));
                    l2_deletes.extend(crate::pages::stale_pages_for_memory_tx(
                        &tx, scope, &target, &now,
                    )?);
                    let hash = claim_sha256(kind, &claim);
                    tx.execute(
                        "INSERT INTO memories
                           (id, tenant_id, user_id, kind, claim, normalized_claim, claim_sha256,
                            source_class, status, version, occurred_at, valid_from, valid_until,
                            origin_host_id, origin_agent_id, created_at, updated_at)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,'user_explicit','active',1,NULL,NULL,?8,
                                 'dream','dream',?9,?9)",
                        params![
                            new_id,
                            scope.tenant_id,
                            scope.user_id,
                            kind.as_str(),
                            claim,
                            memory_domain::normalize_v1(&claim),
                            hash,
                            p.valid_until,
                            now
                        ],
                    )?;
                    Self::attach_frozen_evidence_tx(&tx, scope, &new_id, &spans)?;
                    tx.execute(
                        "INSERT INTO memory_revisions
                           (tenant_id, user_id, memory_id, version, previous_claim, new_claim,
                            previous_status, new_status, actor_kind, actor_id, reason_code, changed_at)
                         VALUES (?1,?2,?3,1,NULL,?4,NULL,'active','system',?5,'dream_adjudicate',?6)",
                        params![scope.tenant_id, scope.user_id, new_id, claim, p.candidate_id, now],
                    )?;
                    tx.execute(
                        "INSERT INTO memory_relations (tenant_id, user_id, from_memory_id, to_memory_id, kind, created_at)
                         VALUES (?1,?2,?3,?4,'supersedes',?5)",
                        params![scope.tenant_id, scope.user_id, new_id, target, now],
                    )?;
                    Self::commit_candidate_tx(&tx, scope, &p.candidate_id)?;
                    record("applied", &Some(new_id), "updated_supersedes", &mut outcome)?;
                }
                "conflict" | "defer" => {
                    if p.action == "conflict"
                        && p.target_memory_id.is_some()
                        && target_in_recalls != Some(true)
                    {
                        record(
                            "rejected",
                            &None,
                            "target_not_in_frozen_recalls",
                            &mut outcome,
                        )?;
                        continue;
                    }
                    Self::hold_candidate_tx(&tx, scope, &p.candidate_id)?;
                    record("applied", &None, "held", &mut outcome)?;
                }
                "not_memory" => {
                    tx.execute(
                        "UPDATE dream_candidates SET status='rejected', reason_code=?4
                         WHERE tenant_id=?1 AND user_id=?2 AND id=?4",
                        params![scope.tenant_id, scope.user_id, now, p.candidate_id],
                    )?;
                    record("applied", &None, "rejected_not_memory", &mut outcome)?;
                }
                _ => {
                    record("rejected", &None, "unknown_action", &mut outcome)?;
                }
            }
        }
        tx.commit()?;
        for (memory_id, version) in l1_audits {
            self.record_memory_audit_best_effort(
                scope,
                &crate::soul::MemoryAuditEntry {
                    record_id: memory_id,
                    layer: crate::soul::AuditLayer::L1,
                    action: crate::soul::AuditAction::Update,
                    agent_id: None,
                    task_id: None,
                    version,
                    updated_at_ms: chrono::Utc::now().timestamp_millis(),
                    request_id: None,
                },
            );
        }
        for (page_id, version) in l2_deletes {
            self.record_memory_audit_best_effort(
                scope,
                &crate::soul::MemoryAuditEntry {
                    record_id: page_id,
                    layer: crate::soul::AuditLayer::L2,
                    action: crate::soul::AuditAction::Delete,
                    agent_id: None,
                    task_id: None,
                    version,
                    updated_at_ms: chrono::Utc::now().timestamp_millis(),
                    request_id: None,
                },
            );
        }
        Ok(outcome)
    }

    /// 追加冻结 evidence 到目标 L1（span 取自冻结输入；已存在则忽略）。
    fn attach_frozen_evidence_tx(
        tx: &rusqlite::Transaction<'_>,
        scope: &ScopeKey,
        memory_id: &str,
        spans: &[&AdjudicationCandidate],
    ) -> Result<(), StoreError> {
        // 同候选可能冻结多行 evidence；全部追加到该 L1。
        for s in spans {
            tx.execute(
                "INSERT OR IGNORE INTO memory_evidence
                   (id, tenant_id, user_id, memory_id, evidence_id, start_byte, end_byte)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    Uuid::now_v7().to_string(),
                    scope.tenant_id,
                    scope.user_id,
                    memory_id,
                    s.evidence_id,
                    s.start_byte,
                    s.end_byte
                ],
            )?;
        }
        tx.execute(
            "UPDATE memories SET updated_at=?4
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            params![scope.tenant_id, scope.user_id, memory_id, now_rfc3339()?],
        )?;
        Ok(())
    }

    fn commit_candidate_tx(
        tx: &rusqlite::Transaction<'_>,
        scope: &ScopeKey,
        candidate_id: &str,
    ) -> Result<(), StoreError> {
        tx.execute(
            "UPDATE dream_candidates SET status='committed'
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            params![scope.tenant_id, scope.user_id, candidate_id],
        )?;
        Ok(())
    }

    fn hold_candidate_tx(
        tx: &rusqlite::Transaction<'_>,
        scope: &ScopeKey,
        candidate_id: &str,
    ) -> Result<(), StoreError> {
        tx.execute(
            "UPDATE dream_candidates SET status='held'
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            params![scope.tenant_id, scope.user_id, candidate_id],
        )?;
        Ok(())
    }
}

// ---- adjudicate_v1 输出契约（doc6/09 §5：严格 JSON、字段集固定、引用后核）----

use serde::Deserialize;

/// 模型输出的单条建议（schema 校验层；Rust 应用时仍复核引用与版本）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdjudicateV1Item {
    pub candidate_id: String,
    pub durability: String,
    pub action: String,
    #[serde(default)]
    pub reason_code: Option<String>,
    #[serde(default)]
    pub target_memory_id: Option<String>,
    #[serde(default)]
    pub expected_target_version: Option<i64>,
    #[serde(default)]
    pub model_confidence: Option<f64>,
    #[serde(default)]
    pub valid_until: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdjudicateV1Output {
    pub results: Vec<AdjudicateV1Item>,
}

/// 校验 `adjudicate_v1` 输出：字段集固定、枚举合法、confidence 仅诊断（不限阈值）。
/// 引用与版本由 Rust 在 adjudication_apply 内核验，这里只做 schema 级检查。
pub fn parse_adjudicate_v1(raw: &str) -> Result<Vec<AdjudicateV1Item>, StoreError> {
    let t = raw.trim();
    let t = if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.trim_start_matches("json").trim_start_matches('\n');
        rest.trim_end_matches("```").trim()
    } else {
        t
    };
    let out: AdjudicateV1Output =
        serde_json::from_str(t).map_err(|_| StoreError::InvalidPageField)?;
    for i in &out.results {
        if !matches!(
            i.durability.as_str(),
            "durable" | "time_bound" | "uncertain" | "not_memory"
        ) {
            return Err(StoreError::InvalidPageField);
        }
        if !matches!(
            i.action.as_str(),
            "create"
                | "attach_evidence"
                | "update"
                | "keep_separate"
                | "conflict"
                | "defer"
                | "not_memory"
        ) {
            return Err(StoreError::InvalidPageField);
        }
        if i.candidate_id.is_empty() {
            return Err(StoreError::InvalidPageField);
        }
        // confidence 只作诊断：超出 [0,1] 视为 schema 违规（不是阈值门）。
        if let Some(c) = i.model_confidence {
            if !(0.0..=1.0).contains(&c) {
                return Err(StoreError::InvalidPageField);
            }
        }
    }
    Ok(out.results)
}
