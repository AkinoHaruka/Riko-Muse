//! Dream child read capabilities (D6-12): scope/job/generation/runner checked,
//! snapshot the exact active targets observed, and never mutate memory truth.

use memory_domain::{DomainScope, ScopeKey};
use rusqlite::{params, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};

use crate::{now_rfc3339, Store, StoreError};

pub const DREAM_READ_CALL_BUDGET: i64 = 32;

#[derive(Debug, Clone)]
pub struct DreamReadManifest {
    pub job_id: String,
    pub purpose: String,
    pub status: String,
    pub generation: i64,
    pub input_fingerprint: String,
    pub evidence: Vec<(String, String, i64)>,
}

#[derive(Debug, Clone)]
pub struct DreamEvidence {
    pub evidence_id: String,
    pub role: String,
    pub event_seq: i64,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct DreamMemoryTarget {
    pub memory_id: String,
    pub version: i64,
    pub kind: String,
    pub claim: String,
}

fn owned(
    tx: &Transaction<'_>,
    scope: &ScopeKey,
    runner_id: &str,
    job_id: &str,
    generation: i64,
    now: &str,
) -> Result<bool, StoreError> {
    tx.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM dream_jobs j JOIN dream_runners r
             ON r.tenant_id=j.tenant_id AND r.user_id=j.user_id AND r.runner_id=j.runner_id
           WHERE j.tenant_id=?1 AND j.user_id=?2 AND j.id=?3 AND j.runner_id=?4
             AND j.status='running' AND j.claim_generation=?5
             AND j.lease_until>?6 AND r.lease_until>?6)",
        params![
            scope.tenant_id,
            scope.user_id,
            job_id,
            runner_id,
            generation,
            now
        ],
        |r| r.get(0),
    )
    .map_err(Into::into)
}

fn require_owned(
    tx: &Transaction<'_>,
    scope: &ScopeKey,
    runner_id: &str,
    job_id: &str,
    generation: i64,
    now: &str,
) -> Result<(), StoreError> {
    if owned(tx, scope, runner_id, job_id, generation, now)? {
        Ok(())
    } else {
        Err(StoreError::StaleClaim)
    }
}

fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

fn page_fingerprint(
    title: &str,
    description: &str,
    body: &str,
    generator: &str,
    input: &str,
) -> String {
    sha256_hex(&format!(
        "{title}\0{description}\0{body}\0{generator}\0{input}"
    ))
}

impl Store {
    /// Atomically spend one bounded read request after validating the live lease.
    pub fn dream_consume_read_budget(
        &mut self,
        scope: &ScopeKey,
        runner_id: &str,
        job_id: &str,
        generation: i64,
    ) -> Result<(), StoreError> {
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let changed = tx.execute(
            "UPDATE dream_jobs SET read_calls=read_calls+1
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND runner_id=?4
               AND status='running' AND claim_generation=?5 AND lease_until>?6
               AND read_calls<read_budget AND EXISTS (
                 SELECT 1 FROM dream_runners r WHERE r.tenant_id=dream_jobs.tenant_id
                   AND r.user_id=dream_jobs.user_id AND r.runner_id=dream_jobs.runner_id
                   AND r.lease_until>?6)",
            params![
                scope.tenant_id,
                scope.user_id,
                job_id,
                runner_id,
                generation,
                now
            ],
        )?;
        if changed == 0 {
            require_owned(&tx, scope, runner_id, job_id, generation, &now)?;
            return Err(StoreError::DreamReadBudgetExceeded);
        }
        tx.commit()?;
        Ok(())
    }

    pub fn dream_record_search(
        &mut self,
        scope: &ScopeKey,
        runner_id: &str,
        job_id: &str,
        generation: i64,
        operation: &str,
        candidate_id: Option<&str>,
        query: &str,
        semantic_complete: bool,
        result_count: usize,
    ) -> Result<(), StoreError> {
        if !matches!(operation, "memory" | "page")
            || query.trim().is_empty()
            || result_count > 10
            || candidate_id.is_some_and(|id| id.is_empty() || id.len() > 128)
            || (operation == "page" && candidate_id.is_some())
        {
            return Err(StoreError::InvalidPageField);
        }
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        require_owned(&tx, scope, runner_id, job_id, generation, &now)?;
        if let Some(candidate_id) = candidate_id {
            let belongs: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM adjudication_jobs a JOIN adjudication_job_inputs i
                   ON i.tenant_id=a.tenant_id AND i.user_id=a.user_id AND i.job_id=a.id
                 WHERE a.tenant_id=?1 AND a.user_id=?2 AND a.dream_job_id=?3 AND i.candidate_id=?4)",
                params![scope.tenant_id,scope.user_id,job_id,candidate_id],|r|r.get(0))?;
            if !belongs {
                return Err(StoreError::EvidenceNotFound);
            }
        }
        let document_key_match = if operation == "page" {
            let key: Option<String> = tx.query_row(
                "SELECT c.document_key FROM dream_consolidation_links l JOIN consolidation_jobs c
                   ON c.tenant_id=l.tenant_id AND c.user_id=l.user_id AND c.id=l.consolidation_job_id
                 WHERE l.tenant_id=?1 AND l.user_id=?2 AND l.dream_job_id=?3",
                params![scope.tenant_id,scope.user_id,job_id],|r|r.get(0)).optional()?;
            key.is_some_and(|key| query.to_lowercase().contains(&key.to_lowercase()))
        } else {
            false
        };
        let query_hash = sha256_hex(&query.trim().to_lowercase());
        tx.execute(
            "INSERT INTO dream_read_search_receipts
              (tenant_id,user_id,dream_job_id,generation,operation,candidate_id,query_sha256,
               semantic_complete,document_key_match,result_count,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(tenant_id,user_id,dream_job_id,generation,operation,candidate_id,query_sha256)
             DO UPDATE SET semantic_complete=MAX(semantic_complete,excluded.semantic_complete),
               document_key_match=MAX(document_key_match,excluded.document_key_match),
               result_count=excluded.result_count,created_at=excluded.created_at",
            params![scope.tenant_id,scope.user_id,job_id,generation,operation,candidate_id.unwrap_or(""),query_hash,
                semantic_complete,document_key_match,result_count as i64,now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn dream_page_search_ready(
        &self,
        scope: &ScopeKey,
        job_id: &str,
        generation: i64,
        document_key: &str,
        dom: &DomainScope,
    ) -> Result<bool, StoreError> {
        let search_complete: bool = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM dream_read_search_receipts
             WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND generation=?4
               AND operation='page' AND semantic_complete=1 AND document_key_match=1
               AND ?5=(SELECT c.document_key FROM dream_consolidation_links l JOIN consolidation_jobs c
                 ON c.tenant_id=l.tenant_id AND c.user_id=l.user_id AND c.id=l.consolidation_job_id
                 WHERE l.tenant_id=?1 AND l.user_id=?2 AND l.dream_job_id=?3))",
            params![scope.tenant_id,scope.user_id,job_id,generation,document_key],|r|r.get(0),
        )?;
        if !search_complete {
            return Ok(false);
        }
        let page: Option<(String, i64)> = self
            .conn()
            .query_row(
                "SELECT id,version FROM memory_pages WHERE tenant_id=?1 AND user_id=?2
             AND document_kind='topic_page' AND document_key=?3 AND status='published'",
                params![scope.tenant_id, scope.user_id, document_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((page_id, version)) = page {
            let now = now_rfc3339()?;
            if self.get_page(scope, &page_id, &now, dom)?.is_some() {
                return self
                    .conn()
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM dream_read_page_targets
                     WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND generation=?4
                       AND page_id=?5 AND page_version=?6 AND detail_read=1)",
                        params![
                            scope.tenant_id,
                            scope.user_id,
                            job_id,
                            generation,
                            page_id,
                            version
                        ],
                        |r| r.get(0),
                    )
                    .map_err(Into::into);
            }
        }
        Ok(true)
    }

    pub fn dream_read_manifest(
        &self,
        scope: &ScopeKey,
        runner_id: &str,
        job_id: &str,
        generation: i64,
    ) -> Result<DreamReadManifest, StoreError> {
        let now = now_rfc3339()?;
        let tx = self.conn().unchecked_transaction()?;
        require_owned(&tx, scope, runner_id, job_id, generation, &now)?;
        let (purpose, status, current_generation, input_fingerprint): (
            String,
            String,
            i64,
            String,
        ) = tx
            .query_row(
                "SELECT purpose,status,claim_generation,input_fingerprint FROM dream_jobs
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, job_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or(StoreError::JobNotFound)?;
        let mut stmt = tx.prepare(
            "SELECT i.evidence_id,i.role,i.event_seq,i.content_sha256,e.content_sha256,e.content
             FROM dream_job_inputs i JOIN evidence_events e
               ON e.tenant_id=i.tenant_id AND e.user_id=i.user_id AND e.id=i.evidence_id
             WHERE i.tenant_id=?1 AND i.user_id=?2 AND i.job_id=?3
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
                       WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id AND pj.target_id=m.id
                         AND pj.status IN ('pending','running'))))
             ORDER BY i.input_order LIMIT 80",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, job_id, now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        let mut evidence = Vec::new();
        for row in rows {
            let (id, role, seq, frozen_hash, stored_hash, content) = row?;
            if frozen_hash != stored_hash || sha256_hex(&content) != frozen_hash {
                return Err(StoreError::StaleInput);
            }
            evidence.push((id, role, seq));
        }
        drop(stmt);
        tx.commit()?;
        Ok(DreamReadManifest {
            job_id: job_id.to_string(),
            purpose,
            status,
            generation: current_generation,
            input_fingerprint,
            evidence,
        })
    }

    /// Read only IDs present in this job's frozen input. Suppressed, purged,
    /// retired, expired, or otherwise inactive source evidence is omitted.
    pub fn dream_read_evidence(
        &self,
        scope: &ScopeKey,
        runner_id: &str,
        job_id: &str,
        generation: i64,
        evidence_ids: &[String],
    ) -> Result<Vec<DreamEvidence>, StoreError> {
        if evidence_ids.is_empty() || evidence_ids.len() > 20 {
            return Err(StoreError::InvalidPageField);
        }
        let now = now_rfc3339()?;
        let tx = self.conn().unchecked_transaction()?;
        require_owned(&tx, scope, runner_id, job_id, generation, &now)?;
        for id in evidence_ids {
            let frozen: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM dream_job_inputs WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3 AND evidence_id=?4)",
                params![scope.tenant_id, scope.user_id, job_id, id],
                |r| r.get(0),
            )?;
            if !frozen {
                return Err(StoreError::EvidenceNotFound);
            }
        }
        let placeholders = vec!["?"; evidence_ids.len()].join(",");
        let sql = format!(
            "SELECT i.evidence_id,i.role,i.event_seq,i.content_sha256,e.content_sha256,e.content
             FROM dream_job_inputs i JOIN evidence_events e
               ON e.tenant_id=i.tenant_id AND e.user_id=i.user_id AND e.id=i.evidence_id
             WHERE i.tenant_id=?1 AND i.user_id=?2 AND i.job_id=?3
               AND i.evidence_id IN ({placeholders})
               AND NOT EXISTS (SELECT 1 FROM suppressed_sources ss
                 WHERE ss.tenant_id=e.tenant_id AND ss.user_id=e.user_id AND ss.evidence_id=e.id)
               AND NOT EXISTS (SELECT 1 FROM purge_tombstones pt
                 WHERE pt.tenant_id=e.tenant_id AND pt.user_id=e.user_id
                   AND pt.source_kind='evidence' AND pt.source_id=e.content_sha256)
               AND NOT EXISTS (SELECT 1 FROM memory_evidence me
                 JOIN memories m ON m.tenant_id=me.tenant_id AND m.user_id=me.user_id AND m.id=me.memory_id
                 WHERE me.tenant_id=e.tenant_id AND me.user_id=e.user_id AND me.evidence_id=e.id
                   AND (m.status<>'active' OR (m.valid_until IS NOT NULL AND m.valid_until<=?{})
                     OR EXISTS (SELECT 1 FROM memory_retirements r
                       WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)
                     OR EXISTS (SELECT 1 FROM purge_jobs pj
                       WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id AND pj.target_id=m.id
                         AND pj.status IN ('pending','running'))))
             ORDER BY i.input_order LIMIT 20",
            evidence_ids.len() + 4
        );
        let mut values: Vec<String> = vec![
            scope.tenant_id.clone(),
            scope.user_id.clone(),
            job_id.to_string(),
        ];
        values.extend(evidence_ids.iter().cloned());
        values.push(now);
        let mut stmt = tx.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(values.iter().map(|v| v as &dyn rusqlite::ToSql)),
            |r| {
                Ok((
                    DreamEvidence {
                        evidence_id: r.get(0)?,
                        role: r.get(1)?,
                        event_seq: r.get(2)?,
                        content: r.get(5)?,
                    },
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )?;
        let mut result = Vec::new();
        for row in rows {
            let (evidence, frozen_hash, stored_hash) = row?;
            if frozen_hash != stored_hash || sha256_hex(&evidence.content) != frozen_hash {
                return Err(StoreError::StaleInput);
            }
            result.push(evidence);
        }
        drop(stmt);
        // An empty/short response is valid when some requested sources became
        // unavailable after the trigger; the child must not cite them.
        tx.commit()?;
        Ok(result)
    }

    /// Snapshot only current active memories that exactly match the result seen
    /// by the search path. A later version change makes detail reads stale.
    pub fn dream_snapshot_memories(
        &mut self,
        scope: &ScopeKey,
        runner_id: &str,
        job_id: &str,
        generation: i64,
        candidates: &[(String, i64, String)],
    ) -> Result<Vec<String>, StoreError> {
        if candidates.len() > 20 {
            return Err(StoreError::InvalidPageField);
        }
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        require_owned(&tx, scope, runner_id, job_id, generation, &now)?;
        let mut accepted = Vec::new();
        for (id, expected_version, expected_claim) in candidates {
            let current: Option<(i64, String, String)> = tx
                .query_row(
                    "SELECT version,claim_sha256,claim FROM memories m
                     WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='active'
                       AND (valid_until IS NULL OR valid_until>?4)
                       AND NOT EXISTS (SELECT 1 FROM memory_retirements r
                         WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)
                       AND NOT EXISTS (SELECT 1 FROM purge_jobs pj
                         WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id AND pj.target_id=m.id
                           AND pj.status IN ('pending','running'))",
                    params![scope.tenant_id, scope.user_id, id, now],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((version, hash, claim)) = current else {
                continue;
            };
            if version != *expected_version || claim != *expected_claim {
                continue;
            }
            let prior: Option<(i64, String)> = tx
                .query_row(
                    "SELECT memory_version,claim_sha256 FROM dream_read_memory_targets
                     WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND generation=?4 AND memory_id=?5",
                    params![scope.tenant_id, scope.user_id, job_id, generation, id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if prior
                .as_ref()
                .is_some_and(|p| p != &(version, hash.clone()))
            {
                return Err(StoreError::StaleInput);
            }
            tx.execute(
                "INSERT OR IGNORE INTO dream_read_memory_targets
                 (tenant_id,user_id,dream_job_id,generation,memory_id,memory_version,claim_sha256,created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![scope.tenant_id, scope.user_id, job_id, generation, id, version, hash, now],
            )?;
            accepted.push(id.clone());
        }
        let total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM dream_read_memory_targets WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND generation=?4",
            params![scope.tenant_id, scope.user_id, job_id, generation],
            |r| r.get(0),
        )?;
        if total > 100 {
            return Err(StoreError::InvalidPageField);
        }
        tx.commit()?;
        Ok(accepted)
    }

    pub fn dream_snapshot_pages(
        &mut self,
        scope: &ScopeKey,
        runner_id: &str,
        job_id: &str,
        generation: i64,
        candidates: &[(String, i64)],
    ) -> Result<Vec<String>, StoreError> {
        if candidates.len() > 20 {
            return Err(StoreError::InvalidPageField);
        }
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        require_owned(&tx, scope, runner_id, job_id, generation, &now)?;
        let mut accepted = Vec::new();
        for (id, expected_version) in candidates {
            let row: Option<(i64, String, String, String, String, String)> = tx
                .query_row(
                    "SELECT p.version,p.title,p.description,p.body_md,p.generator_version,p.input_fingerprint
                     FROM memory_pages p WHERE p.tenant_id=?1 AND p.user_id=?2 AND p.id=?3 AND p.status='published'
                       AND EXISTS (SELECT 1 FROM page_sources ps WHERE ps.tenant_id=p.tenant_id AND ps.user_id=p.user_id AND ps.page_id=p.id)
                       AND NOT EXISTS (SELECT 1 FROM page_sources ps
                         JOIN memories m ON m.tenant_id=ps.tenant_id AND m.user_id=ps.user_id AND m.id=ps.memory_id
                         WHERE ps.tenant_id=p.tenant_id AND ps.user_id=p.user_id AND ps.page_id=p.id
                           AND (m.status<>'active' OR m.version<>ps.memory_version OR m.claim_sha256<>ps.claim_sha256
                             OR (m.valid_until IS NOT NULL AND m.valid_until<=?4)
                             OR EXISTS (SELECT 1 FROM memory_retirements r
                               WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)
                             OR EXISTS (SELECT 1 FROM purge_jobs pj WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id
                               AND pj.target_id=m.id AND pj.status IN ('pending','running'))))",
                    params![scope.tenant_id, scope.user_id, id, now],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .optional()?;
            let Some((version, title, description, body, generator, input)) = row else {
                continue;
            };
            if version != *expected_version {
                continue;
            }
            let fingerprint = page_fingerprint(&title, &description, &body, &generator, &input);
            let prior: Option<(i64, String)> = tx
                .query_row(
                    "SELECT page_version,page_fingerprint FROM dream_read_page_targets
                     WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND generation=?4 AND page_id=?5",
                    params![scope.tenant_id, scope.user_id, job_id, generation, id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if prior
                .as_ref()
                .is_some_and(|p| p != &(version, fingerprint.clone()))
            {
                return Err(StoreError::StaleInput);
            }
            tx.execute(
                "INSERT OR IGNORE INTO dream_read_page_targets
                 (tenant_id,user_id,dream_job_id,generation,page_id,page_version,page_fingerprint,created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![scope.tenant_id, scope.user_id, job_id, generation, id, version, fingerprint, now],
            )?;
            accepted.push(id.clone());
        }
        let total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM dream_read_page_targets WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND generation=?4",
            params![scope.tenant_id, scope.user_id, job_id, generation],
            |r| r.get(0),
        )?;
        if total > 100 {
            return Err(StoreError::InvalidPageField);
        }
        tx.commit()?;
        Ok(accepted)
    }

    pub fn dream_memory_snapshot_valid(
        &self,
        scope: &ScopeKey,
        runner_id: &str,
        job_id: &str,
        generation: i64,
        memory_id: &str,
    ) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let tx = self.conn().unchecked_transaction()?;
        require_owned(&tx, scope, runner_id, job_id, generation, &now)?;
        let valid: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM dream_read_memory_targets t JOIN memories m
               ON m.tenant_id=t.tenant_id AND m.user_id=t.user_id AND m.id=t.memory_id
             WHERE t.tenant_id=?1 AND t.user_id=?2 AND t.dream_job_id=?3 AND t.generation=?4
               AND t.memory_id=?5 AND t.memory_version=m.version AND t.claim_sha256=m.claim_sha256
               AND m.status='active' AND (m.valid_until IS NULL OR m.valid_until>?6)
               AND NOT EXISTS (SELECT 1 FROM memory_retirements r
                 WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)
               AND NOT EXISTS (SELECT 1 FROM purge_jobs pj WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id
                 AND pj.target_id=m.id AND pj.status IN ('pending','running')))",
            params![scope.tenant_id, scope.user_id, job_id, generation, memory_id, now],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(valid)
    }

    pub fn dream_page_snapshot_valid(
        &self,
        scope: &ScopeKey,
        runner_id: &str,
        job_id: &str,
        generation: i64,
        page_id: &str,
        page_version: i64,
    ) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let tx = self.conn().unchecked_transaction()?;
        require_owned(&tx, scope, runner_id, job_id, generation, &now)?;
        let row: Option<(i64, String, String, String, String, String, bool)> = tx.query_row(
            "SELECT p.version,p.title,p.description,p.body_md,p.generator_version,p.input_fingerprint,
               (EXISTS (SELECT 1 FROM page_sources ps WHERE ps.tenant_id=p.tenant_id AND ps.user_id=p.user_id AND ps.page_id=p.id)
                AND NOT EXISTS (SELECT 1 FROM page_sources ps JOIN memories m
                  ON m.tenant_id=ps.tenant_id AND m.user_id=ps.user_id AND m.id=ps.memory_id
                  WHERE ps.tenant_id=p.tenant_id AND ps.user_id=p.user_id AND ps.page_id=p.id
                    AND (m.status<>'active' OR m.version<>ps.memory_version OR m.claim_sha256<>ps.claim_sha256
                      OR (m.valid_until IS NOT NULL AND m.valid_until<=?6)
                      OR EXISTS (SELECT 1 FROM memory_retirements r
                        WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)
                      OR EXISTS (SELECT 1 FROM purge_jobs pj WHERE pj.tenant_id=m.tenant_id AND pj.user_id=m.user_id
                        AND pj.target_id=m.id AND pj.status IN ('pending','running')))))
             FROM dream_read_page_targets t JOIN memory_pages p
               ON p.tenant_id=t.tenant_id AND p.user_id=t.user_id AND p.id=t.page_id
             WHERE t.tenant_id=?1 AND t.user_id=?2 AND t.dream_job_id=?3 AND t.generation=?4
               AND t.page_id=?5 AND t.page_version=?7 AND p.status='published'",
            params![scope.tenant_id, scope.user_id, job_id, generation, page_id, now, page_version],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        ).optional()?;
        let valid = row.is_some_and(|(version, title, description, body, generator, input, sources_valid)| {
            version == page_version
                && sources_valid
                && tx.query_row(
                    "SELECT page_fingerprint FROM dream_read_page_targets
                     WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND generation=?4 AND page_id=?5",
                    params![scope.tenant_id, scope.user_id, job_id, generation, page_id],
                    |r| r.get::<_, String>(0),
                ).is_ok_and(|saved| saved == page_fingerprint(&title, &description, &body, &generator, &input))
        });
        tx.commit()?;
        Ok(valid)
    }

    /// Read page detail only from the snapshot previously returned by search.
    /// Validation and body read share one SQLite transaction to avoid TOCTOU.
    pub fn dream_read_page_detail(
        &self,
        scope: &ScopeKey,
        runner_id: &str,
        job_id: &str,
        generation: i64,
        page_id: &str,
    ) -> Result<Option<crate::pages::PageRow>, StoreError> {
        let now = now_rfc3339()?;
        let tx = self.conn().unchecked_transaction()?;
        require_owned(&tx, scope, runner_id, job_id, generation, &now)?;
        let page: Option<crate::pages::PageRow> = tx.query_row(
            "SELECT p.id,p.document_kind,p.document_key,p.question_version,p.question_text,
                    p.title,p.description,p.body_md,p.status,p.version,p.generator_version,p.input_fingerprint,p.updated_at,
                    t.page_version,t.page_fingerprint
             FROM dream_read_page_targets t JOIN memory_pages p
               ON p.tenant_id=t.tenant_id AND p.user_id=t.user_id AND p.id=t.page_id
             WHERE t.tenant_id=?1 AND t.user_id=?2 AND t.dream_job_id=?3 AND t.generation=?4 AND t.page_id=?5",
            params![scope.tenant_id, scope.user_id, job_id, generation, page_id],
            |r| {
                let row = crate::pages::PageRow {
                    page_id: r.get(0)?, document_kind: r.get(1)?, document_key: r.get(2)?,
                    question_version: r.get(3)?, question_text: r.get(4)?, title: r.get(5)?,
                    description: r.get(6)?, body_md: r.get(7)?, status: r.get(8)?, version: r.get(9)?,
                    generator_version: r.get(10)?, input_fingerprint: r.get(11)?,
                    updated_at: r.get(12)?, sources: Vec::new(),
                };
                Ok((row, r.get::<_, i64>(13)?, r.get::<_, String>(14)?))
            },
        ).optional()?.and_then(|(row, snapshot_version, fingerprint)| {
            (row.version == snapshot_version
                && row.status == "published"
                && fingerprint == page_fingerprint(&row.title, &row.description, &row.body_md, &row.generator_version, &row.input_fingerprint))
                .then_some(row)
        });
        let Some(mut page) = page else {
            tx.commit()?;
            return Ok(None);
        };
        let mut stmt = tx.prepare(
            "SELECT ps.memory_id,ps.memory_version,ps.claim_sha256,m.status,m.version,m.claim_sha256,m.valid_until
             FROM page_sources ps JOIN memories m
               ON m.tenant_id=ps.tenant_id AND m.user_id=ps.user_id AND m.id=ps.memory_id
             WHERE ps.tenant_id=?1 AND ps.user_id=?2 AND ps.page_id=?3",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, page_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let rows = rows.collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (id, version, hash, status, current_version, current_hash, valid_until) in rows {
            let retired: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_retirements WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3)",
                params![scope.tenant_id,scope.user_id,id],|r|r.get(0))?;
            let purging: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM purge_jobs WHERE tenant_id=?1 AND user_id=?2 AND target_id=?3 AND status IN ('pending','running'))",
                params![scope.tenant_id,scope.user_id,id],|r|r.get(0))?;
            if status != "active"
                || version != current_version
                || hash != current_hash
                || retired
                || purging
                || valid_until
                    .as_deref()
                    .is_some_and(|until| until <= now.as_str())
            {
                tx.commit()?;
                return Ok(None);
            }
            page.sources.push((id, version));
        }
        if page.sources.is_empty() {
            tx.commit()?;
            return Ok(None);
        }
        tx.execute(
            "UPDATE dream_read_page_targets SET detail_read=1
             WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND generation=?4 AND page_id=?5",
            params![scope.tenant_id,scope.user_id,job_id,generation,page_id],
        )?;
        tx.commit()?;
        Ok(Some(page))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RememberOutcome, Store};
    use memory_domain::{MemoryKind, Origin, ScopeKey};

    fn setup(tag: &str) -> (Store, ScopeKey, Origin, String, String, String, i64) {
        let migrations = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("migrations");
        let mut store = Store::open_in_memory(&migrations).unwrap();
        let temp = std::env::temp_dir().join(format!(
            "dream-read-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&temp).unwrap();
        let token_path = temp.join("user.token");
        store.principal_add("tenant", "user", &token_path).unwrap();
        let token = std::fs::read_to_string(token_path).unwrap();
        let scope = store.verify_token(token.trim()).unwrap().unwrap();
        let origin = Origin {
            host_id: "dsh".into(),
            agent_id: "agent-a".into(),
            session_id: format!("session-{tag}"),
        };
        let now = chrono::Utc::now();
        let remembered_evidence = match store
            .record_evidence(
                &scope,
                &origin,
                1,
                "user",
                "user",
                &now,
                "我偏好简洁回答",
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        };
        let memory_id = match store
            .remember(
                &scope,
                &origin,
                &remembered_evidence,
                "我偏好简洁回答",
                MemoryKind::Preference,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            RememberOutcome::Created { memory_id, .. }
            | RememberOutcome::Dedup { memory_id, .. } => memory_id,
        };
        let frozen_evidence = match store
            .record_evidence(
                &scope,
                &origin,
                2,
                "user",
                "user",
                &now,
                "我正在整理一套记忆系统",
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        };
        let job = store
            .dream_trigger(
                &scope,
                "manual",
                &format!("trigger-{tag}"),
                Some("agent-a"),
                None,
                None,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
            .unwrap();
        store
            .dream_runner_heartbeat(
                &scope,
                "runner-a",
                "dsh",
                "agent-a",
                r#"["chat","dream_scoped_read_v1"]"#,
                120,
            )
            .unwrap();
        let claim = store
            .dream_runner_claim(&scope, "runner-a", &now_rfc3339().unwrap(), 120)
            .unwrap()
            .unwrap();
        assert_eq!(claim.dream_job.id, job.id);
        (
            store,
            scope,
            origin,
            remembered_evidence,
            frozen_evidence,
            memory_id,
            claim.dream_job.claim_generation,
        )
    }

    #[test]
    fn dream_read_is_scope_bound_frozen_and_filters_retired_sources() {
        let (mut store, scope, origin, remembered_evidence, frozen_evidence, memory_id, generation) =
            setup("scope");
        let job_id: String = store
            .conn()
            .query_row(
                "SELECT id FROM dream_jobs WHERE trigger_key='trigger-scope'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        let manifest = store
            .dream_read_manifest(&scope, "runner-a", &job_id, generation)
            .unwrap();
        assert!(manifest
            .evidence
            .iter()
            .any(|(id, _, _)| id == &remembered_evidence));
        let read = store
            .dream_read_evidence(
                &scope,
                "runner-a",
                &job_id,
                generation,
                &[frozen_evidence.clone()],
            )
            .unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].content, "我正在整理一套记忆系统");
        assert!(matches!(
            store.dream_read_evidence(
                &scope,
                "runner-a",
                &job_id,
                generation,
                &["not-frozen".into()]
            ),
            Err(StoreError::EvidenceNotFound)
        ));

        let other_token_path =
            std::env::temp_dir().join(format!("dream-read-other-{}.token", uuid::Uuid::now_v7()));
        store
            .principal_add("tenant", "other-user", &other_token_path)
            .unwrap();
        let other_token = std::fs::read_to_string(other_token_path).unwrap();
        let other_scope = store.verify_token(other_token.trim()).unwrap().unwrap();
        assert!(matches!(
            store.dream_read_manifest(&other_scope, "runner-a", &job_id, generation),
            Err(StoreError::StaleClaim)
        ));

        // Once the only L1 derived from an input is forgotten, that source must
        // disappear from both manifest and content reads for the running child.
        store
            .conn_mut()
            .execute(
                "UPDATE memories SET status='forgotten' WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
            )
            .unwrap();
        let manifest = store
            .dream_read_manifest(&scope, "runner-a", &job_id, generation)
            .unwrap();
        assert!(!manifest
            .evidence
            .iter()
            .any(|(id, _, _)| id == &remembered_evidence));
        assert!(store
            .dream_read_evidence(
                &scope,
                "runner-a",
                &job_id,
                generation,
                &[remembered_evidence]
            )
            .unwrap()
            .is_empty());
        let _ = origin;
    }

    #[test]
    fn dream_read_budget_and_generation_are_enforced() {
        let (mut store, scope, _origin, _, _, _, generation) = setup("budget");
        let job_id: String = store
            .conn()
            .query_row(
                "SELECT id FROM dream_jobs WHERE trigger_key='trigger-budget'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(matches!(
            store.dream_consume_read_budget(&scope, "runner-a", &job_id, generation + 1),
            Err(StoreError::StaleClaim)
        ));
        for _ in 0..DREAM_READ_CALL_BUDGET {
            store
                .dream_consume_read_budget(&scope, "runner-a", &job_id, generation)
                .unwrap();
        }
        assert!(matches!(
            store.dream_consume_read_budget(&scope, "runner-a", &job_id, generation),
            Err(StoreError::DreamReadBudgetExceeded)
        ));
        let used: i64 = store
            .conn()
            .query_row(
                "SELECT read_calls FROM dream_jobs WHERE id=?1",
                [&job_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(used, DREAM_READ_CALL_BUDGET);
    }

    #[test]
    fn dream_snapshot_rejects_changed_memory_version() {
        let (mut store, scope, _origin, _, _, memory_id, generation) = setup("version");
        let job_id: String = store
            .conn()
            .query_row(
                "SELECT id FROM dream_jobs WHERE trigger_key='trigger-version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let (version, claim): (i64, String) = store
            .conn()
            .query_row(
                "SELECT version,claim FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            store
                .dream_snapshot_memories(
                    &scope,
                    "runner-a",
                    &job_id,
                    generation,
                    &[(memory_id.clone(), version, claim)],
                )
                .unwrap(),
            vec![memory_id.clone()]
        );
        assert!(store
            .dream_memory_snapshot_valid(&scope, "runner-a", &job_id, generation, &memory_id)
            .unwrap());
        store
            .conn_mut()
            .execute(
                "UPDATE memories SET version=version+1 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
            )
            .unwrap();
        assert!(!store
            .dream_memory_snapshot_valid(&scope, "runner-a", &job_id, generation, &memory_id)
            .unwrap());
    }

    #[test]
    fn dream_manifest_rejects_mutated_frozen_evidence_content() {
        let (mut store, scope, _origin, _, frozen_evidence, _, generation) =
            setup("evidence-drift");
        let job_id: String = store
            .conn()
            .query_row(
                "SELECT id FROM dream_jobs WHERE trigger_key='trigger-evidence-drift'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        store
            .conn_mut()
            .execute(
                "UPDATE evidence_events SET content='内容被篡改' WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                rusqlite::params![scope.tenant_id, scope.user_id, frozen_evidence],
            )
            .unwrap();
        assert!(matches!(
            store.dream_read_manifest(&scope, "runner-a", &job_id, generation),
            Err(StoreError::StaleInput)
        ));
        assert!(matches!(
            store.dream_read_evidence(&scope, "runner-a", &job_id, generation, &[frozen_evidence]),
            Err(StoreError::StaleInput)
        ));
    }
}
