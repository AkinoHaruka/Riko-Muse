//! V2-Q1 上下文条目（entry）：从可见 L0 用户事件确定性切块（doc7/10 §2—§4）。
//!
//! body 是来源事件内容的**逐字**拼接，不做摘要、不改写；逐条来源带字节 span 与内容哈希。
//! 读路径逐条复核来源：事件被 purge 删除、或对应记忆版本变化，条目即时不再返回。

use memory_domain::{DomainScope, ScopeKey};
use memory_recall::{cjk_bigrams, latin_tokens};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

pub const ENTRY_GENERATOR_VERSION: &str = "entry_v1";
/// 单条目正文上限（字符）。
pub const ENTRY_MAX_CHARS: usize = 1200;
/// 单条目事件数上限。
pub const ENTRY_MAX_EVENTS: usize = 8;
/// title 取首条事件的前若干字符。
const TITLE_MAX_CHARS: usize = 40;
/// 词法候选上限。
const ENTRY_CANDIDATE_LIMIT: usize = 200;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EntrySource {
    pub evidence_id: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub content_sha256: String,
    pub memory_id: Option<String>,
    pub memory_version: Option<i64>,
    pub claim_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextEntry {
    pub entry_id: String,
    pub domain_id: String,
    pub host_id: String,
    pub session_id: String,
    pub title: String,
    pub body: String,
    pub first_event_seq: i64,
    pub last_event_seq: i64,
    pub version: i64,
    pub batch_version: i64,
    pub sources: Vec<EntrySource>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryHit {
    pub entry_id: String,
    pub title: String,
    pub excerpt: String,
    pub score: f64,
    pub match_reason: &'static str,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntrySearchResult {
    pub hits: Vec<EntryHit>,
    pub skipped_stale: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryRefreshOutcome {
    pub batch_version: i64,
    pub entries: usize,
    pub sources: usize,
    pub previous_entries: usize,
}

fn fingerprint(parts: &[String]) -> String {
    let mut sorted = parts.to_vec();
    sorted.sort();
    let mut hasher = Sha256::new();
    for p in &sorted {
        hasher.update(p.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn title_of(body: &str) -> String {
    body.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(TITLE_MAX_CHARS)
        .collect()
}

fn excerpt_of(body: &str, max: usize) -> String {
    let mut out: String = body.chars().take(max).collect();
    if body.chars().count() > max {
        out.push('…');
    }
    out
}

/// 一条待写入的条目（refresh 内部使用）。
struct PendingEntry {
    host_id: String,
    session_id: String,
    first_seq: i64,
    last_seq: i64,
    body: String,
    sources: Vec<EntrySource>,
}

impl Store {
    /// 重建本域（\`dom.write\`）的上下文条目（doc7/10 §2）。
    pub fn entries_refresh(
        &mut self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<EntryRefreshOutcome, StoreError> {
        let doc_domain = dom.write.clone();
        let now = now_rfc3339()?;
        // 候选：写域内 user 事件（无映射按 user_main，与 rupture_scan 同口径）。
        let events: Vec<(String, String, String, i64, String, String)> = {
            let mut stmt = self.conn().prepare(
                "SELECT e.id, e.host_id, e.session_id, e.event_seq, e.content, e.content_sha256
                 FROM evidence_events e
                 LEFT JOIN evidence_domain_map d
                   ON d.tenant_id=e.tenant_id AND d.user_id=e.user_id AND d.evidence_id=e.id
                 WHERE e.tenant_id=?1 AND e.user_id=?2
                   AND COALESCE(d.domain_id,'user_main')=?3
                   AND e.role='user' AND e.source_kind='user'
                 ORDER BY e.session_id, e.event_seq",
            )?;
            let rows =
                stmt.query_map(params![scope.tenant_id, scope.user_id, doc_domain], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        // 确定性切块：会话变化 / 字符上限 / 事件数上限。
        let mut pending: Vec<PendingEntry> = Vec::new();
        for (evidence_id, host_id, session_id, seq, content, content_sha) in &events {
            let need_new = match pending.last() {
                None => true,
                Some(p) => {
                    p.session_id != *session_id
                        || p.body.chars().count() + content.chars().count() + 1 > ENTRY_MAX_CHARS
                        || p.sources.len() >= ENTRY_MAX_EVENTS
                }
            };
            if need_new {
                pending.push(PendingEntry {
                    host_id: host_id.clone(),
                    session_id: session_id.clone(),
                    first_seq: *seq,
                    last_seq: *seq,
                    body: content.clone(),
                    sources: Vec::new(),
                });
            }
            let p = pending.last_mut().expect("just pushed");
            if p.body.is_empty() {
                p.body = content.clone();
            } else if p.sources.len() > 0 || p.body != *content {
                p.body.push('\n');
                p.body.push_str(content);
            }
            p.last_seq = *seq;
            // 该事件派生出的原子记忆（可空）。
            let mem: Option<(String, i64, String)> = self
                .conn()
                .query_row(
                    "SELECT m.id, m.version, m.claim_sha256 FROM memory_evidence me
                     JOIN memories m ON m.tenant_id=me.tenant_id AND m.user_id=me.user_id
                       AND m.id=me.memory_id
                     WHERE me.tenant_id=?1 AND me.user_id=?2 AND me.evidence_id=?3
                     ORDER BY m.id LIMIT 1",
                    params![scope.tenant_id, scope.user_id, evidence_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let (memory_id, memory_version, claim_sha256) = match mem {
                Some((id, v, sha)) => (Some(id), Some(v), Some(sha)),
                None => (None, None, None),
            };
            p.sources.push(EntrySource {
                evidence_id: evidence_id.clone(),
                start_byte: 0,
                end_byte: content.len() as i64,
                content_sha256: content_sha.clone(),
                memory_id,
                memory_version,
                claim_sha256,
            });
        }

        // 事务内整体替换。
        let tx = self.conn_mut().transaction()?;
        let previous: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM context_entries
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
                params![scope.tenant_id, scope.user_id, doc_domain],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let prev_batch: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(batch_version),0) FROM context_entries
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
                params![scope.tenant_id, scope.user_id, doc_domain],
                |r| r.get(0),
            )
            .unwrap_or(0);
        // 先清索引，再删条目（entry_sources 由外键级联）。
        {
            let mut stmt = tx.prepare(
                "SELECT id FROM context_entries WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
            )?;
            let ids = stmt
                .query_map(params![scope.tenant_id, scope.user_id, doc_domain], |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for id in &ids {
                tx.execute("DELETE FROM entry_fts WHERE entry_id=?1", params![id])?;
                tx.execute(
                    "DELETE FROM entry_grams WHERE tenant_id=?1 AND user_id=?2 AND entry_id=?3",
                    params![scope.tenant_id, scope.user_id, id],
                )?;
            }
        }
        tx.execute(
            "DELETE FROM context_entries WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
            params![scope.tenant_id, scope.user_id, doc_domain],
        )?;

        let batch_version = prev_batch + 1;
        let mut entry_count = 0usize;
        let mut source_count = 0usize;
        for p in &pending {
            let entry_id = Uuid::now_v7().to_string();
            let fp = fingerprint(
                &p.sources
                    .iter()
                    .map(|s| format!("{}|{}", s.evidence_id, s.content_sha256))
                    .collect::<Vec<_>>(),
            );
            tx.execute(
                "INSERT INTO context_entries
                   (id, tenant_id, user_id, domain_id, host_id, session_id, title, body,
                    source_type, first_event_seq, last_event_seq, version, status,
                    generator_version, source_fingerprint, batch_version, generated_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'l0_window',?9,?10,1,'active',?11,?12,?13,?14,?14)",
                params![
                    entry_id,
                    scope.tenant_id,
                    scope.user_id,
                    doc_domain,
                    p.host_id,
                    p.session_id,
                    title_of(&p.body),
                    p.body,
                    p.first_seq,
                    p.last_seq,
                    ENTRY_GENERATOR_VERSION,
                    fp,
                    batch_version,
                    now
                ],
            )?;
            for s in &p.sources {
                tx.execute(
                    "INSERT INTO entry_sources
                       (id, tenant_id, user_id, entry_id, evidence_id, start_byte, end_byte,
                        content_sha256, memory_id, memory_version, claim_sha256)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                    params![
                        Uuid::now_v7().to_string(),
                        scope.tenant_id,
                        scope.user_id,
                        entry_id,
                        s.evidence_id,
                        s.start_byte,
                        s.end_byte,
                        s.content_sha256,
                        s.memory_id,
                        s.memory_version,
                        s.claim_sha256
                    ],
                )?;
                source_count += 1;
            }
            tx.execute(
                "INSERT INTO entry_fts (entry_id, title, body) VALUES (?1,?2,?3)",
                params![entry_id, title_of(&p.body), p.body],
            )?;
            for g in cjk_bigrams(&format!("{} {}", title_of(&p.body), p.body)) {
                tx.execute(
                    "INSERT OR IGNORE INTO entry_grams (tenant_id, user_id, entry_id, gram)
                     VALUES (?1,?2,?3,?4)",
                    params![scope.tenant_id, scope.user_id, entry_id, g],
                )?;
            }
            entry_count += 1;
        }
        tx.commit()?;
        Ok(EntryRefreshOutcome {
            batch_version,
            entries: entry_count,
            sources: source_count,
            previous_entries: previous as usize,
        })
    }

    /// 一条来源是否仍然成立（doc7/10 §3）。
    fn entry_source_valid(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        s: &EntrySource,
        now: &str,
    ) -> Result<bool, StoreError> {
        // 1) 事件仍存在且内容哈希未变（purge 会删除证据行）。
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM evidence_events
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND content_sha256=?4",
            params![
                scope.tenant_id,
                scope.user_id,
                s.evidence_id,
                s.content_sha256
            ],
            |r| r.get(0),
        )?;
        if n == 0 {
            return Ok(false);
        }
        // 2) 若来源带记忆，该记忆必须仍通过统一可见性判定且版本/哈希未变。
        if let Some(mid) = &s.memory_id {
            let sql = format!(
                "SELECT COUNT(*) FROM memories m
                 WHERE m.tenant_id=?1 AND m.user_id=?2 AND m.id=?3
                   AND m.version=?4 AND m.claim_sha256=?5 AND ({})",
                crate::explain::visible_memory_sql(false)
            );
            let m: i64 = self.conn().query_row(
                &sql,
                params![
                    scope.tenant_id,
                    scope.user_id,
                    mid,
                    s.memory_version.unwrap_or(-1),
                    s.claim_sha256.clone().unwrap_or_default(),
                    now,
                    dom.read_json()
                ],
                |r| r.get(0),
            )?;
            if m == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn entry_sources(
        &self,
        scope: &ScopeKey,
        entry_id: &str,
    ) -> Result<Vec<EntrySource>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT evidence_id, start_byte, end_byte, content_sha256, memory_id, memory_version,
                    claim_sha256
             FROM entry_sources WHERE tenant_id=?1 AND user_id=?2 AND entry_id=?3
             ORDER BY evidence_id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, entry_id], |r| {
            Ok(EntrySource {
                evidence_id: r.get(0)?,
                start_byte: r.get(1)?,
                end_byte: r.get(2)?,
                content_sha256: r.get(3)?,
                memory_id: r.get(4)?,
                memory_version: r.get(5)?,
                claim_sha256: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 读单条（逐条来源复核；来源失效即视为不存在）。
    pub fn entry_get(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        entry_id: &str,
    ) -> Result<Option<ContextEntry>, StoreError> {
        let now = now_rfc3339()?;
        let row: Option<(String, String, String, String, String, i64, i64, i64, i64)> = self
            .conn()
            .query_row(
                "SELECT domain_id, host_id, session_id, title, body, first_event_seq,
                        last_event_seq, version, batch_version
                 FROM context_entries
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='active'
                   AND domain_id IN (SELECT value FROM json_each(?4))",
                params![scope.tenant_id, scope.user_id, entry_id, dom.read_json()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            domain_id,
            host_id,
            session_id,
            title,
            body,
            first_event_seq,
            last_event_seq,
            version,
            batch_version,
        )) = row
        else {
            return Ok(None);
        };
        let sources = self.entry_sources(scope, entry_id)?;
        if sources.is_empty() {
            return Ok(None);
        }
        for s in &sources {
            if !self.entry_source_valid(scope, dom, s, &now)? {
                return Ok(None);
            }
        }
        Ok(Some(ContextEntry {
            entry_id: entry_id.to_string(),
            domain_id,
            host_id,
            session_id,
            title,
            body,
            first_event_seq,
            last_event_seq,
            version,
            batch_version,
            sources,
        }))
    }

    /// entry 词法召回（doc7/10 §4）：FTS5（拉丁 token）+ CJK 二元字，候选合并后逐条复核来源。
    pub fn entries_search(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        query: &str,
        limit: usize,
    ) -> Result<EntrySearchResult, StoreError> {
        let now = now_rfc3339()?;
        let mut candidates: Vec<(String, f64, &'static str)> = Vec::new();
        let tokens = latin_tokens(query);
        if let Some(m) = crate::memories::fts_match_query_pub(&tokens) {
            let mut stmt = self.conn().prepare(
                "SELECT entry_id, rank FROM entry_fts WHERE entry_fts MATCH ?1
                 ORDER BY rank LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![m, ENTRY_CANDIDATE_LIMIT as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
            })?;
            for (i, row) in rows.enumerate() {
                let (id, _) = row?;
                candidates.push((id, 1.0 / (i as f64 + 1.0), "fts"));
            }
        }
        let grams = cjk_bigrams(query);
        if !grams.is_empty() {
            let placeholders = vec!["?"; grams.len()].join(",");
            let sql = format!(
                "SELECT g.entry_id, COUNT(DISTINCT g.gram) AS hits FROM entry_grams g
                 JOIN context_entries e ON e.tenant_id=g.tenant_id AND e.user_id=g.user_id
                   AND e.id=g.entry_id AND e.status='active'
                 WHERE g.tenant_id=? AND g.user_id=? AND g.gram IN ({placeholders})
                 GROUP BY g.entry_id ORDER BY hits DESC LIMIT ?"
            );
            let mut stmt = self.conn().prepare(&sql)?;
            let mut bind: Vec<&dyn rusqlite::ToSql> = Vec::new();
            bind.push(&scope.tenant_id);
            bind.push(&scope.user_id);
            for g in &grams {
                bind.push(g);
            }
            let cap = ENTRY_CANDIDATE_LIMIT as i64;
            bind.push(&cap);
            let rows = stmt.query_map(bind.as_slice(), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for (i, row) in rows.enumerate() {
                let (id, hits) = row?;
                let score = hits as f64 / grams.len().max(1) as f64 / (i as f64 + 1.0);
                if let Some(existing) = candidates.iter_mut().find(|(eid, _, _)| *eid == id) {
                    // 两路都命中：取两条道之和，标记 rrf（与记忆/页面同口径）。
                    existing.1 += score;
                    existing.2 = "rrf";
                } else {
                    candidates.push((id, score, "grams"));
                }
            }
        }
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut hits = Vec::new();
        let mut skipped_stale = 0usize;
        for (entry_id, score, reason) in candidates {
            if hits.len() >= limit {
                break;
            }
            let Some(entry) = self.entry_get(scope, dom, &entry_id)? else {
                skipped_stale += 1;
                continue;
            };
            let body = entry.body.clone();
            hits.push(EntryHit {
                entry_id,
                title: entry.title,
                excerpt: excerpt_of(&body, 160),
                score,
                match_reason: reason,
                session_id: entry.session_id,
            });
        }
        let _ = now;
        Ok(EntrySearchResult {
            hits,
            skipped_stale,
        })
    }

    /// purge / forget 闭包：删来源行，再删零来源条目（doc7/10 §3）。
    pub fn entries_prune_orphans(&self, scope: &ScopeKey) -> Result<usize, StoreError> {
        self.conn().execute(
            "DELETE FROM entry_fts WHERE entry_id IN (
                 SELECT d.id FROM context_entries d
                 WHERE d.tenant_id=?1 AND d.user_id=?2
                   AND NOT EXISTS (SELECT 1 FROM entry_sources s
                       WHERE s.tenant_id=d.tenant_id AND s.user_id=d.user_id AND s.entry_id=d.id))",
            params![scope.tenant_id, scope.user_id],
        )?;
        self.conn().execute(
            "DELETE FROM entry_grams WHERE tenant_id=?1 AND user_id=?2 AND entry_id IN (
                 SELECT d.id FROM context_entries d
                 WHERE d.tenant_id=?1 AND d.user_id=?2
                   AND NOT EXISTS (SELECT 1 FROM entry_sources s
                       WHERE s.tenant_id=d.tenant_id AND s.user_id=d.user_id AND s.entry_id=d.id))",
            params![scope.tenant_id, scope.user_id],
        )?;
        let removed = self.conn().execute(
            "DELETE FROM context_entries
             WHERE tenant_id=?1 AND user_id=?2
               AND NOT EXISTS (SELECT 1 FROM entry_sources s
                   WHERE s.tenant_id=context_entries.tenant_id
                     AND s.user_id=context_entries.user_id
                     AND s.entry_id=context_entries.id)",
            params![scope.tenant_id, scope.user_id],
        )?;
        Ok(removed)
    }
}
