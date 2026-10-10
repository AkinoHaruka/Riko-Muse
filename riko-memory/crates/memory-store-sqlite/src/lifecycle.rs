//! D6-9 生命周期治理（doc6/02 §7、doc6/12）：可逆退休（retire/restore）、
//! purge 两阶段执行闭包与墓碑、retention 策略与清理作业。
//!
//! retire 是可逆覆盖：不扩展 memories.status CHECK、不改 v1 /forget 语义；
//! 所有 read/query path 在排序前排除 retired（doc6/09 卡要求）。purge 只经
//! 可信 UI/CLI 两阶段 preview+confirm，不注册 Agent/Dream 工具。

use memory_domain::{DomainScope, Origin, ScopeKey};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::soul::{insert_receipt_tx, AuditAction, AuditLayer, MemoryAuditEntry};
use crate::{now_rfc3339, Store, StoreError};

/// Agent 生命周期请求。最新用户事件、精确 quote/span、CAS 版本和幂等键均在
/// 同一 SQLite 事务内复核；不能依赖 handler 先查后写。
pub struct RetireRequest {
    pub expected_version: i64,
    pub actor_kind: &'static str,
    pub reason_code: Option<String>,
    pub idempotency_key: String,
    pub origin: Origin,
    pub user_evidence_id: String,
    pub target_quote: String,
    pub start_byte: i64,
    pub end_byte: i64,
}

pub struct RestoreRequest {
    pub expected_version: i64,
    pub actor_kind: &'static str,
    pub idempotency_key: String,
    pub origin: Origin,
    pub user_evidence_id: String,
    pub target_quote: String,
    pub start_byte: i64,
    pub end_byte: i64,
}

#[derive(Debug, Clone)]
pub struct RetireOverrideRow {
    pub memory_id: String,
    pub memory_version: i64,
    pub actor_kind: String,
    pub reason_code: Option<String>,
    pub retired_at: String,
}

impl Store {
    // ---- retire / restore ----

    /// 退休（可逆覆盖）：核 memory 存在且 active、版本 CAS；幂等（已退休同目标
    /// 返回 false=已覆盖）。成功后 best-effort 记 memory_audit L1 update。
    pub fn retire_memory(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        req: &RetireRequest,
        dom: &DomainScope,
    ) -> Result<bool, StoreError> {
        // V2-S1：仅写域内对象可 retire（doc7/04 §3）。
        Self::require_memory_in_write_domain(self, scope, memory_id, dom)?;
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let request_sha = request_sha256(&serde_json::json!({
            "memory_id": memory_id, "expected_version": req.expected_version,
            "actor_kind": req.actor_kind, "reason_code": req.reason_code,
            "origin": {"host_id": req.origin.host_id, "agent_id": req.origin.agent_id, "session_id": req.origin.session_id},
            "user_evidence_id": req.user_evidence_id, "target_quote": req.target_quote,
            "start_byte": req.start_byte, "end_byte": req.end_byte,
        }));
        if let Some(changed) = receipt_replay_tx(
            &tx,
            scope,
            "memory_retire",
            &req.idempotency_key,
            &request_sha,
        )? {
            tx.commit()?;
            return Ok(changed);
        }
        validate_latest_instruction_tx(
            &tx,
            scope,
            &req.origin,
            &req.user_evidence_id,
            &req.target_quote,
            req.start_byte,
            req.end_byte,
        )?;
        let row: Option<(String, i64, String)> = tx
            .query_row(
                "SELECT claim, version, status FROM memories
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((claim, version, status)) = row else {
            return Err(StoreError::MemoryNotFound);
        };
        if status != "active" {
            return Err(StoreError::MemoryNotFound);
        }
        if version != req.expected_version {
            return Err(StoreError::VersionConflict);
        }
        if !claim.contains(&req.target_quote) {
            return Err(StoreError::AmbiguousTarget);
        }
        let n = tx.execute(
            "INSERT INTO memory_retirements
               (tenant_id, user_id, memory_id, memory_version, actor_kind, reason_code,
                user_evidence_id, retired_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT (tenant_id, user_id, memory_id) DO NOTHING",
            params![
                scope.tenant_id,
                scope.user_id,
                memory_id,
                version,
                req.actor_kind,
                req.reason_code,
                req.user_evidence_id,
                now
            ],
        )?;
        if n == 0 {
            insert_receipt_tx(
                &tx,
                scope,
                "memory_retire",
                &req.idempotency_key,
                &request_sha,
                "200",
                r#"{"changed":false}"#,
            )?;
            tx.commit()?;
            return Ok(false);
        }
        let stale_pages = crate::pages::stale_pages_for_memory_tx(&tx, scope, memory_id, &now)?;
        // 退休与向量失效不互斥：置 stale 立即从语义支路消失（读路径另有 get_memory 门）。
        Self::stale_vectors_in_tx(&tx, scope, "memory", memory_id)?;
        Self::mark_index_dirty(&tx)?;
        insert_receipt_tx(
            &tx,
            scope,
            "memory_retire",
            &req.idempotency_key,
            &request_sha,
            "200",
            r#"{"changed":true}"#,
        )?;
        tx.commit()?;
        self.record_memory_audit_best_effort(
            scope,
            &MemoryAuditEntry {
                record_id: memory_id.to_owned(),
                layer: AuditLayer::L1,
                action: AuditAction::Update,
                agent_id: Some(req.origin.agent_id.clone()),
                task_id: None,
                version,
                updated_at_ms: chrono::Utc::now().timestamp_millis(),
                request_id: None,
            },
        );
        for (page_id, page_version) in stale_pages {
            self.record_memory_audit_best_effort(
                scope,
                &MemoryAuditEntry {
                    record_id: page_id,
                    layer: AuditLayer::L2,
                    action: AuditAction::Delete,
                    agent_id: Some(req.origin.agent_id.clone()),
                    task_id: None,
                    version: page_version,
                    updated_at_ms: chrono::Utc::now().timestamp_millis(),
                    request_id: None,
                },
            );
        }
        Ok(true)
    }

    /// 恢复（同事务移除当前覆盖）：要求当前 active、未到期、至少一条有效 evidence；
    /// 旧 page/vector 不复活（页面来源失效为单向派生状态）。
    pub fn restore_memory(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        req: &RestoreRequest,
        dom: &DomainScope,
    ) -> Result<bool, StoreError> {
        // V2-S1：仅写域内对象可 restore（doc7/04 §3）。
        Self::require_memory_in_write_domain(self, scope, memory_id, dom)?;
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let request_sha = request_sha256(&serde_json::json!({
            "memory_id": memory_id, "expected_version": req.expected_version,
            "actor_kind": req.actor_kind,
            "origin": {"host_id": req.origin.host_id, "agent_id": req.origin.agent_id, "session_id": req.origin.session_id},
            "user_evidence_id": req.user_evidence_id, "target_quote": req.target_quote,
            "start_byte": req.start_byte, "end_byte": req.end_byte,
        }));
        if let Some(changed) = receipt_replay_tx(
            &tx,
            scope,
            "memory_restore",
            &req.idempotency_key,
            &request_sha,
        )? {
            tx.commit()?;
            return Ok(changed);
        }
        validate_latest_instruction_tx(
            &tx,
            scope,
            &req.origin,
            &req.user_evidence_id,
            &req.target_quote,
            req.start_byte,
            req.end_byte,
        )?;
        let row: Option<(i64, Option<String>)> = tx
            .query_row(
                "SELECT m.version, m.valid_until FROM memories m
             WHERE m.tenant_id=?1 AND m.user_id=?2 AND m.id=?3 AND m.status='active'
               AND (m.valid_until IS NULL OR m.valid_until > ?4)",
                params![scope.tenant_id, scope.user_id, memory_id, now],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((version, _)) = row else {
            return Err(StoreError::MemoryNotFound);
        };
        if version != req.expected_version {
            return Err(StoreError::VersionConflict);
        }
        let valid_evidence: i64 = tx.query_row(
            "SELECT COUNT(*) FROM memory_evidence me
             JOIN evidence_events e ON e.tenant_id=me.tenant_id AND e.user_id=me.user_id AND e.id=me.evidence_id
             WHERE me.tenant_id=?1 AND me.user_id=?2 AND me.memory_id=?3
               AND NOT EXISTS (SELECT 1 FROM suppressed_sources ss
                   WHERE ss.tenant_id=me.tenant_id AND ss.user_id=me.user_id AND ss.evidence_id=me.evidence_id)
               AND NOT EXISTS (SELECT 1 FROM purge_tombstones pt
                   WHERE pt.tenant_id=me.tenant_id AND pt.user_id=me.user_id
                     AND pt.source_kind='evidence' AND pt.source_id=e.content_sha256)",
            params![scope.tenant_id, scope.user_id, memory_id],
            |r| r.get(0),
        )?;
        if valid_evidence == 0 {
            return Err(StoreError::MemoryNotFound);
        }
        let n = tx.execute(
            "DELETE FROM memory_retirements WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        if n == 0 {
            insert_receipt_tx(
                &tx,
                scope,
                "memory_restore",
                &req.idempotency_key,
                &request_sha,
                "200",
                r#"{"changed":false}"#,
            )?;
            tx.commit()?;
            return Ok(false);
        }
        insert_receipt_tx(
            &tx,
            scope,
            "memory_restore",
            &req.idempotency_key,
            &request_sha,
            "200",
            r#"{"changed":true}"#,
        )?;
        tx.commit()?;
        self.record_memory_audit_best_effort(
            scope,
            &MemoryAuditEntry {
                record_id: memory_id.to_owned(),
                layer: AuditLayer::L1,
                action: AuditAction::Update,
                agent_id: Some(req.origin.agent_id.clone()),
                task_id: None,
                version,
                updated_at_ms: chrono::Utc::now().timestamp_millis(),
                request_id: None,
            },
        );
        Ok(true)
    }

    /// 退休覆盖行（诊断）。
    pub fn retirement_get(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
        dom: &DomainScope,
    ) -> Result<Option<RetireOverrideRow>, StoreError> {
        let dom_json = dom.read_json();
        self.conn()
            .query_row(
                "SELECT r.memory_id, r.memory_version, r.actor_kind, r.reason_code, r.retired_at
                 FROM memory_retirements r
                 JOIN memories m ON m.tenant_id=r.tenant_id AND m.user_id=r.user_id AND m.id=r.memory_id
                 WHERE r.tenant_id=?1 AND r.user_id=?2 AND r.memory_id=?3
                   AND m.domain_id IN (SELECT value FROM json_each(?4))",
                params![scope.tenant_id, scope.user_id, memory_id, dom_json],
                |r| {
                    Ok(RetireOverrideRow {
                        memory_id: r.get(0)?,
                        memory_version: r.get(1)?,
                        actor_kind: r.get(2)?,
                        reason_code: r.get(3)?,
                        retired_at: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 退休 memory ID 集合（search/resident 组装后过滤用；有界 scope 内集合）。
    /// V2-S1：仅收集读域集内记忆的退休记录。
    pub fn retired_ids(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT r.memory_id FROM memory_retirements r
             JOIN memories m ON m.tenant_id=r.tenant_id AND m.user_id=r.user_id AND m.id=r.memory_id
             WHERE r.tenant_id=?1 AND r.user_id=?2
               AND m.domain_id IN (SELECT value FROM json_each(?3))",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, dom.read_json()],
            |r| r.get(0),
        )?;
        Ok(rows.collect::<Result<std::collections::HashSet<_>, _>>()?)
    }

    /// V2-S1 写域闸：目标记忆必须属于写域，否则按不存在处理。
    fn require_memory_in_write_domain(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
        dom: &DomainScope,
    ) -> Result<(), StoreError> {
        let d: Option<String> = self
            .conn()
            .query_row(
                "SELECT domain_id FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
                |r| r.get(0),
            )
            .optional()?;
        match d {
            Some(d) if d == dom.write => Ok(()),
            _ => Err(StoreError::MemoryNotFound),
        }
    }
}

fn request_sha256(value: &serde_json::Value) -> String {
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

fn receipt_replay_tx(
    tx: &rusqlite::Transaction<'_>,
    scope: &ScopeKey,
    operation: &str,
    idempotency_key: &str,
    request_sha: &str,
) -> Result<Option<bool>, StoreError> {
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT request_sha256, response_json FROM mutation_receipts
         WHERE tenant_id=?1 AND user_id=?2 AND operation=?3 AND idempotency_key=?4",
            params![scope.tenant_id, scope.user_id, operation, idempotency_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some((sha, _)) if sha != request_sha => Err(StoreError::IdempotencyConflict),
        Some((_, response)) => {
            let changed = serde_json::from_str::<serde_json::Value>(&response)
                .ok()
                .and_then(|v| v.get("changed").and_then(serde_json::Value::as_bool))
                .unwrap_or(false);
            Ok(Some(changed))
        }
    }
}

fn validate_latest_instruction_tx(
    tx: &rusqlite::Transaction<'_>,
    scope: &ScopeKey,
    origin: &Origin,
    evidence_id: &str,
    quote: &str,
    start_byte: i64,
    end_byte: i64,
) -> Result<(), StoreError> {
    let event: Option<(String, String)> = tx
        .query_row(
            "SELECT id, content FROM evidence_events e
         WHERE e.tenant_id=?1 AND e.user_id=?2 AND e.host_id=?3 AND e.session_id=?4
           AND e.role='user' AND e.source_kind='user'
           AND NOT EXISTS (SELECT 1 FROM evidence_events newer
             WHERE newer.tenant_id=e.tenant_id AND newer.user_id=e.user_id
               AND newer.host_id=e.host_id AND newer.session_id=e.session_id
               AND newer.role='user' AND newer.source_kind='user' AND newer.event_seq>e.event_seq)",
            params![
                scope.tenant_id,
                scope.user_id,
                origin.host_id,
                origin.session_id
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((latest_id, content)) = event else {
        return Err(StoreError::StaleUserEvidence);
    };
    if latest_id != evidence_id {
        return Err(StoreError::StaleUserEvidence);
    }
    if start_byte < 0
        || end_byte <= start_byte
        || end_byte > content.len() as i64
        || !content.is_char_boundary(start_byte as usize)
        || !content.is_char_boundary(end_byte as usize)
        || content.get(start_byte as usize..end_byte as usize) != Some(quote)
    {
        return Err(StoreError::QuoteMismatch);
    }
    Ok(())
}

pub(crate) fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}
