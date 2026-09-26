//! D6-9 purge 两阶段执行与 retention（doc6/02 §7、doc6/12 §5）。
//!
//! preview 只读业务记忆、只写确认元数据；confirm 原子消费 token 并在同一
//! SQLite 事务内执行依赖闭包（首版单事务有界闭包：单条记忆及其独占依赖；
//! 超出单事务安全范围的多对象批量清理由 retention 逐对象复用同一闭包）。
//! 完成后清除 confirmation 与 job 中可反查目标的 ID；两张审计表匹配行随闭包
//! 删除；删 evidence 时以 purge_tombstones 防止 spool 重放复活。

use memory_domain::ScopeKey;
use rusqlite::{params, OptionalExtension};
use sha2::Digest;
use uuid::Uuid;

use crate::{lifecycle::sha256_hex, now_rfc3339, Store, StoreError};

/// confirmation 有效期（短期一次性）。
const CONFIRMATION_TTL_SECS: i64 = 900;

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
                scope.tenant_id, scope.user_id, sha256_hex(&token),
                memory_id, target_version,
                fingerprint, idempotency_key, expires, now
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
        let Some((target, _tver, frozen_fp, expires_at, consumed)) = row else {
            // 同幂等键已消费：无正文结果（不重复执行，不泄露目标）。
            let replayed: i64 = self.conn().query_row(
                "SELECT COUNT(*) FROM purge_confirmations
                 WHERE tenant_id=?1 AND user_id=?2 AND idempotency_key=?3 AND consumed=1",
                params![scope.tenant_id, scope.user_id, idempotency_key],
                |r| r.get(0),
            )?;
            if replayed > 0 {
                return Ok(PurgeOutcome { job_id: String::new(), deleted: serde_json::json!({"replayed": true}) });
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
        let deleted = Self::execute_purge_tx(&tx, scope, &target)?;
        tx.execute(
            "UPDATE purge_confirmations SET consumed=1, target_id=NULL, token_sha256='', dependency_fingerprint=''
             WHERE tenant_id=?1 AND user_id=?2 AND token_sha256=?3",
            params![scope.tenant_id, scope.user_id, token_sha],
        )?;
        tx.execute(
            "INSERT INTO purge_jobs
               (id, tenant_id, user_id, operation, target_id, dependency_fingerprint, status,
                attempts, deleted_counts_json, created_at, updated_at)
             VALUES (?1,?2,?3,'purge_memory',?4,?5,'succeeded',1,?6,?7,?7)",
            params![job_id, scope.tenant_id, scope.user_id, target, frozen_fp,
                    deleted.to_string(), now],
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
    pub fn purge_closure(&self, scope: &ScopeKey, memory_id: &str) -> Result<PurgePreview, StoreError> {
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
        let mut p = PurgePreview { memory_id: memory_id.to_string(), ..Default::default() };
        let mut ids: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT evidence_id FROM memory_evidence WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, memory_id], |r| r.get(0))?;
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
                 WHERE s.tenant_id=?1 AND s.user_id=?2 AND s.memory_id=?3 AND pg.status='published'",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, memory_id], |r| r.get(0))?;
            p.page_ids = rows.collect::<Result<Vec<_>, _>>()?;
            p.page_ids.sort();
        }
        // 旧候选（quote/claim 闭包）：以本记忆 evidence 为来源的旧 memory_candidates。
        if !p.evidence_ids.is_empty() {
            let placeholders = p.evidence_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT COUNT(*) FROM memory_candidates WHERE tenant_id=?1 AND user_id=?2 AND primary_evidence_id IN ({placeholders})"
            );
            let mut bind: Vec<&dyn rusqlite::ToSql> =
                vec![&scope.tenant_id, &scope.user_id];
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
                       + (SELECT COUNT(*) FROM suppressed_sources WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3)
                       + (SELECT COUNT(*) FROM dream_job_inputs WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3)",
                params![scope.tenant_id, scope.user_id, eid, memory_id],
                |r| r.get(0),
            )?;
            if shared > 0 {
                continue; // 共享 evidence 不删本体（其他对象仍引用）
            }
            // 3a. 旧候选闭包：primary_evidence 指向该事件的 memory_candidates（quote/claim）。
            let old_candidates: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT id FROM memory_candidates WHERE tenant_id=?1 AND user_id=?2 AND primary_evidence_id=?3",
                )?;
                let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, eid], |r| r.get(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
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
                let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, eid], |r| r.get(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
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
            // 3c. suppressed_sources 由墓碑替代（防 spool 重放复活）。
            tx.execute(
                "DELETE FROM suppressed_sources WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
                params![scope.tenant_id, scope.user_id, eid],
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
                 WHERE s.tenant_id=?1 AND s.user_id=?2 AND s.memory_id=?3 AND pg.status='published'",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, memory_id], |r| r.get(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for pid in &page_ids {
            tx.execute(
                "UPDATE memory_pages SET status='archived', updated_at=?4
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, pid, now],
            )?;
            tx.execute(
                "DELETE FROM page_fts WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
                params![scope.tenant_id, scope.user_id, pid],
            )?;
            tx.execute(
                "DELETE FROM page_grams WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
                params![scope.tenant_id, scope.user_id, pid],
            )?;
            tx.execute(
                "DELETE FROM semantic_vectors WHERE tenant_id=?1 AND user_id=?2 AND object_kind='page' AND object_id=?3",
                params![scope.tenant_id, scope.user_id, pid],
            )?;
        }
        // 5. 记忆本体依赖闭包。
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
        // 6. 两张审计表匹配行随闭包删除（doc6/02 §7）。
        tx.execute(
            "DELETE FROM audit_events WHERE tenant_id=?1 AND user_id=?2 AND target_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        tx.execute(
            "DELETE FROM memory_audit WHERE tenant_id=?1 AND user_id=?2 AND record_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        // mutation_receipts 中可反查闭包目标的响应一并删除。
        tx.execute(
            "DELETE FROM mutation_receipts WHERE tenant_id=?1 AND user_id=?2 AND response_json LIKE ?3",
            params![scope.tenant_id, scope.user_id, format!("%{memory_id}%")],
        )?;
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
            params![scope.tenant_id, scope.user_id, version, now,
                    raw_evidence_retention_days, expired_memory_purge_after_days, enabled as i64],
        )?;
        tx.execute(
            "INSERT INTO retention_policy_history
               (tenant_id, user_id, policy_version, raw_evidence_retention_days,
                expired_memory_purge_after_days, enabled, effective_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![scope.tenant_id, scope.user_id, version,
                    raw_evidence_retention_days, expired_memory_purge_after_days, enabled as i64, now],
        )?;
        tx.commit()?;
        Ok(version)
    }

    /// 执行一轮 retention（无 LLM；复用 purge 闭包与墓碑；提交前复核当前策略版本）。
    /// 批次 fingerprint = (policy_version, cutoffs, 目标清单)；默认策略（0/关闭）无目标。
    pub fn retention_run(&mut self, scope: &ScopeKey) -> Result<Option<serde_json::Value>, StoreError> {
        let (version, raw_days, expired_days, enabled): (i64, i64, i64, i64) = match self
            .conn()
            .query_row(
                "SELECT policy_version, raw_evidence_retention_days, expired_memory_purge_after_days, enabled
                 FROM retention_policies WHERE tenant_id=?1 AND user_id=?2",
                params![scope.tenant_id, scope.user_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
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
            let mut stmt = self.conn().prepare(
                "SELECT id FROM memories
                 WHERE tenant_id=?1 AND user_id=?2 AND status='active'
                   AND valid_until IS NOT NULL AND valid_until < ?3",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, cutoff_mem], |r| r.get(0))?;
            targets.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        // 目标 2：过期超 expired_days 的非 active 记忆（superseded/expired 行同理物理清理）。
        if expired_days > 0 {
            let cutoff_mem = cutoff(expired_days)?;
            let mut stmt = self.conn().prepare(
                "SELECT id FROM memories
                 WHERE tenant_id=?1 AND user_id=?2 AND status IN ('superseded','expired')
                   AND updated_at < ?3",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, cutoff_mem], |r| r.get(0))?;
            targets.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        targets.sort();
        targets.dedup();
        // 目标 3：raw evidence（已 processed 且无任何记忆引用的旧 user 事件）。
        let mut raw_evidence: Vec<(String, String)> = Vec::new(); // (id, content sha)
        if raw_days > 0 {
            let cutoff_ev = cutoff(raw_days)?;
            let mut stmt = self.conn().prepare(
                "SELECT e.id, e.content_sha256 FROM evidence_events e
                 WHERE e.tenant_id=?1 AND e.user_id=?2 AND e.occurred_at < ?3
                   AND e.id NOT IN (SELECT evidence_id FROM memory_evidence WHERE tenant_id=?1 AND user_id=?2)
                   AND e.id NOT IN (SELECT evidence_id FROM dream_job_inputs WHERE tenant_id=?1 AND user_id=?2)",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, cutoff_ev], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            raw_evidence.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        raw_evidence.sort();
        raw_evidence.dedup();
        if targets.is_empty() && raw_evidence.is_empty() {
            return Ok(None);
        }
        // 批次 fingerprint 幂等。
        let mut fp_src: Vec<String> = targets.clone();
        fp_src.extend(raw_evidence.iter().map(|(id, _)| id.clone()));
        fp_src.push(format!("v{version}"));
        fp_src.sort();
        let fingerprint = sha256_hex(&fp_src.join("\u{0}"));
        let existing: Option<i64> = self
            .conn()
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
        let job_id = Uuid::now_v7().to_string();
        self.conn_mut().execute(
            "INSERT INTO retention_jobs
               (id, tenant_id, user_id, policy_version, batch_fingerprint, status, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,'running',?6,?6)",
            params![job_id, scope.tenant_id, scope.user_id, version, fingerprint, now],
        )?;
        // 提交删除前再次读取当前 policy version（策略取消/收窄 → 旧 job 终止）。
        let cur_version: i64 = self.conn().query_row(
            "SELECT policy_version FROM retention_policies WHERE tenant_id=?1 AND user_id=?2",
            params![scope.tenant_id, scope.user_id],
            |r| r.get(0),
        )?;
        if cur_version != version {
            self.conn_mut().execute(
                "UPDATE retention_jobs SET status='failed', error_code='POLICY_CHANGED' WHERE id=?1",
                params![job_id],
            )?;
            return Err(StoreError::StateConflict);
        }
        // 执行闭包（逐对象；同事务）。
        let tx = self.conn_mut().transaction()?;
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
                Self::execute_purge_tx(&tx, scope, mid)?;
                purged += 1;
            }
        }
        let mut raw_deleted = 0usize;
        for (eid, ev_sha) in &raw_evidence {
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
        tx.commit()?;
        self.conn_mut().execute(
            "UPDATE retention_jobs SET status='succeeded', deleted_counts_json=?3, updated_at=?4 WHERE id=?1 AND status='running'",
            params![job_id, version,
                    serde_json::json!({"memories_purged": purged, "raw_evidence_deleted": raw_deleted}).to_string(),
                    now_rfc3339()?],
        )?;
        Ok(Some(serde_json::json!({
            "policy_version": version,
            "memories_purged": purged,
            "raw_evidence_deleted": raw_deleted,
        })))
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
