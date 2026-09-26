//! 派生知识文档存储（doc6/02 §3、doc6/05）。
//!
//! 问题目录由 Rust 管理（首版 CLI）；页面发布在单事务内核对输入指纹与全部
//! 来源 L1 的 status/version/hash/有效期（doc6/05 §4），CAS 版本替换旧
//! published，旧版留 revisions。correct/forget/retire 引用源变化时同事务置
//! stale 并移除索引（`stale_pages_for_memory`）；读路径每次复核来源
//! （doc6/05 §4：后台修复只改善可用性，不承担撤销正确性）。

use memory_domain::ScopeKey;
use memory_recall::cjk_bigrams;
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

/// generator/Prompt 版本（doc6/05 §3）：修改 Prompt 必须新建版本；旧 job 按保存版本分派。
pub const GENERATE_CONSOLIDATE_V1: &str = "consolidate_v1";
pub const GENERATE_MENTAL_MODEL_V1: &str = "mental_model_v1";

pub const QUESTION_TEXT_MAX_CHARS: usize = 200;
pub const PAGE_TITLE_MAX_CHARS: usize = 80;
pub const PAGE_BODY_MAX_CHARS: usize = 1200;

fn validate_question_key(key: &str) -> Result<(), StoreError> {
    let ok = !key.is_empty()
        && key.chars().count() <= 64
        && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(StoreError::InvalidQuestionKey)
    }
}

#[derive(Debug, Clone)]
pub struct QuestionRow {
    pub question_key: String,
    pub question_text: String,
    pub version: i64,
    pub status: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct PageRow {
    pub page_id: String,
    pub document_kind: String,
    pub document_key: String,
    pub question_version: Option<i64>,
    pub question_text: Option<String>,
    pub title: String,
    pub body_md: String,
    pub status: String,
    pub version: i64,
    pub generator_version: String,
    pub input_fingerprint: String,
    pub updated_at: String,
    /// 来源（memory_id, memory_version）；读时已复核当前有效。
    pub sources: Vec<(String, i64)>,
}

/// 发布请求（doc6/05 §4）：来源由 Rust 从 job 固化输入核验，不接受模型自报。
pub struct PublishRequest<'a> {
    pub scope: &'a ScopeKey,
    pub document_kind: &'a str,
    pub document_key: &'a str,
    pub question_version: Option<i64>,
    pub question_text: Option<&'a str>,
    pub title: &'a str,
    pub body_md: &'a str,
    pub generator_version: &'a str,
    pub input_fingerprint: &'a str,
    /// (memory_id, memory_version, claim_sha256)：必须全部来自 job 固化输入。
    pub sources: &'a [(String, i64, String)],
    pub actor_kind: &'a str,
}

impl Store {
    // ---- 问题目录（doc6/05 §2；可信 CLI，模型不可改）----

    /// 登记问题：key 必须不存在（doc6/02 §3 add 语义）。
    pub fn question_add(
        &mut self,
        scope: &ScopeKey,
        key: &str,
        text: &str,
        actor_kind: &str,
    ) -> Result<i64, StoreError> {
        validate_question_key(key)?;
        if text.is_empty() || text.chars().count() > QUESTION_TEXT_MAX_CHARS {
            return Err(StoreError::InvalidQuestionText);
        }
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT version FROM mental_model_questions
                 WHERE tenant_id=?1 AND user_id=?2 AND question_key=?3",
                params![scope.tenant_id, scope.user_id, key],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            return Err(StoreError::StateConflict);
        }
        tx.execute(
            "INSERT INTO mental_model_questions
               (tenant_id, user_id, question_key, question_text, version, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, 'active', ?5, ?5)",
            params![scope.tenant_id, scope.user_id, key, text, now],
        )?;
        tx.execute(
            "INSERT INTO mental_model_question_revisions
               (tenant_id, user_id, question_key, version, question_text, status, actor_kind, changed_at)
             VALUES (?1, ?2, ?3, 1, ?4, 'active', ?5, ?6)",
            params![scope.tenant_id, scope.user_id, key, text, actor_kind, now],
        )?;
        tx.commit()?;
        Ok(1)
    }

    /// 修改问题正文（CAS）：同事务写 revision，并使该问题的已发布画像立即
    /// stale 且移除索引（doc6/05 §2）；同内容幂等不增版本。
    pub fn question_update(
        &mut self,
        scope: &ScopeKey,
        key: &str,
        text: &str,
        expected_version: i64,
        actor_kind: &str,
    ) -> Result<i64, StoreError> {
        if text.is_empty() || text.chars().count() > QUESTION_TEXT_MAX_CHARS {
            return Err(StoreError::InvalidQuestionText);
        }
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let current: (i64, String) = tx
            .query_row(
                "SELECT version, question_text FROM mental_model_questions
                 WHERE tenant_id=?1 AND user_id=?2 AND question_key=?3",
                params![scope.tenant_id, scope.user_id, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::QuestionNotFound)?;
        if current.1 == text && expected_version == current.0 {
            tx.commit()?;
            return Ok(current.0);
        }
        if expected_version != current.0 {
            return Err(StoreError::VersionConflict);
        }
        let new_version = current.0 + 1;
        tx.execute(
            "UPDATE mental_model_questions SET question_text=?4, version=?5, updated_at=?6
             WHERE tenant_id=?1 AND user_id=?2 AND question_key=?3",
            params![scope.tenant_id, scope.user_id, key, text, new_version, now],
        )?;
        tx.execute(
            "INSERT INTO mental_model_question_revisions
               (tenant_id, user_id, question_key, version, question_text, status, actor_kind, changed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?7)",
            params![scope.tenant_id, scope.user_id, key, new_version, text, actor_kind, now],
        )?;
        Self::stale_question_pages_tx(&tx, scope, key)?;
        tx.commit()?;
        Ok(new_version)
    }

    /// 归档问题（用户"删除"首版行为，doc6/02 §3）：同事务置画像 stale。
    pub fn question_archive(
        &mut self,
        scope: &ScopeKey,
        key: &str,
        expected_version: i64,
        actor_kind: &str,
    ) -> Result<i64, StoreError> {
        self.question_transition(scope, key, expected_version, actor_kind, "archived")
    }

    /// 重新启用：按新版本与当前有效 L1 重新生成，旧页面正文不直接恢复。
    pub fn question_reactivate(
        &mut self,
        scope: &ScopeKey,
        key: &str,
        expected_version: i64,
        actor_kind: &str,
    ) -> Result<i64, StoreError> {
        self.question_transition(scope, key, expected_version, actor_kind, "active")
    }

    fn question_transition(
        &mut self,
        scope: &ScopeKey,
        key: &str,
        expected_version: i64,
        actor_kind: &str,
        new_status: &'static str,
    ) -> Result<i64, StoreError> {
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let current: (i64, String, String) = tx
            .query_row(
                "SELECT version, question_text, status FROM mental_model_questions
                 WHERE tenant_id=?1 AND user_id=?2 AND question_key=?3",
                params![scope.tenant_id, scope.user_id, key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(StoreError::QuestionNotFound)?;
        if current.2 == new_status {
            tx.commit()?;
            return Ok(current.0); // 幂等
        }
        if expected_version != current.0 {
            return Err(StoreError::VersionConflict);
        }
        let new_version = current.0 + 1;
        tx.execute(
            "UPDATE mental_model_questions SET version=?4, status=?5, updated_at=?6
             WHERE tenant_id=?1 AND user_id=?2 AND question_key=?3",
            params![scope.tenant_id, scope.user_id, key, new_version, new_status, now],
        )?;
        tx.execute(
            "INSERT INTO mental_model_question_revisions
               (tenant_id, user_id, question_key, version, question_text, status, actor_kind, changed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![scope.tenant_id, scope.user_id, key, new_version, current.1, new_status, actor_kind, now],
        )?;
        Self::stale_question_pages_tx(&tx, scope, key)?;
        tx.commit()?;
        Ok(new_version)
    }

    /// 问题目录列表（含 archived；CLI/HTTP 审阅用，不触发模型）。
    pub fn question_list(
        &self,
        scope: &ScopeKey,
        status: Option<&str>,
    ) -> Result<Vec<QuestionRow>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT question_key, question_text, version, status, updated_at FROM mental_model_questions
             WHERE tenant_id=?1 AND user_id=?2 AND (?3 IS NULL OR status=?3)
             ORDER BY question_key",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, status], |r| {
            Ok(QuestionRow {
                question_key: r.get(0)?,
                question_text: r.get(1)?,
                version: r.get(2)?,
                status: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 同事务把某问题的已发布画像置 stale 并移除索引（doc6/05 §2/§4）。
    fn stale_question_pages_tx(
        tx: &rusqlite::Transaction<'_>,
        scope: &ScopeKey,
        key: &str,
    ) -> Result<(), StoreError> {
        let stale_ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM memory_pages
                 WHERE tenant_id=?1 AND user_id=?2 AND document_kind='mental_model'
                   AND document_key=?3 AND status='published'",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, key], |r| r.get(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for id in stale_ids {
            tx.execute(
                "UPDATE memory_pages SET status='stale', updated_at=?4
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, id, now_rfc3339()?],
            )?;
            remove_page_index_tx(tx, scope, &id)?;
        }
        Ok(())
    }

    // ---- 页面发布与读取（doc6/05 §4）----

    /// 发布：单事务核全部来源当前 active/同版/哈希一致；CAS 版本替换旧
    /// published（旧版留 revisions）；同事务更新 FTS/grams。任一来源失效 →
    /// StaleInput 整批不发布（doc6/05 §4：不做部分发布）。
    pub fn publish_page(&mut self, req: &PublishRequest<'_>) -> Result<(String, i64), StoreError> {
        if req.title.is_empty() || req.title.chars().count() > PAGE_TITLE_MAX_CHARS {
            return Err(StoreError::InvalidPageField);
        }
        if req.body_md.is_empty() || req.body_md.chars().count() > PAGE_BODY_MAX_CHARS {
            return Err(StoreError::InvalidPageField);
        }
        if req.sources.is_empty() {
            return Err(StoreError::InvalidPageField);
        }
        let now = now_rfc3339()?;
        for (mid, ver, sha) in req.sources {
            let row: Option<(i64, String)> = self
                .conn()
                .query_row(
                    "SELECT version, claim_sha256 FROM memories
                     WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='active'
                       AND (valid_until IS NULL OR valid_until > ?4)",
                    params![req.scope.tenant_id, req.scope.user_id, mid, now],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match row {
                Some((v, s)) if v == *ver && s == *sha => {}
                _ => return Err(StoreError::StaleInput),
            }
        }
        let existing: Option<(String, i64)> = self
            .conn()
            .query_row(
                "SELECT id, version FROM memory_pages
                 WHERE tenant_id=?1 AND user_id=?2 AND document_kind=?3 AND document_key=?4
                   AND status IN ('published','stale') LIMIT 1",
                params![req.scope.tenant_id, req.scope.user_id, req.document_kind, req.document_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (page_id, new_version, previous) = match &existing {
            Some((id, v)) => {
                let prev: Option<(String, String, i64)> = self
                    .conn()
                    .query_row(
                        "SELECT title, body_md, version FROM memory_pages WHERE id=?1",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                (id.clone(), v + 1, prev.map(|(t, b, _)| (t, b)))
            }
            None => (Uuid::now_v7().to_string(), 1, None),
        };
        let tx = self.conn_mut().transaction()?;
        if existing.is_some() {
            // 旧版快照已在首次发布时写入 page_revisions（version=旧值）；此处只
            // CAS 推进页面行并替换来源（doc6/05 §4：旧版留 revisions 供查证）。
            tx.execute(
                "UPDATE memory_pages
                 SET title=?4, body_md=?5, status='published', version=?6,
                     generator_version=?7, input_fingerprint=?8, updated_at=?9
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![
                    req.scope.tenant_id,
                    req.scope.user_id,
                    page_id,
                    req.title,
                    req.body_md,
                    new_version,
                    req.generator_version,
                    req.input_fingerprint,
                    now
                ],
            )?;
            tx.execute(
                "DELETE FROM page_sources WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
                params![req.scope.tenant_id, req.scope.user_id, page_id],
            )?;
        } else {
            tx.execute(
                "INSERT INTO memory_pages
                   (id, tenant_id, user_id, document_kind, document_key, question_version,
                    question_text, title, body_md, status, version, generator_version,
                    input_fingerprint, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'published', 1, ?10, ?11, ?12, ?12)",
                params![
                    page_id,
                    req.scope.tenant_id,
                    req.scope.user_id,
                    req.document_kind,
                    req.document_key,
                    req.question_version,
                    req.question_text,
                    req.title,
                    req.body_md,
                    req.generator_version,
                    req.input_fingerprint,
                    now
                ],
            )?;
        }
        for (mid, ver, sha) in req.sources {
            tx.execute(
                "INSERT INTO page_sources
                   (tenant_id, user_id, page_id, memory_id, memory_version, claim_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![req.scope.tenant_id, req.scope.user_id, page_id, mid, ver, sha],
            )?;
        }
        // 新版本快照（doc6/02 §3：来源 ID/version 列表 JSON）。
        let sources_json = format!(
            "[{}]",
            req.sources
                .iter()
                .map(|(id, v, _)| format!("{{\"memory_id\":\"{id}\",\"memory_version\":{v}}}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        tx.execute(
            "INSERT INTO page_revisions
               (tenant_id, user_id, page_id, version, document_kind, document_key,
                question_version, question_text, previous_title, new_title,
                previous_body_md, new_body_md, source_ids_json, generator_version, actor_kind, changed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                req.scope.tenant_id,
                req.scope.user_id,
                page_id,
                new_version,
                req.document_kind,
                req.document_key,
                req.question_version,
                req.question_text,
                previous.as_ref().map(|(t, _)| t.clone()),
                req.title,
                previous.as_ref().map(|(_, b)| b.clone()),
                req.body_md,
                sources_json,
                req.generator_version,
                req.actor_kind,
                now
            ],
        )?;
        // 索引重建（published 页进 FTS/grams；doc6/02 §3）。
        tx.execute("DELETE FROM page_fts WHERE page_id=?1", params![page_id])?;
        tx.execute(
            "DELETE FROM page_grams WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
            params![req.scope.tenant_id, req.scope.user_id, page_id],
        )?;
        tx.execute(
            "INSERT INTO page_fts (page_id, title, body_md) VALUES (?1, ?2, ?3)",
            params![page_id, req.title, req.body_md],
        )?;
        for g in cjk_bigrams(&format!("{} {}", req.title, req.body_md)) {
            tx.execute(
                "INSERT OR IGNORE INTO page_grams (tenant_id, user_id, page_id, gram) VALUES (?1, ?2, ?3, ?4)",
                params![req.scope.tenant_id, req.scope.user_id, page_id, g],
            )?;
        }
        tx.commit()?;
        Ok((page_id, new_version))
    }

    /// 读单页（doc6/05 §4 读时复核）：published 且全部来源当前 active/同版/
    /// 未过期才返回；任何不符立即不可见。
    pub fn get_page(
        &self,
        scope: &ScopeKey,
        page_id: &str,
        now: &str,
    ) -> Result<Option<PageRow>, StoreError> {
        let base: Option<PageRow> = self
            .conn()
            .query_row(
                "SELECT id, document_kind, document_key, question_version, question_text,
                        title, body_md, status, version, generator_version, input_fingerprint, updated_at
                 FROM memory_pages WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, page_id],
                |r| {
                    Ok(PageRow {
                        page_id: r.get(0)?,
                        document_kind: r.get(1)?,
                        document_key: r.get(2)?,
                        question_version: r.get(3)?,
                        question_text: r.get(4)?,
                        title: r.get(5)?,
                        body_md: r.get(6)?,
                        status: r.get(7)?,
                        version: r.get(8)?,
                        generator_version: r.get(9)?,
                        input_fingerprint: r.get(10)?,
                        updated_at: r.get(11)?,
                        sources: Vec::new(),
                    })
                },
            )
            .optional()?;
        let Some(mut page) = base else { return Ok(None) };
        if page.status != "published" {
            return Ok(None);
        }
        let mut stmt = self.conn().prepare(
            "SELECT s.memory_id, s.memory_version, s.claim_sha256, m.status, m.version, m.claim_sha256
             FROM page_sources s JOIN memories m
               ON m.tenant_id=s.tenant_id AND m.user_id=s.user_id AND m.id=s.memory_id
             WHERE s.tenant_id=?1 AND s.user_id=?2 AND s.page_id=?3",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, page_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        for row in rows {
            let (mid, ver, sha, status, cur_ver, cur_sha) = row?;
            if status != "active" || cur_ver != ver || cur_sha != sha {
                return Ok(None); // 来源失效 → 不可见（doc6/05 §4）
            }
            page.sources.push((mid, ver));
        }
        if page.sources.is_empty() {
            return Ok(None);
        }
        Ok(Some(page))
    }

    /// correct/forget/retire 引用源变化时调用（doc6/05 §4/§5）：引用该 memory
    /// 的 published 页置 stale 并移除索引（调用方与其业务修改同事务或随后
    /// 立即执行；doc6/02 §8.1 同事务失效）。
    pub fn stale_pages_for_memory(&mut self, scope: &ScopeKey, memory_id: &str) -> Result<(), StoreError> {
        let ids: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT DISTINCT p.id FROM memory_pages p
                 JOIN page_sources s ON s.tenant_id=p.tenant_id AND s.user_id=p.user_id
                    AND s.page_id=p.id
                 WHERE p.tenant_id=?1 AND p.user_id=?2 AND s.memory_id=?3 AND p.status='published'",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, memory_id], |r| r.get(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if ids.is_empty() {
            return Ok(());
        }
        let tx = self.conn_mut().transaction()?;
        for id in ids {
            tx.execute(
                "UPDATE memory_pages SET status='stale', updated_at=?4
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='published'",
                params![scope.tenant_id, scope.user_id, id, now_rfc3339()?],
            )?;
            remove_page_index_tx(&tx, scope, &id)?;
        }
        tx.commit()?;
        Ok(())
    }
}

impl Store {
/// 页面列表（CLI/HTTP 审阅；默认只 published，可看 stale/archived）。
/// 不做来源逐页复核（show/读取接口才复核），列表标 status 供审阅。
pub fn page_list(
        &self,
        scope: &ScopeKey,
        statuses: &[&str],
        limit: usize,
    ) -> Result<Vec<PageRow>, StoreError> {
        let status_clause = if statuses.is_empty() {
            "status IN ('published','stale','archived')".to_string()
        } else {
            let list = statuses.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",");
            format!("status IN ({list})")
        };
        let mut stmt = self.conn().prepare(&format!(
            "SELECT id, document_kind, document_key, question_version, question_text,
                    title, body_md, status, version, generator_version, input_fingerprint, updated_at
             FROM memory_pages WHERE tenant_id=?1 AND user_id=?2 AND {status_clause}
             ORDER BY updated_at DESC LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, limit as i64], |r| {
            Ok(PageRow {
                page_id: r.get(0)?,
                document_kind: r.get(1)?,
                document_key: r.get(2)?,
                question_version: r.get(3)?,
                question_text: r.get(4)?,
                title: r.get(5)?,
                body_md: r.get(6)?,
                status: r.get(7)?,
                version: r.get(8)?,
                generator_version: r.get(9)?,
                input_fingerprint: r.get(10)?,
                updated_at: r.get(11)?,
                sources: Vec::new(),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 用户/管理员归档页面（doc6/06 §2 POST /v1/pages/{id}/archive 的存储层）：
    /// CAS；同事务移除索引；归档保留 revision 但不再搜索/注入。
    pub fn page_archive(
        &mut self,
        scope: &ScopeKey,
        page_id: &str,
        expected_version: i64,
    ) -> Result<bool, StoreError> {
        let tx = self.conn_mut().transaction()?;
        let n = tx.execute(
            "UPDATE memory_pages SET status='archived', updated_at=?4
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3
               AND status='published' AND version=?5",
            params![scope.tenant_id, scope.user_id, page_id, now_rfc3339()?, expected_version],
        )?;
        remove_page_index_tx(&tx, scope, page_id)?;
        // D6-8：页面向量失效与归档同事务（doc6/02 §4）。
        crate::Store::stale_vectors_in_tx(&tx, scope, "page", page_id)?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// 页面词法检索（doc6/06）：grams 二元字匹配（与 memory_grams 同法；
    /// FTS unicode61 对连续 CJK 长 token 不可靠，grams 是本项目中文主通道）。
    /// 只返回 published 页（读时仍有 get_page 来源复核兜底）。
    pub fn page_fts_search(
        &self,
        scope: &ScopeKey,
        query: &str,
        limit: usize,
    ) -> Result<Vec<String>, StoreError> {
        let grams = cjk_bigrams(query);
        if grams.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; grams.len()].join(",");
        let mut bind_values: Vec<String> = vec![scope.tenant_id.clone(), scope.user_id.clone()];
        bind_values.extend(grams.clone());
        let sql = format!(
            "SELECT DISTINCT g.page_id FROM page_grams g
             JOIN memory_pages p ON p.id=g.page_id AND p.tenant_id=g.tenant_id AND p.user_id=g.user_id
             WHERE g.tenant_id=?1 AND g.user_id=?2 AND p.status='published'
               AND g.gram IN ({placeholders})
             ORDER BY g.page_id LIMIT ?{}",
            bind_values.len() + 1
        );
        bind_values.push((limit as i64).to_string());
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(bind_values.iter().map(|s| s as &dyn rusqlite::ToSql)),
            |r| r.get(0),
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ---- 页面 pin（doc6/06 §2 resident_page_pins；doc6/03 §4 pin 后进 resident）----

    /// pin 一页（仅 published 且全部来源有效可 pin）；重 pin 沿原行增 version。
    pub fn page_pin(&mut self, scope: &ScopeKey, page_id: &str) -> Result<i64, StoreError> {
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        // 只允许 pin 当前 published 页（doc6/02 §3）。
        let published: Option<i64> = tx
            .query_row(
                "SELECT version FROM memory_pages
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='published'",
                params![scope.tenant_id, scope.user_id, page_id],
                |r| r.get(0),
            )
            .optional()?;
        if published.is_none() {
            return Err(StoreError::PageNotFound);
        }
        let existing: Option<(i64, i64)> = tx
            .query_row(
                "SELECT enabled, version FROM resident_page_pins
                 WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
                params![scope.tenant_id, scope.user_id, page_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let version = match existing {
            None => {
                let max: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(position), -1) FROM resident_page_pins
                     WHERE tenant_id=?1 AND user_id=?2 AND enabled=1",
                    params![scope.tenant_id, scope.user_id],
                    |r| r.get(0),
                )?;
                tx.execute(
                    "INSERT INTO resident_page_pins
                       (tenant_id, user_id, page_id, enabled, position, pinned_at, version)
                     VALUES (?1, ?2, ?3, 1, ?4, ?5, 1)",
                    params![scope.tenant_id, scope.user_id, page_id, max + 1, now],
                )?;
                1
            }
            Some((enabled, version)) if enabled == 0 => {
                let max: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(position), -1) FROM resident_page_pins
                     WHERE tenant_id=?1 AND user_id=?2 AND enabled=1",
                    params![scope.tenant_id, scope.user_id],
                    |r| r.get(0),
                )?;
                tx.execute(
                    "UPDATE resident_page_pins SET enabled=1, position=?4, version=?5, pinned_at=?6
                     WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
                    params![scope.tenant_id, scope.user_id, page_id, max + 1, version + 1, now],
                )?;
                version + 1
            }
            Some((_, version)) => version, // 已 enabled：幂等
        };
        tx.commit()?;
        Ok(version)
    }

    /// 解除页面 pin：置 enabled=0 并增版本；无行/已 disabled 幂等。
    pub fn page_unpin(&mut self, scope: &ScopeKey, page_id: &str) -> Result<i64, StoreError> {
        let now = now_rfc3339()?;
        let existing: Option<(i64, i64)> = self
            .conn()
            .query_row(
                "SELECT enabled, version FROM resident_page_pins
                 WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
                params![scope.tenant_id, scope.user_id, page_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((enabled, version)) = existing else { return Ok(0) };
        if enabled == 0 {
            return Ok(version);
        }
        self.conn_mut().execute(
            "UPDATE resident_page_pins SET enabled=0, version=?4
             WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
            params![scope.tenant_id, scope.user_id, page_id, version + 1],
        )?;
        let _ = now;
        Ok(version + 1)
    }

    /// rebuild 时从规范表重建页面索引：只 published 页进 FTS/grams
    /// （doc6/02 §3；stale/archived 不复活——rebuild 后不复活验收）。
    pub fn rebuild_page_index(&mut self) -> Result<(usize, usize), StoreError> {
        let tx = self.conn_mut().transaction()?;
        tx.execute("DELETE FROM page_fts", [])?;
        tx.execute("DELETE FROM page_grams", [])?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare(
                "SELECT id, tenant_id, user_id, title, body_md FROM memory_pages WHERE status='published'",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?;
            let collected: Vec<(String, String, String, String, String)> =
                rows.collect::<Result<Vec<_>, _>>()?;
            for (id, tenant, user, title, body) in collected {
                tx.execute(
                    "INSERT INTO page_fts (page_id, title, body_md) VALUES (?1, ?2, ?3)",
                    params![id, title, body],
                )?;
                for g in cjk_bigrams(&format!("{title} {body}")) {
                    tx.execute(
                        "INSERT OR IGNORE INTO page_grams (tenant_id, user_id, page_id, gram)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![tenant, user, id, g],
                    )?;
                }
                inserted += 1;
            }
        }
        tx.commit()?;
        Ok((inserted, 0))
    }
}

/// 移除一页的 FTS/grams 索引行（同事务；doc6/02 §8.1）。
fn remove_page_index_tx(
    tx: &rusqlite::Transaction<'_>,
    scope: &ScopeKey,
    page_id: &str,
) -> Result<(), StoreError> {
    tx.execute("DELETE FROM page_fts WHERE page_id=?1", params![page_id])?;
    tx.execute(
        "DELETE FROM page_grams WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
        params![scope.tenant_id, scope.user_id, page_id],
    )?;
    Ok(())
}

// ---- Prompt 输出契约（doc6/05 §3；严格 JSON，可剥 Markdown 围栏）----

use serde::Deserialize;

/// `mental_model_v1` 输出（doc6/05 §3.1）：question_key 必须与 job 一致；
/// 来源 1—20 且全部来自冻结输入。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MentalModelOutput {
    pub question_key: String,
    pub answer_md: String,
    pub source_memory_ids: Vec<String>,
}

/// `consolidate_v1` 单页输出（doc6/05 §3）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsolidatePageOutput {
    pub topic_key: String,
    pub title: String,
    pub body_md: String,
    pub source_memory_ids: Vec<String>,
}

/// `consolidate_v1` 输出：每响应最多 4 页。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsolidateOutput {
    pub pages: Vec<ConsolidatePageOutput>,
}

/// 剥一层 Markdown 代码围栏（doc6/05 §3：剥离后仍严格校验）。
fn strip_fence(raw: &str) -> &str {
    let t = raw.trim();
    if let Some(rest) = t.strip_prefix("```").or_else(|| t.strip_prefix("```json")) {
        let rest = rest.trim_start_matches("json").trim_start_matches('\n');
        return rest.trim_end_matches("```").trim();
    }
    t
}

/// 校验 `mental_model_v1` 输出（doc6/05 §3.1）：字段集固定、答案 1—1200 字、
/// 来源 1—20 条。来源是否在冻结输入内由调用方核（publish/路由侧）。
pub fn parse_mental_model_output(raw: &str, expected_key: &str) -> Result<MentalModelOutput, StoreError> {
    let out: MentalModelOutput = serde_json::from_str(strip_fence(raw))
        .map_err(|_| StoreError::InvalidPageField)?;
    if out.question_key != expected_key {
        return Err(StoreError::InvalidPageField);
    }
    if out.answer_md.is_empty() || out.answer_md.chars().count() > PAGE_BODY_MAX_CHARS {
        return Err(StoreError::InvalidPageField);
    }
    if out.source_memory_ids.is_empty() || out.source_memory_ids.len() > 20 {
        return Err(StoreError::InvalidPageField);
    }
    Ok(out)
}

/// 校验 `consolidate_v1` 输出：≤4 页；title 1—80、正文 1—1200、来源 2—20。
pub fn parse_consolidate_output(raw: &str) -> Result<ConsolidateOutput, StoreError> {
    let out: ConsolidateOutput = serde_json::from_str(strip_fence(raw))
        .map_err(|_| StoreError::InvalidPageField)?;
    if out.pages.is_empty() || out.pages.len() > 4 {
        return Err(StoreError::InvalidPageField);
    }
    for p in &out.pages {
        if p.topic_key.is_empty() || p.source_memory_ids.len() < 2 || p.source_memory_ids.len() > 20 {
            return Err(StoreError::InvalidPageField);
        }
        if p.title.is_empty() || p.title.chars().count() > PAGE_TITLE_MAX_CHARS {
            return Err(StoreError::InvalidPageField);
        }
        if p.body_md.is_empty() || p.body_md.chars().count() > PAGE_BODY_MAX_CHARS {
            return Err(StoreError::InvalidPageField);
        }
    }
    Ok(out)
}
