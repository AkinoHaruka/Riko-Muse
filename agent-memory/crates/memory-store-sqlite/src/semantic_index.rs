//! D6-8 语义向量索引（doc6/02 §4、doc6/04 §2）：版本化向量缓存 + 异步索引队列。
//!
//! 只比较相同 `model_id + dimensions` 的向量；BLOB 为有限非 NaN 的 f32 小端数组。
//! 对象状态/版本变化由业务事务同事务置 stale（correct/forget/归档），新建/更新
//! 由服务端入队异步重算；索引失败不影响 L1 与 resident 的使用（doc6/02 §4）。

use memory_domain::ScopeKey;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

/// 对象内容哈希（索引冻结值；与 claim_sha256 分开，页面正文也用它）。
pub(crate) fn content_sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

/// 向量文本规范化版本（v1：normalize_v1 后原文；页面加标题行。变更须记录新约定）。
pub fn semantic_index_text(kind: &str, title: Option<&str>, body: &str) -> String {
    let normalized = memory_domain::normalize_v1(body);
    match kind {
        "page" => match title {
            Some(t) => format!("{}\n{}", memory_domain::normalize_v1(t), normalized),
            None => normalized,
        },
        _ => normalized,
    }
}

/// 索引作业行。
#[derive(Debug, Clone)]
pub struct SemanticJobRow {
    pub id: String,
    pub object_kind: String,
    pub object_id: String,
    pub source_version: i64,
    pub content_sha256: String,
    pub model_id: String,
    pub status: String,
    pub attempts: i64,
    pub run_after: String,
    pub claim_generation: i64,
    pub error_code: Option<String>,
}

fn map_semantic_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<SemanticJobRow> {
    Ok(SemanticJobRow {
        id: r.get(0)?,
        object_kind: r.get(1)?,
        object_id: r.get(2)?,
        source_version: r.get(3)?,
        content_sha256: r.get(4)?,
        model_id: r.get(5)?,
        status: r.get(6)?,
        attempts: r.get(7)?,
        run_after: r.get(8)?,
        claim_generation: r.get(9)?,
        error_code: r.get(10)?,
    })
}

const SEMANTIC_JOB_COLS: &str = "id, object_kind, object_id, source_version, content_sha256,
    model_id, status, attempts, run_after, claim_generation, error_code";

impl Store {
    /// 入队异步索引（doc6/02 §4：新建/更新 L1、发布页面后由服务端调用）。
    /// 读取对象当前 (version, content hash)；对象无效（非 active/published）返回 None。
    /// 同对象同模型已有待处理作业时幂等复用并刷新冻结版本（不重复索引）。
    pub fn semantic_enqueue(
        &mut self,
        scope: &ScopeKey,
        object_kind: &str,
        object_id: &str,
        model_id: &str,
    ) -> Result<Option<SemanticJobRow>, StoreError> {
        let Some((version, content_sha)) =
            self.semantic_object_fingerprint(scope, object_kind, object_id)?
        else {
            return Ok(None);
        };
        let now = now_rfc3339()?;
        let id = Uuid::now_v7().to_string();
        let n = self.conn_mut().execute(
            "INSERT INTO semantic_jobs
               (id, tenant_id, user_id, object_kind, object_id, source_version, content_sha256,
                model_id, status, attempts, run_after, claim_generation, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'queued',0,?9,0,?9,?9)
             ON CONFLICT DO NOTHING",
            params![
                id,
                scope.tenant_id,
                scope.user_id,
                object_kind,
                object_id,
                version,
                content_sha,
                model_id,
                now
            ],
        )?;
        if n == 0 {
            self.conn_mut().execute(
                "UPDATE semantic_jobs SET source_version=?6, content_sha256=?7, updated_at=?8
                 WHERE tenant_id=?1 AND user_id=?2 AND object_kind=?3 AND object_id=?4 AND model_id=?5
                   AND status IN ('queued','retryable_failed','provider_wait')",
                params![scope.tenant_id, scope.user_id, object_kind, object_id, model_id,
                        version, content_sha, now],
            )?;
        }
        let job = self
            .conn()
            .query_row(
                &format!(
                    "SELECT {SEMANTIC_JOB_COLS} FROM semantic_jobs
                     WHERE tenant_id=?1 AND user_id=?2 AND object_kind=?3 AND object_id=?4 AND model_id=?5
                       AND status IN ('queued','retryable_failed','provider_wait')"
                ),
                params![scope.tenant_id, scope.user_id, object_kind, object_id, model_id],
                map_semantic_job,
            )
            .optional()?;
        Ok(job)
    }

    /// 对象当前指纹：memory 须 active（version, claim_sha256）；page 须 published
    /// （version, sha256(body_md)）。其余状态返回 None。
    pub fn semantic_object_fingerprint(
        &self,
        scope: &ScopeKey,
        object_kind: &str,
        object_id: &str,
    ) -> Result<Option<(i64, String)>, StoreError> {
        let now = now_rfc3339()?;
        match object_kind {
            "memory" => self
                .conn()
                .query_row(
                    "SELECT version, claim_sha256 FROM memories
                     WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='active'
                       AND (valid_until IS NULL OR valid_until>?4)
                       AND NOT EXISTS (SELECT 1 FROM memory_retirements r
                         WHERE r.tenant_id=memories.tenant_id AND r.user_id=memories.user_id AND r.memory_id=memories.id)",
                    params![scope.tenant_id, scope.user_id, object_id, now],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(Into::into),
            "page" => {
                let row: Option<(i64, String)> = self
                    .conn()
                    .query_row(
                        "SELECT version, body_md FROM memory_pages
                         WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='published'
                           AND EXISTS (SELECT 1 FROM page_sources ps
                             WHERE ps.tenant_id=memory_pages.tenant_id AND ps.user_id=memory_pages.user_id AND ps.page_id=memory_pages.id)
                           AND NOT EXISTS (SELECT 1 FROM page_sources ps
                             JOIN memories m ON m.tenant_id=ps.tenant_id AND m.user_id=ps.user_id AND m.id=ps.memory_id
                             WHERE ps.tenant_id=memory_pages.tenant_id AND ps.user_id=memory_pages.user_id AND ps.page_id=memory_pages.id
                               AND (m.status<>'active' OR m.version<>ps.memory_version OR m.claim_sha256<>ps.claim_sha256
                                 OR (m.valid_until IS NOT NULL AND m.valid_until<=?4)
                                 OR EXISTS (SELECT 1 FROM memory_retirements r
                                   WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)))",
                        params![scope.tenant_id, scope.user_id, object_id, now],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                Ok(row.map(|(v, body)| (v, content_sha256_hex(&body))))
            }
            _ => Err(StoreError::StateConflict),
        }
    }

    /// 保存向量（worker 用）。核对象当前版本/哈希与作业冻结值一致；不一致返回
    /// StaleInput（作业按 stale_input 收尾，doc6/02 §4 查询前核版本）。
    pub fn semantic_vector_save(
        &mut self,
        scope: &ScopeKey,
        object_kind: &str,
        object_id: &str,
        model_id: &str,
        source_version: i64,
        content_sha256: &str,
        vector: &[f32],
    ) -> Result<(), StoreError> {
        if vector.is_empty() || !vector.iter().all(|v| v.is_finite()) {
            return Err(StoreError::InvalidPageField);
        }
        let Some((cur_version, cur_sha)) =
            self.semantic_object_fingerprint(scope, object_kind, object_id)?
        else {
            return Err(StoreError::StaleInput);
        };
        if cur_version != source_version || cur_sha != content_sha256 {
            return Err(StoreError::StaleInput);
        }
        let dims = vector.len() as i64;
        let mut blob = Vec::with_capacity(vector.len() * 4);
        for v in vector {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        let now = now_rfc3339()?;
        self.conn_mut().execute(
            "INSERT INTO semantic_vectors
               (tenant_id, user_id, object_kind, object_id, model_id, source_version,
                content_sha256, dimensions, vector_blob, status, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'ready',?10,?10)
             ON CONFLICT (tenant_id, user_id, object_kind, object_id, model_id)
             DO UPDATE SET source_version=?6, content_sha256=?7, dimensions=?8,
                           vector_blob=?9, status='ready', updated_at=?10",
            params![
                scope.tenant_id,
                scope.user_id,
                object_kind,
                object_id,
                model_id,
                source_version,
                content_sha256,
                dims,
                blob,
                now
            ],
        )?;
        Ok(())
    }

    /// 同事务置 stale（doc6/02 §4：correct/forget/归档与向量失效同一规范事务）。
    /// 同时作废待处理索引作业（对象已非 active/published，索引无意义）。
    pub fn stale_vectors_in_tx(
        tx: &rusqlite::Transaction<'_>,
        scope: &ScopeKey,
        object_kind: &str,
        object_id: &str,
    ) -> Result<(), StoreError> {
        let now = now_rfc3339()?;
        tx.execute(
            "UPDATE semantic_vectors SET status='stale', updated_at=?5
             WHERE tenant_id=?1 AND user_id=?2 AND object_kind=?3 AND object_id=?4",
            params![scope.tenant_id, scope.user_id, object_kind, object_id, now],
        )?;
        tx.execute(
            "UPDATE semantic_jobs SET status='stale_input', updated_at=?5
             WHERE tenant_id=?1 AND user_id=?2 AND object_kind=?3 AND object_id=?4
               AND status IN ('queued','retryable_failed','provider_wait')",
            params![scope.tenant_id, scope.user_id, object_kind, object_id, now],
        )?;
        Ok(())
    }

    /// scope 内有界近邻扫描（doc6/04 §2）：只对 ready 且同 model_id/dimensions 的
    /// 向量做余弦；返回 (hits, ready 总数)。对象生命周期有效性（active/published/
    /// 有效期/来源复核）由调用方对每个命中重新核验，不信任缓存向量。
    /// ready 总数达到 SEMANTIC_SCAN_LIMIT 上限时调用方须将语义支路标记
    /// limit_exceeded 并只信词法（doc6/04 §2）。
    pub fn semantic_scan(
        &self,
        scope: &ScopeKey,
        object_kind: &str,
        model_id: &str,
        query: &[f32],
        top_k: usize,
    ) -> Result<(Vec<(String, f32)>, usize), StoreError> {
        if query.is_empty() || !query.iter().all(|v| v.is_finite()) {
            return Err(StoreError::InvalidPageField);
        }
        let dims = query.len() as i64;
        let mut stmt = self.conn().prepare(
            "SELECT object_id, vector_blob FROM semantic_vectors
             WHERE tenant_id=?1 AND user_id=?2 AND object_kind=?3 AND model_id=?4
               AND status='ready' AND dimensions=?5
             LIMIT ?6",
        )?;
        let rows = stmt.query_map(
            params![
                scope.tenant_id,
                scope.user_id,
                object_kind,
                model_id,
                dims,
                memory_contract::SEMANTIC_SCAN_LIMIT as i64
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )?;
        let qnorm: f32 = query.iter().map(|v| v * v).sum::<f32>().sqrt();
        let mut hits: Vec<(String, f32)> = Vec::new();
        let mut ready_count = 0usize;
        for row in rows {
            let (id, blob) = row?;
            ready_count += 1;
            if let Ok(sim) = decode_cosine(&blob, query, qnorm) {
                hits.push((id, sim));
            }
        }
        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(top_k);
        Ok((hits, ready_count))
    }

    /// 为全部 active 记忆与 published 页面补建索引队列（CLI reindex-semantic，
    /// doc6/02 §4）。幂等：已有待处理作业只刷新冻结版本。
    pub fn semantic_reindex_all(&mut self, model_id: &str) -> Result<usize, StoreError> {
        let scopes: Vec<(String, String)> = {
            let mut stmt = self
                .conn()
                .prepare("SELECT tenant_id, user_id FROM principals WHERE status='active'")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut n = 0usize;
        let now = now_rfc3339()?;
        for (tenant, user) in scopes {
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let mem_ids: Vec<String> = {
                let mut stmt = self.conn().prepare(
                    "SELECT id FROM memories m WHERE tenant_id=?1 AND user_id=?2 AND status='active'
                       AND (valid_until IS NULL OR valid_until>?3)
                       AND NOT EXISTS (SELECT 1 FROM memory_retirements r
                         WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)",
                )?;
                let rows =
                    stmt.query_map(params![scope.tenant_id, scope.user_id, now], |r| r.get(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            let page_ids: Vec<String> = {
                let mut stmt = self.conn().prepare(
                    "SELECT p.id FROM memory_pages p WHERE p.tenant_id=?1 AND p.user_id=?2 AND p.status='published'
                       AND EXISTS (SELECT 1 FROM page_sources ps WHERE ps.tenant_id=p.tenant_id AND ps.user_id=p.user_id AND ps.page_id=p.id)
                       AND NOT EXISTS (SELECT 1 FROM page_sources ps
                         JOIN memories m ON m.tenant_id=ps.tenant_id AND m.user_id=ps.user_id AND m.id=ps.memory_id
                         WHERE ps.tenant_id=p.tenant_id AND ps.user_id=p.user_id AND ps.page_id=p.id
                           AND (m.status<>'active' OR m.version<>ps.memory_version OR m.claim_sha256<>ps.claim_sha256
                             OR (m.valid_until IS NOT NULL AND m.valid_until<=?3)
                             OR EXISTS (SELECT 1 FROM memory_retirements r
                               WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)))",
                )?;
                let rows =
                    stmt.query_map(params![scope.tenant_id, scope.user_id, now], |r| r.get(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for mid in mem_ids {
                if self
                    .semantic_enqueue(&scope, "memory", &mid, model_id)?
                    .is_some()
                {
                    n += 1;
                }
            }
            for pid in page_ids {
                if self
                    .semantic_enqueue(&scope, "page", &pid, model_id)?
                    .is_some()
                {
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// 领取索引作业（跨 scope；内置 worker 单循环，doc4 claim/lease/generation）。
    /// 返回 (scope, 行)。
    pub fn semantic_job_claim(
        &mut self,
        now: &str,
        lease_secs: u64,
    ) -> Result<Option<(ScopeKey, SemanticJobRow)>, StoreError> {
        let next: Option<(String, i64, String, String)> = self
            .conn()
            .query_row(
                "SELECT id, claim_generation, tenant_id, user_id FROM semantic_jobs
                 WHERE status='queued' AND run_after<=?1
                 ORDER BY run_after, created_at LIMIT 1",
                params![now],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((id, generation, tenant, user)) = next else {
            return Ok(None);
        };
        let lease = lease_from(now, lease_secs)?;
        self.conn_mut().execute(
            "UPDATE semantic_jobs SET status='running', lease_until=?4, claim_generation=?5,
                    attempts=attempts+1, updated_at=?6
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='queued'",
            params![tenant, user, id, lease, generation + 1, now_rfc3339()?],
        )?;
        let scope = ScopeKey {
            tenant_id: tenant,
            user_id: user,
        };
        let job = self.semantic_get_job(&scope, &id)?;
        Ok(job.filter(|j| j.status == "running").map(|j| (scope, j)))
    }

    pub fn semantic_get_job(
        &self,
        scope: &ScopeKey,
        job_id: &str,
    ) -> Result<Option<SemanticJobRow>, StoreError> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {SEMANTIC_JOB_COLS} FROM semantic_jobs
                     WHERE tenant_id=?1 AND user_id=?2 AND id=?3"
                ),
                params![scope.tenant_id, scope.user_id, job_id],
                map_semantic_job,
            )
            .optional()
            .map_err(Into::into)
    }

    /// 索引作业收尾：succeed / retryable_failed / provider_wait / dead / stale_input。
    /// 返回是否生效（generation 匹配）；retry_delay_secs 提供退避 run_after。
    pub fn semantic_job_finish(
        &mut self,
        scope: &ScopeKey,
        job_id: &str,
        expected_generation: i64,
        status: &str,
        error_code: Option<&str>,
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
            "UPDATE semantic_jobs SET status=?5, error_code=?6, run_after=COALESCE(?7, run_after),
                    lease_until=NULL, updated_at=?8
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status='running' AND claim_generation=?4",
            params![
                scope.tenant_id,
                scope.user_id,
                job_id,
                expected_generation,
                status,
                error_code,
                run_after,
                now
            ],
        )?;
        Ok(n > 0)
    }
}

fn lease_from(now: &str, secs: u64) -> Result<String, StoreError> {
    let t = chrono::DateTime::parse_from_rfc3339(now)
        .map_err(|e| StoreError::Time(e.to_string()))?
        + chrono::Duration::seconds(secs as i64);
    Ok(t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}

/// f32 小端 BLOB 解码 + 余弦（维度不符/非有限返回 Err，调用方跳过坏行）。
fn decode_cosine(blob: &[u8], query: &[f32], qnorm: f32) -> Result<f32, ()> {
    if blob.len() != query.len() * 4 {
        return Err(());
    }
    let mut dot = 0f32;
    let mut vnorm = 0f32;
    for (i, q) in query.iter().enumerate() {
        let bytes = [
            blob[i * 4],
            blob[i * 4 + 1],
            blob[i * 4 + 2],
            blob[i * 4 + 3],
        ];
        let v = f32::from_le_bytes(bytes);
        if !v.is_finite() {
            return Err(());
        }
        dot += v * q;
        vnorm += v * v;
    }
    let vnorm = vnorm.sqrt();
    if qnorm == 0.0 || vnorm == 0.0 {
        return Err(());
    }
    Ok(dot / (qnorm * vnorm))
}
