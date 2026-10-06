//! D6-9 purge 两阶段执行与 retention（doc6/02 §7、doc6/12 §5）。
//!
//! preview 只读业务记忆、只写确认元数据；confirm 原子消费 token 并在同一
//! SQLite 事务内执行依赖闭包（首版单事务有界闭包：单条记忆及其独占依赖；
//! 超出单事务安全范围的多对象批量清理由 retention 逐对象复用同一闭包）。
//! 完成后清除 confirmation 与 job 中可反查目标的 ID；两张审计表匹配行随闭包
//! 删除；删 evidence 时以 purge_tombstones 防止 spool 重放复活。

use memory_domain::ScopeKey;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use std::collections::HashSet;
use uuid::Uuid;

use crate::soul::{AuditAction, AuditLayer, MemoryAuditEntry};
use crate::{lifecycle::sha256_hex, now_rfc3339, Store, StoreError};

/// confirmation 有效期（短期一次性）。
const CONFIRMATION_TTL_SECS: i64 = 900;
const RETENTION_BATCH_MAX_OBJECTS: usize = 256;

/// preview 闭包摘要（无正文：仅计数与 ID 清单供可信 UI 展示）。
#[derive(Debug, Default, serde::Serialize)]
pub struct PurgePreview {
    pub memory_id: String,
    pub evidence_ids: Vec<String>,
    pub revision_count: i64,
    pub relation_count: i64,
    pub pin_count: i64,
    pub page_ids: Vec<String>,
    pub candidate_count: i64,
    pub audit_count: i64,
    pub dependency_fingerprint: String,
}

#[derive(Debug, serde::Serialize)]
pub struct PurgeOutcome {
    pub job_id: String,
    pub deleted: serde_json::Value,
}

impl Store {
    /// Scope keys with an explicitly enabled positive retention policy only.
    /// Used by the no-LLM timer; never opens or upgrades an external database.
    pub fn retention_enabled_scopes(&self) -> Result<Vec<ScopeKey>, StoreError> {
        let mut stmt=self.conn().prepare(
            "SELECT tenant_id,user_id FROM retention_policies
             WHERE enabled=1 AND (raw_evidence_retention_days>0 OR expired_memory_purge_after_days>0)
             ORDER BY tenant_id,user_id")?;
        let rows = stmt.query_map([], |r| {
            Ok(ScopeKey {
                tenant_id: r.get(0)?,
                user_id: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 依赖闭包计算（只读）：目标记忆 + 独占 evidence + 依赖页/候选/审计。
    /// idempotency_key 由可信调用者指定（confirm 凭同键取回无正文结果）。
    pub fn purge_preview(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        idempotency_key: &str,
    ) -> Result<(String, PurgePreview), StoreError> {
        let mut preview = self.purge_closure(scope, memory_id)?;
        let fingerprint = preview.dependency_fingerprint.clone();
        let target_version = preview_version(self, scope, memory_id)?;
        // 只写确认元数据（明文 token 只返回一次）。
        let token = Uuid::now_v7().simple().to_string();
        let now = now_rfc3339()?;
        let expires = (chrono::DateTime::parse_from_rfc3339(&now)
            .map_err(|e| StoreError::Time(e.to_string()))?
            + chrono::Duration::seconds(CONFIRMATION_TTL_SECS))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        self.conn_mut().execute(
            "INSERT INTO purge_confirmations
               (tenant_id, user_id, token_sha256, operation, target_id, target_version,
                dependency_fingerprint, idempotency_key, expires_at, consumed, created_at)
             VALUES (?1,?2,?3,'purge_memory',?4,?5,?6,?7,?8,0,?9)",
            params![
                scope.tenant_id,
                scope.user_id,
                sha256_hex(&token),
                memory_id,
                target_version,
                fingerprint,
                idempotency_key,
                expires,
                now
            ],
        )?;
        preview.memory_id = memory_id.to_string();
        Ok((token, preview))
    }

    /// confirm：原子消费 token（同幂等键重复请求取回无正文结果）；指纹变化拒绝；
    /// 执行闭包后 job 终态清除可反查目标。
    pub fn purge_confirm(
        &mut self,
        scope: &ScopeKey,
        token: &str,
        idempotency_key: &str,
    ) -> Result<PurgeOutcome, StoreError> {
        let token_sha = sha256_hex(token);
        // 先查未消费确认（token+幂等键）；再查已消费幂等记录（同键重放 → 无正文结果）。
        let row: Option<(String, i64, String, String, i64)> = self
            .conn()
            .query_row(
                "SELECT target_id, target_version, dependency_fingerprint, expires_at, consumed
                 FROM purge_confirmations
                 WHERE tenant_id=?1 AND user_id=?2 AND token_sha256=?3 AND idempotency_key=?4",
                params![scope.tenant_id, scope.user_id, token_sha, idempotency_key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((target, _tver, frozen_fp, expires_at, _consumed)) = row else {
            // 同幂等键已消费：无正文结果（不重复执行，不泄露目标）。
            let replayed: i64 = self.conn().query_row(
                "SELECT COUNT(*) FROM purge_confirmations
                 WHERE tenant_id=?1 AND user_id=?2 AND idempotency_key=?3 AND consumed=1",
                params![scope.tenant_id, scope.user_id, idempotency_key],
                |r| r.get(0),
            )?;
            if replayed > 0 {
                return Ok(PurgeOutcome {
                    job_id: String::new(),
                    deleted: serde_json::json!({"replayed": true}),
                });
            }
            return Err(StoreError::MemoryNotFound);
        };
        let now = now_rfc3339()?;
        if expires_at <= now {
            return Err(StoreError::StateConflict); // 过期确认作废
        }
        // 指纹复核：闭包在 preview/confirm 之间变化（如新增共享引用）→ 拒绝执行。
        let current = self.purge_closure(scope, &target)?;
        if current.dependency_fingerprint != frozen_fp {
            return Err(StoreError::StateConflict); // fingerprint 变化，需重新 preview
        }
        // 执行闭包（单事务）+ 原子消费 token。
        let job_id = Uuid::now_v7().to_string();
        let tx = self.conn_mut().transaction()?;
        let deleted = Self::execute_purge_tx(&tx, scope, &target, Some(&token_sha))?;
        tx.execute(
            "INSERT INTO purge_jobs
               (id, tenant_id, user_id, operation, target_id, dependency_fingerprint, status,
                attempts, deleted_counts_json, created_at, updated_at)
             VALUES (?1,?2,?3,'purge_memory',?4,?5,'succeeded',1,?6,?7,?7)",
            params![
                job_id,
                scope.tenant_id,
                scope.user_id,
                target,
                frozen_fp,
                deleted.to_string(),
                now
            ],
        )?;
        // 终态清除可反查目标的 ID/fingerprint（doc6/02 §7）。
        tx.execute(
            "UPDATE purge_jobs SET target_id=NULL, dependency_fingerprint=NULL WHERE id=?1",
            params![job_id],
        )?;
        tx.commit()?;
        Ok(PurgeOutcome { job_id, deleted })
    }

    /// 依赖闭包计算（只读）。fingerprint = 排序后的依赖 ID 清单哈希。
    pub fn purge_closure(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
    ) -> Result<PurgePreview, StoreError> {
        let exists: Option<i64> = self
            .conn()
            .query_row(
                "SELECT 1 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::MemoryNotFound);
        }
        let mut p = PurgePreview {
            memory_id: memory_id.to_string(),
            ..Default::default()
        };
        let mut ids: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT evidence_id FROM memory_evidence WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, memory_id], |r| {
                r.get(0)
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        ids.sort();
        p.evidence_ids = ids.clone();
        let count = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> Result<i64, StoreError> {
            Ok(self.conn().query_row(sql, args, |r| r.get::<_, i64>(0))?)
        };
        p.revision_count = count(
            "SELECT COUNT(*) FROM memory_revisions WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            &[&scope.tenant_id, &scope.user_id, &memory_id],
        )?;
        p.relation_count = count(
            "SELECT (SELECT COUNT(*) FROM memory_relations WHERE tenant_id=?1 AND user_id=?2 AND from_memory_id=?3)
                   + (SELECT COUNT(*) FROM memory_relations WHERE tenant_id=?1 AND user_id=?2 AND to_memory_id=?3)",
            &[&scope.tenant_id, &scope.user_id, &memory_id],
        )?;
        p.pin_count = count(
            "SELECT COUNT(*) FROM resident_pins WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            &[&scope.tenant_id, &scope.user_id, &memory_id],
        )?;
        {
            let mut stmt = self.conn().prepare(
                "SELECT DISTINCT s.page_id FROM page_sources s
                 JOIN memory_pages pg ON pg.id=s.page_id AND pg.tenant_id=s.tenant_id AND pg.user_id=s.user_id
                 WHERE s.tenant_id=?1 AND s.user_id=?2 AND s.memory_id=?3",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, memory_id], |r| {
                r.get(0)
            })?;
            p.page_ids = rows.collect::<Result<Vec<_>, _>>()?;
            p.page_ids.sort();
        }
        // 旧候选（quote/claim 闭包）：以本记忆 evidence 为来源的旧 memory_candidates。
        if !p.evidence_ids.is_empty() {
            let placeholders = p
                .evidence_ids
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT COUNT(*) FROM memory_candidates WHERE tenant_id=?1 AND user_id=?2 AND primary_evidence_id IN ({placeholders})"
            );
            let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&scope.tenant_id, &scope.user_id];
            for e in &p.evidence_ids {
                bind.push(e);
            }
            p.candidate_count = self.conn().query_row(&sql, bind.as_slice(), |r| r.get(0))?;
        }
        p.audit_count = count(
            "SELECT (SELECT COUNT(*) FROM audit_events WHERE tenant_id=?1 AND user_id=?2 AND target_id=?3)
                   + (SELECT COUNT(*) FROM memory_audit WHERE tenant_id=?1 AND user_id=?2 AND record_id=?3)",
            &[&scope.tenant_id, &scope.user_id, &memory_id],
        )?;
        // fingerprint：memory + 排序依赖集合。
        let mut fp_src: Vec<String> = p.evidence_ids.clone();
        fp_src.extend(p.page_ids.iter().cloned());
        fp_src.push(memory_id.to_string());
        fp_src.sort();
        p.dependency_fingerprint = sha256_hex(&fp_src.join("\u{0}"));
        Ok(p)
    }

    /// 依赖闭包执行（单事务；调用方持 tx）。返回无正文计数 JSON。
    pub(crate) fn execute_purge_tx(
        tx: &rusqlite::Transaction<'_>,
        scope: &ScopeKey,
        memory_id: &str,
        consumed_confirmation_sha: Option<&str>,
    ) -> Result<serde_json::Value, StoreError> {
        let now = now_rfc3339()?;
        // 1. 收集本记忆的 evidence（id + content sha；墓碑按 sha 防重放）。
        let evidence_ids: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT i.evidence_id, e.content_sha256 FROM memory_evidence i
                 JOIN evidence_events e ON e.tenant_id=i.tenant_id AND e.user_id=i.user_id AND e.id=i.evidence_id
                 WHERE i.tenant_id=?1 AND i.user_id=?2 AND i.memory_id=?3",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, memory_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut closure_ids: HashSet<String> = HashSet::from([memory_id.to_owned()]);
        // 2. 逐 evidence 判共享（其他记忆引用即共享，保留 evidence 本体）。
        let mut deleted_evidence: Vec<String> = Vec::new();
        for (eid, ev_sha) in &evidence_ids {
            // 先删本记忆的链接（FK 到 evidence_events），共享判定只看其他对象。
            tx.execute(
                "DELETE FROM memory_evidence WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3 AND evidence_id=?4",
                params![scope.tenant_id, scope.user_id, memory_id, eid],
            )?;
            let shared: i64 = tx.query_row(
                "SELECT (SELECT COUNT(*) FROM memory_evidence WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3 AND memory_id<>?4)
                       + (SELECT COUNT(*) FROM suppressed_sources WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3 AND forgotten_memory_id<>?4)",
                params![scope.tenant_id, scope.user_id, eid, memory_id],
                |r| r.get(0),
            )?;
            if shared > 0 {
                continue; // 共享 evidence 不删本体（其他对象仍引用）
            }
            closure_ids.insert(eid.clone());
            // 3a. 旧候选闭包：primary_evidence 指向该事件的 memory_candidates（quote/claim）。
            let old_candidates: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT id FROM memory_candidates WHERE tenant_id=?1 AND user_id=?2 AND primary_evidence_id=?3",
                )?;
                let rows =
                    stmt.query_map(params![scope.tenant_id, scope.user_id, eid], |r| r.get(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            closure_ids.extend(old_candidates.iter().cloned());
            for oc in old_candidates {
                tx.execute(
                    "DELETE FROM candidate_evidence WHERE tenant_id=?1 AND user_id=?2 AND candidate_id=?3",
                    params![scope.tenant_id, scope.user_id, oc],
                )?;
                tx.execute(
                    "DELETE FROM memory_candidates WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                    params![scope.tenant_id, scope.user_id, oc],
                )?;
            }
            // 3b. Dream 候选/裁决闭包：引用该事件的 dream_candidate_evidence → 候选 → 裁决结果。
            let dream_candidates: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT DISTINCT candidate_id FROM dream_candidate_evidence
                     WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
                )?;
                let rows =
                    stmt.query_map(params![scope.tenant_id, scope.user_id, eid], |r| r.get(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            closure_ids.extend(dream_candidates.iter().cloned());
            for dc in dream_candidates {
                tx.execute(
                    "DELETE FROM adjudication_results WHERE tenant_id=?1 AND user_id=?2 AND candidate_id=?3",
                    params![scope.tenant_id, scope.user_id, dc],
                )?;
                tx.execute(
                    "DELETE FROM adjudication_job_recalls WHERE tenant_id=?1 AND user_id=?2 AND candidate_id=?3",
                    params![scope.tenant_id, scope.user_id, dc],
                )?;
                tx.execute(
                    "DELETE FROM adjudication_job_inputs WHERE tenant_id=?1 AND user_id=?2 AND candidate_id=?3",
                    params![scope.tenant_id, scope.user_id, dc],
                )?;
                tx.execute(
                    "DELETE FROM dream_candidate_evidence WHERE tenant_id=?1 AND user_id=?2 AND candidate_id=?3",
                    params![scope.tenant_id, scope.user_id, dc],
                )?;
                tx.execute(
                    "DELETE FROM dream_candidates WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                    params![scope.tenant_id, scope.user_id, dc],
                )?;
            }
            tx.execute(
                "DELETE FROM adjudication_job_inputs WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
                params![scope.tenant_id, scope.user_id, eid],
            )?;
            let affected_adjudication_jobs: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT DISTINCT aj.id FROM adjudication_jobs aj
                     JOIN dream_job_inputs di ON di.tenant_id=aj.tenant_id AND di.user_id=aj.user_id AND di.job_id=aj.dream_job_id
                     WHERE di.tenant_id=?1 AND di.user_id=?2 AND di.evidence_id=?3",
                )?;
                let rows =
                    stmt.query_map(params![scope.tenant_id, scope.user_id, eid], |r| r.get(0))?;
                rows.collect::<Result<Vec<String>, _>>()?
            };
            closure_ids.extend(affected_adjudication_jobs.iter().cloned());
            for job_id in affected_adjudication_jobs {
                tx.execute(
                    "DELETE FROM adjudication_results WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3",
                    params![scope.tenant_id, scope.user_id, job_id],
                )?;
                tx.execute(
                    "DELETE FROM adjudication_job_recalls WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3",
                    params![scope.tenant_id, scope.user_id, job_id],
                )?;
                tx.execute(
                    "DELETE FROM adjudication_job_inputs WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3",
                    params![scope.tenant_id, scope.user_id, job_id],
                )?;
                tx.execute(
                    "DELETE FROM adjudication_jobs WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                    params![scope.tenant_id, scope.user_id, job_id],
                )?;
            }
            let affected_dream_jobs: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT DISTINCT job_id FROM dream_job_inputs
                     WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
                )?;
                let rows =
                    stmt.query_map(params![scope.tenant_id, scope.user_id, eid], |r| r.get(0))?;
                rows.collect::<Result<Vec<String>, _>>()?
            };
            closure_ids.extend(affected_dream_jobs.iter().cloned());
            // Dream jobs are derived work records, not independent owners of L0.
            // Remove every batch containing this evidence so no frozen prompt or
            // retry can later reintroduce the purged source.
            tx.execute(
                "DELETE FROM dream_jobs WHERE tenant_id=?1 AND user_id=?2 AND id IN
                   (SELECT job_id FROM dream_job_inputs WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3)",
                params![scope.tenant_id, scope.user_id, eid],
            )?;
            tx.execute(
                "DELETE FROM dream_evidence_state WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
                params![scope.tenant_id, scope.user_id, eid],
            )?;
            // 3c. suppressed_sources 由墓碑替代（防 spool 重放复活）。
            tx.execute(
                "DELETE FROM suppressed_sources WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3 AND forgotten_memory_id=?4",
                params![scope.tenant_id, scope.user_id, eid, memory_id],
            )?;
            // 3c-bis. doc7（Riko-Muse）：rupture 事件随 L0 证据闭包删除（来源不可反查即
            // 不可留存）；随之清理失去全部 rupture 的线程与引用它们的 synthesis 版本
            // （synthesis 是纯派生物，与 pages 的 purge 同待遇）。
            tx.execute(
                "DELETE FROM rupture_events WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
                params![scope.tenant_id, scope.user_id, eid],
            )?;
            tx.execute(
                "DELETE FROM repair_threads WHERE tenant_id=?1 AND user_id=?2
                   AND id NOT IN (SELECT DISTINCT thread_id FROM rupture_events
                                  WHERE tenant_id=?1 AND user_id=?2 AND thread_id IS NOT NULL)",
                params![scope.tenant_id, scope.user_id],
            )?;
            tx.execute(
                "DELETE FROM alignment_synthesis WHERE tenant_id=?1 AND user_id=?2 AND (
                   EXISTS (SELECT 1 FROM json_each(alignment_synthesis.source_refs_json, '$.rupture_event_ids') je
                           WHERE je.value NOT IN (SELECT id FROM rupture_events
                                                  WHERE tenant_id=?1 AND user_id=?2))
                   OR EXISTS (SELECT 1 FROM json_each(alignment_synthesis.source_refs_json, '$.thread_ids') jt
                              WHERE jt.value NOT IN (SELECT id FROM repair_threads
                                                     WHERE tenant_id=?1 AND user_id=?2)))",
                params![scope.tenant_id, scope.user_id],
            )?;
            // 3d. 删事件本体 + 墓碑（source_id=content sha，防 spool 重放复活）。
            tx.execute(
                "DELETE FROM evidence_events WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, eid],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO purge_tombstones (tenant_id, user_id, source_kind, source_id, created_at)
                 VALUES (?1,?2,'evidence',?3,?4)",
                params![scope.tenant_id, scope.user_id, ev_sha, now],
            )?;
            deleted_evidence.push(eid.clone());
        }
        // 4. 来源含本记忆的 published 页面：归档 + 删索引/向量/来源行（不复活派生内容）。
        let page_ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT s.page_id FROM page_sources s
                 JOIN memory_pages pg ON pg.id=s.page_id AND pg.tenant_id=s.tenant_id AND pg.user_id=s.user_id
                 WHERE s.tenant_id=?1 AND s.user_id=?2 AND s.memory_id=?3",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, memory_id], |r| {
                r.get(0)
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        closure_ids.extend(page_ids.iter().cloned());
        for pid in &page_ids {
            tx.execute("DELETE FROM page_fts WHERE page_id=?1", params![pid])?;
            tx.execute(
                "DELETE FROM page_grams WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
                params![scope.tenant_id, scope.user_id, pid],
            )?;
            tx.execute(
                "DELETE FROM semantic_vectors WHERE tenant_id=?1 AND user_id=?2 AND object_kind='page' AND object_id=?3",
                params![scope.tenant_id, scope.user_id, pid],
            )?;
            tx.execute(
                "DELETE FROM semantic_jobs WHERE tenant_id=?1 AND user_id=?2 AND object_kind='page' AND object_id=?3",
                params![scope.tenant_id, scope.user_id, pid],
            )?;
            tx.execute(
                "DELETE FROM resident_page_pins WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
                params![scope.tenant_id, scope.user_id, pid],
            )?;
            tx.execute(
                "DELETE FROM page_revisions WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
                params![scope.tenant_id, scope.user_id, pid],
            )?;
            tx.execute(
                "DELETE FROM page_sources WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
                params![scope.tenant_id, scope.user_id, pid],
            )?;
            tx.execute(
                "DELETE FROM memory_pages WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, pid],
            )?;
        }
        // 5. 记忆本体依赖闭包。suppressed_sources 的 forgotten_memory_id FK 指向
        // 本记忆，无论其 evidence 是否共享都随闭包删除（防重放由墓碑承接）。
        tx.execute(
            "DELETE FROM suppressed_sources WHERE tenant_id=?1 AND user_id=?2 AND forgotten_memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        tx.execute(
            "DELETE FROM resident_pins WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        tx.execute(
            "DELETE FROM memory_relations WHERE tenant_id=?1 AND user_id=?2 AND (from_memory_id=?3 OR to_memory_id=?3)",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        tx.execute(
            "DELETE FROM memory_revisions WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        tx.execute(
            "DELETE FROM memory_evidence WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        tx.execute(
            "DELETE FROM memory_fts WHERE memory_id=?1",
            params![memory_id],
        )?;
        tx.execute(
            "DELETE FROM memory_grams WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        tx.execute(
            "DELETE FROM semantic_vectors WHERE tenant_id=?1 AND user_id=?2 AND object_kind='memory' AND object_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        tx.execute(
            "DELETE FROM memory_retirements WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        tx.execute(
            "DELETE FROM page_sources WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        // 6. 两张审计表及幂等回执中可精确反查闭包对象的行一并清理。
        // detail/response JSON 递归按完整字符串值匹配，避免 LIKE 子串误删相邻 ID。
        let audit_rows: Vec<(String, Option<String>, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, target_id, detail_json FROM audit_events WHERE tenant_id=?1 AND user_id=?2",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (audit_id, target_id, detail_json) in audit_rows {
            let detail_refs = serde_json::from_str::<serde_json::Value>(&detail_json)
                .is_ok_and(|v| json_references_any(&v, &closure_ids));
            if target_id
                .as_ref()
                .is_some_and(|id| closure_ids.contains(id))
                || detail_refs
            {
                tx.execute(
                    "DELETE FROM audit_events WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                    params![scope.tenant_id, scope.user_id, audit_id],
                )?;
            }
        }
        let metadata_rows: Vec<(String, String, Option<String>)> = {
            let mut stmt = tx.prepare(
                "SELECT audit_id, record_id, request_id FROM memory_audit WHERE tenant_id=?1 AND user_id=?2",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (audit_id, record_id, request_id) in metadata_rows {
            if closure_ids.contains(&record_id)
                || request_id
                    .as_ref()
                    .is_some_and(|id| closure_ids.contains(id))
            {
                tx.execute(
                    "DELETE FROM memory_audit WHERE tenant_id=?1 AND user_id=?2 AND audit_id=?3",
                    params![scope.tenant_id, scope.user_id, audit_id],
                )?;
            }
        }
        let receipts: Vec<(String, String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT operation, idempotency_key, response_json FROM mutation_receipts WHERE tenant_id=?1 AND user_id=?2",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (operation, key, response_json) in receipts {
            let response_refs = serde_json::from_str::<serde_json::Value>(&response_json)
                .is_ok_and(|v| json_references_any(&v, &closure_ids));
            if response_refs {
                tx.execute(
                    "DELETE FROM mutation_receipts WHERE tenant_id=?1 AND user_id=?2 AND operation=?3 AND idempotency_key=?4",
                    params![scope.tenant_id, scope.user_id, operation, key],
                )?;
            }
        }
        // 清除所有可定位目标的预览。手动确认保留当前消费行供同键幂等重放。
        match consumed_confirmation_sha {
            Some(token_sha) => {
                tx.execute(
                    "DELETE FROM purge_confirmations
                     WHERE tenant_id=?1 AND user_id=?2 AND target_id=?3 AND token_sha256<>?4",
                    params![scope.tenant_id, scope.user_id, memory_id, token_sha],
                )?;
                tx.execute(
                    "UPDATE purge_confirmations SET consumed=1, target_id=NULL, target_version=NULL,
                            token_sha256='', dependency_fingerprint=NULL
                     WHERE tenant_id=?1 AND user_id=?2 AND token_sha256=?3",
                    params![scope.tenant_id, scope.user_id, token_sha],
                )?;
            }
            None => {
                tx.execute(
                    "DELETE FROM purge_confirmations WHERE tenant_id=?1 AND user_id=?2 AND target_id=?3",
                    params![scope.tenant_id, scope.user_id, memory_id],
                )?;
            }
        }
        // 7. 记忆本体最后删（FK 依赖已清）。
        tx.execute(
            "DELETE FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        // 8. 记忆墓碑。
        tx.execute(
            "INSERT OR IGNORE INTO purge_tombstones (tenant_id, user_id, source_kind, source_id, created_at)
             VALUES (?1,?2,'memory',?3,?4)",
            params![scope.tenant_id, scope.user_id, memory_id, now],
        )?;
        Self::mark_index_dirty(tx)?;
        Ok(serde_json::json!({
            "evidence_deleted": deleted_evidence.len(),
            "pages_archived": page_ids.len(),
        }))
    }

    // ---- retention（doc6/12 §5：默认 0=关闭；正值即持续授权，无需逐批确认）----

    /// 设置策略（版本递增；历史版本留无正文策略值）。仅可信 CLI 调用。
    pub fn retention_set_policy(
        &mut self,
        scope: &ScopeKey,
        raw_evidence_retention_days: i64,
        expired_memory_purge_after_days: i64,
        enabled: bool,
    ) -> Result<i64, StoreError> {
        let now = now_rfc3339()?;
        let prev: Option<i64> = self
            .conn()
            .query_row(
                "SELECT policy_version FROM retention_policies WHERE tenant_id=?1 AND user_id=?2",
                params![scope.tenant_id, scope.user_id],
                |r| r.get(0),
            )
            .optional()?;
        let version = prev.unwrap_or(0) + 1;
        let tx = self.conn_mut().transaction()?;
        tx.execute(
            "INSERT INTO retention_policies
               (tenant_id, user_id, policy_version, effective_at, raw_evidence_retention_days,
                expired_memory_purge_after_days, enabled, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?4)
             ON CONFLICT (tenant_id, user_id) DO UPDATE SET
               policy_version=?3, effective_at=?4, raw_evidence_retention_days=?5,
               expired_memory_purge_after_days=?6, enabled=?7, updated_at=?4",
            params![
                scope.tenant_id,
                scope.user_id,
                version,
                now,
                raw_evidence_retention_days,
                expired_memory_purge_after_days,
                enabled as i64
            ],
        )?;
        tx.execute(
            "INSERT INTO retention_policy_history
               (tenant_id, user_id, policy_version, raw_evidence_retention_days,
                expired_memory_purge_after_days, enabled, effective_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                scope.tenant_id,
                scope.user_id,
                version,
                raw_evidence_retention_days,
                expired_memory_purge_after_days,
                enabled as i64,
                now
            ],
        )?;
        tx.commit()?;
        Ok(version)
    }

    /// 执行一轮 retention（无 LLM；复用 purge 闭包与墓碑；提交前复核当前策略版本）。
    /// 批次 fingerprint = (policy_version, cutoffs, 目标清单)；默认策略（0/关闭）无目标。
    pub fn retention_run(
        &mut self,
        scope: &ScopeKey,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        // Begin the write transaction before reading policy or selecting targets.
        // This prevents another process from attaching a new source or changing
        // the retention policy between the snapshot and physical deletion.
        let tx = self
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (version, effective_at, raw_days, expired_days, enabled): (i64, String, i64, i64, i64) = match tx
            .query_row(
                "SELECT policy_version,effective_at,raw_evidence_retention_days,expired_memory_purge_after_days,enabled
                 FROM retention_policies WHERE tenant_id=?1 AND user_id=?2",
                params![scope.tenant_id, scope.user_id],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
            )
            .optional()?
        {
            Some(v) => v,
            None => return Ok(None), // 未配置：默认关闭
        };
        if enabled == 0 || (raw_days == 0 && expired_days == 0) {
            return Ok(None); // 默认不删除
        }
        let now = now_rfc3339()?;
        let cutoff = |days: i64| -> Result<String, StoreError> {
            Ok((chrono::DateTime::parse_from_rfc3339(&now)
                .map_err(|e| StoreError::Time(e.to_string()))?
                - chrono::Duration::days(days))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
        };
        // 目标 1：过期超过 expired_days 的记忆（读时已过期的 active 行）。
        let mut targets: Vec<String> = Vec::new();
        if expired_days > 0 {
            let cutoff_mem = cutoff(expired_days)?;
            let mut stmt = tx.prepare(
                "SELECT id FROM memories
                 WHERE tenant_id=?1 AND user_id=?2 AND status='active'
                   AND created_at>=?4 AND valid_until IS NOT NULL AND valid_until < ?3
                   AND NOT EXISTS (
                     SELECT 1 FROM memory_evidence me
                     JOIN dream_job_inputs i ON i.tenant_id=me.tenant_id AND i.user_id=me.user_id
                       AND i.evidence_id=me.evidence_id
                     JOIN dream_jobs j ON j.tenant_id=i.tenant_id AND j.user_id=i.user_id AND j.id=i.job_id
                     WHERE me.tenant_id=memories.tenant_id AND me.user_id=memories.user_id
                       AND me.memory_id=memories.id
                       AND j.status IN ('queued','running','provider_wait','retryable_failed'))
                 ORDER BY id
                 LIMIT ?5",
            )?;
            let rows = stmt.query_map(
                params![
                    scope.tenant_id,
                    scope.user_id,
                    cutoff_mem,
                    effective_at,
                    RETENTION_BATCH_MAX_OBJECTS as i64
                ],
                |r| r.get(0),
            )?;
            targets.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        // 目标 2：过期超 expired_days 的非 active 记忆（superseded/expired 行同理物理清理）。
        if expired_days > 0 {
            let cutoff_mem = cutoff(expired_days)?;
            let mut stmt = tx.prepare(
                "SELECT id FROM memories
                 WHERE tenant_id=?1 AND user_id=?2 AND status IN ('superseded','expired')
                   AND created_at>=?4 AND updated_at < ?3
                   AND NOT EXISTS (
                     SELECT 1 FROM memory_evidence me
                     JOIN dream_job_inputs i ON i.tenant_id=me.tenant_id AND i.user_id=me.user_id
                       AND i.evidence_id=me.evidence_id
                     JOIN dream_jobs j ON j.tenant_id=i.tenant_id AND j.user_id=i.user_id AND j.id=i.job_id
                     WHERE me.tenant_id=memories.tenant_id AND me.user_id=memories.user_id
                       AND me.memory_id=memories.id
                       AND j.status IN ('queued','running','provider_wait','retryable_failed'))
                 ORDER BY id
                 LIMIT ?5",
            )?;
            let rows = stmt.query_map(
                params![
                    scope.tenant_id,
                    scope.user_id,
                    cutoff_mem,
                    effective_at,
                    RETENTION_BATCH_MAX_OBJECTS.saturating_sub(targets.len()) as i64
                ],
                |r| r.get(0),
            )?;
            targets.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        // 目标 3：按 received_at（记录时间）和策略生效时间选旧 user evidence。
        // assigned 或仍被可恢复 job 使用的证据不能进入物理清理。
        let mut raw_evidence: Vec<(String, String)> = Vec::new(); // (id, content sha)
        if raw_days > 0 {
            let cutoff_ev = cutoff(raw_days)?;
            let mut stmt = tx.prepare(
                "SELECT e.id, e.content_sha256 FROM evidence_events e
                 WHERE e.tenant_id=?1 AND e.user_id=?2 AND e.role='user' AND e.source_kind='user'
                   AND e.received_at>=?4 AND e.received_at < ?3
                   AND NOT EXISTS (SELECT 1 FROM dream_evidence_state s
                     LEFT JOIN dream_jobs j ON j.tenant_id=s.tenant_id AND j.user_id=s.user_id
                       AND j.id=s.active_job_id
                     WHERE s.tenant_id=e.tenant_id AND s.user_id=e.user_id AND s.evidence_id=e.id
                       AND s.status='assigned'
                       AND (j.id IS NULL OR j.status IN ('queued','running','provider_wait','retryable_failed')))
                   AND NOT EXISTS (SELECT 1 FROM dream_job_inputs i JOIN dream_jobs j
                     ON j.tenant_id=i.tenant_id AND j.user_id=i.user_id AND j.id=i.job_id
                     WHERE i.tenant_id=e.tenant_id AND i.user_id=e.user_id AND i.evidence_id=e.id
                       AND j.status IN ('queued','running','provider_wait','retryable_failed'))
                 ORDER BY e.received_at, e.id
                 LIMIT ?5",
            )?;
            let rows = stmt.query_map(
                params![
                    scope.tenant_id,
                    scope.user_id,
                    cutoff_ev,
                    effective_at,
                    RETENTION_BATCH_MAX_OBJECTS as i64
                ],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?;
            raw_evidence.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        raw_evidence.sort();
        raw_evidence.dedup();
        let expiring_evidence: HashSet<String> =
            raw_evidence.iter().map(|(id, _)| id.clone()).collect();
        // Expiring a source that is the last valid support for an active memory
        // purges that memory and its derived pages. Otherwise only the expired
        // source relation is removed and the memory remains supported.
        for (evidence_id, _) in &raw_evidence {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT m.id FROM memory_evidence me JOIN memories m
                 ON m.tenant_id=me.tenant_id AND m.user_id=me.user_id AND m.id=me.memory_id
                 WHERE me.tenant_id=?1 AND me.user_id=?2 AND me.evidence_id=?3 AND m.status='active'
                   AND (m.valid_until IS NULL OR m.valid_until>?4)
                   AND NOT EXISTS (SELECT 1 FROM memory_retirements r
                     WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)",
            )?;
            let linked = stmt
                .query_map(
                    params![scope.tenant_id, scope.user_id, evidence_id, now],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            for memory_id in linked {
                let other_sources: Vec<String> = {
                    let mut stmt=tx.prepare(
                    "SELECT e.id FROM memory_evidence me JOIN evidence_events e
                     ON e.tenant_id=me.tenant_id AND e.user_id=me.user_id AND e.id=me.evidence_id
                     WHERE me.tenant_id=?1 AND me.user_id=?2 AND me.memory_id=?3 AND me.evidence_id<>?4
                       AND e.role='user' AND e.source_kind='user'
                       AND NOT EXISTS(SELECT 1 FROM suppressed_sources ss WHERE ss.tenant_id=e.tenant_id
                         AND ss.user_id=e.user_id AND ss.evidence_id=e.id)
                       AND NOT EXISTS(SELECT 1 FROM purge_tombstones pt WHERE pt.tenant_id=e.tenant_id
                         AND pt.user_id=e.user_id AND pt.source_kind='evidence' AND pt.source_id=e.content_sha256)")?;
                    let rows = stmt.query_map(
                        params![scope.tenant_id, scope.user_id, memory_id, evidence_id],
                        |r| r.get::<_, String>(0),
                    )?;
                    rows.collect::<Result<Vec<_>, _>>()?
                };
                let has_surviving_source = other_sources
                    .iter()
                    .any(|source_id| !expiring_evidence.contains(source_id));
                if !has_surviving_source {
                    targets.push(memory_id);
                }
            }
        }
        targets.sort();
        targets.dedup();
        if targets.is_empty() && raw_evidence.is_empty() {
            return Ok(None);
        }
        // 批次 fingerprint 幂等。
        let mut fp_src: Vec<String> = targets.clone();
        fp_src.extend(raw_evidence.iter().map(|(id, _)| id.clone()));
        fp_src.push(format!("v{version}"));
        fp_src.sort();
        let fingerprint = sha256_hex(&fp_src.join("\u{0}"));
        let existing: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM retention_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND policy_version=?3 AND batch_fingerprint=?4
                   AND status='succeeded'",
                params![scope.tenant_id, scope.user_id, version, fingerprint],
                |r| r.get(0),
            )
            .optional()?;
        if existing.is_some() {
            return Ok(None); // 同批次已执行
        }
        // Revalidate the full policy inside the locked transaction immediately
        // before applying the selected deletion closure.
        let current: (i64, String, i64, i64, i64) = tx.query_row(
            "SELECT policy_version,effective_at,raw_evidence_retention_days,
                    expired_memory_purge_after_days,enabled
             FROM retention_policies WHERE tenant_id=?1 AND user_id=?2",
            params![scope.tenant_id, scope.user_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        if current
            != (
                version,
                effective_at.clone(),
                raw_days,
                expired_days,
                enabled,
            )
        {
            return Err(StoreError::StateConflict);
        }
        // Same fingerprint can be observed concurrently by two schedulers; the
        // unique key plus this in-transaction check makes it a no-op.
        let already_done: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM retention_jobs WHERE tenant_id=?1 AND user_id=?2
             AND policy_version=?3 AND batch_fingerprint=?4 AND status='succeeded' LIMIT 1",
                params![scope.tenant_id, scope.user_id, version, fingerprint],
                |r| r.get(0),
            )
            .optional()?;
        if already_done.is_some() {
            tx.commit()?;
            return Ok(None);
        }
        let job_id = Uuid::now_v7().to_string();
        tx.execute(
            "INSERT INTO retention_jobs
               (id,tenant_id,user_id,policy_version,batch_fingerprint,status,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,'running',?6,?6)",
            params![
                job_id,
                scope.tenant_id,
                scope.user_id,
                version,
                fingerprint,
                now
            ],
        )?;
        let mut purged = 0usize;
        for mid in &targets {
            let exists: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                    params![scope.tenant_id, scope.user_id, mid],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_some() {
                Self::execute_purge_tx(&tx, scope, mid, None)?;
                purged += 1;
            }
        }
        let mut raw_deleted = 0usize;
        let target_set: HashSet<String> = targets.iter().cloned().collect();
        let mut detached_audits: Vec<(String, i64)> = Vec::new();
        for (eid, ev_sha) in &raw_evidence {
            let exists:bool=tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM evidence_events WHERE tenant_id=?1 AND user_id=?2 AND id=?3)",
                params![scope.tenant_id,scope.user_id,eid],|r|r.get(0))?;
            if !exists {
                raw_deleted += 1;
                continue;
            }
            // Remove expired source links. Memories with another valid source
            // survive; unique-source active memories were included in targets.
            let linked: Vec<(String, i64)> = {
                let mut stmt = tx.prepare(
                    "SELECT DISTINCT m.id,m.version FROM memory_evidence me JOIN memories m
                     ON m.tenant_id=me.tenant_id AND m.user_id=me.user_id AND m.id=me.memory_id
                     WHERE me.tenant_id=?1 AND me.user_id=?2 AND me.evidence_id=?3",
                )?;
                let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, eid], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for (memory_id, version) in linked {
                if !target_set.contains(&memory_id) {
                    tx.execute("DELETE FROM memory_evidence WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3 AND evidence_id=?4",
                        params![scope.tenant_id,scope.user_id,memory_id,eid])?;
                    detached_audits.push((memory_id, version));
                }
            }
            let job_ids: Vec<String> = {
                let mut stmt=tx.prepare("SELECT DISTINCT job_id FROM dream_job_inputs WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3")?;
                let rows =
                    stmt.query_map(params![scope.tenant_id, scope.user_id, eid], |r| r.get(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            let mut closure_ids: HashSet<String> = HashSet::from([eid.clone(), ev_sha.clone()]);
            closure_ids.extend(job_ids.iter().cloned());
            let candidates: Vec<String> = {
                let mut stmt=tx.prepare("SELECT DISTINCT candidate_id FROM dream_candidate_evidence WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3")?;
                let rows =
                    stmt.query_map(params![scope.tenant_id, scope.user_id, eid], |r| r.get(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            closure_ids.extend(candidates);
            for job_id in job_ids {
                tx.execute(
                    "DELETE FROM dream_jobs WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                    params![scope.tenant_id, scope.user_id, job_id],
                )?;
            }
            tx.execute("DELETE FROM dream_candidate_redecisions WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
                params![scope.tenant_id,scope.user_id,eid])?;
            tx.execute("DELETE FROM dream_candidate_evidence WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
                params![scope.tenant_id,scope.user_id,eid])?;
            tx.execute("DELETE FROM dream_evidence_state WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
                params![scope.tenant_id,scope.user_id,eid])?;
            tx.execute("DELETE FROM suppressed_sources WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
                params![scope.tenant_id,scope.user_id,eid])?;
            // Clear audit rows whose exact target/detail can identify this L0 closure.
            let audit_rows: Vec<(String, Option<String>, String)> = {
                let mut stmt=tx.prepare("SELECT id,target_id,detail_json FROM audit_events WHERE tenant_id=?1 AND user_id=?2")?;
                let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for (audit_id, target, detail) in audit_rows {
                let detail_ref = serde_json::from_str::<serde_json::Value>(&detail)
                    .is_ok_and(|v| json_references_any(&v, &closure_ids));
                if target.as_ref().is_some_and(|id| closure_ids.contains(id)) || detail_ref {
                    tx.execute(
                        "DELETE FROM audit_events WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                        params![scope.tenant_id, scope.user_id, audit_id],
                    )?;
                }
            }
            for id in &closure_ids {
                tx.execute(
                    "DELETE FROM memory_audit WHERE tenant_id=?1 AND user_id=?2 AND record_id=?3",
                    params![scope.tenant_id, scope.user_id, id],
                )?;
            }
            tx.execute(
                "DELETE FROM evidence_events WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, eid],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO purge_tombstones (tenant_id, user_id, source_kind, source_id, created_at)
                 VALUES (?1,?2,'evidence',?3,?4)",
                params![scope.tenant_id, scope.user_id, ev_sha, now],
            )?;
            raw_deleted += 1;
        }
        let counts =
            serde_json::json!({"memories_purged":purged,"raw_evidence_deleted":raw_deleted});
        tx.execute(
            "UPDATE retention_jobs SET status='succeeded',deleted_counts_json=?2,updated_at=?3 WHERE id=?1 AND status='running'",
            params![job_id,counts.to_string(),now_rfc3339()?],
        )?;
        tx.commit()?;
        for (memory_id, version) in detached_audits {
            self.record_memory_audit_best_effort(
                scope,
                &MemoryAuditEntry {
                    record_id: memory_id,
                    layer: AuditLayer::L1,
                    action: AuditAction::Update,
                    agent_id: None,
                    task_id: Some(job_id.clone()),
                    version,
                    updated_at_ms: chrono::Utc::now().timestamp_millis(),
                    request_id: None,
                },
            );
        }
        Ok(Some(serde_json::json!({
            "policy_version": version,
            "memories_purged": purged,
            "raw_evidence_deleted": raw_deleted,
        })))
    }
}

fn json_references_any(value: &serde_json::Value, ids: &HashSet<String>) -> bool {
    match value {
        serde_json::Value::String(s) => ids.contains(s),
        serde_json::Value::Array(items) => items.iter().any(|item| json_references_any(item, ids)),
        serde_json::Value::Object(fields) => {
            fields.values().any(|item| json_references_any(item, ids))
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

fn preview_version(store: &Store, scope: &ScopeKey, memory_id: &str) -> Result<i64, StoreError> {
    store
        .conn()
        .query_row(
            "SELECT version FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
            |r| r.get(0),
        )
        .map_err(|_| StoreError::MemoryNotFound)
}
