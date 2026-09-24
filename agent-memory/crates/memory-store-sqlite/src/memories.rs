//! 记忆读写与派生索引维护（doc/04、doc/11 §3、doc/12 §5/§7、doc/13 §6）。
//!
//! 事务顺序（doc/11 §3.4）：规范事务先写 memory+evidence+revision+audit+dirty=1 并提交；
//! 随后独立索引事务维护 FTS/grams，成功才 dirty=0、generation+1；索引失败保留 dirty，
//! 绝不回滚已提交的规范记忆。所有读路径按规范表状态过滤（forgotten 永不返回）。

use memory_domain::{
    claim_sha256, find_quote_span, fold_whitespace, normalize_v1, MemoryKind, Origin, ScopeKey,
};
use memory_recall::{cjk_bigrams, is_single_char_query, latin_tokens, rrf_score};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

pub enum RememberOutcome {
    Created { memory_id: String, version: i64 },
    Dedup { memory_id: String, version: i64 },
}

#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub memory_id: String,
    pub kind: String,
    pub claim: String,
    pub status: String,
    pub version: i64,
    pub occurred_at: Option<String>,
    pub valid_until: Option<String>,
    pub origin_agent_id: String,
    pub updated_at: String,
    pub evidence_refs: Vec<(String, Option<i64>, Option<i64>)>,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub memory_id: String,
    pub kind: String,
    pub claim: String,
    pub status: String,
    pub score: f64,
    pub match_reason: String,
    pub evidence_refs: Vec<String>,
}

pub struct ComposeResult {
    pub text: String,
    pub items: Vec<(String, Vec<String>)>,
    pub truncated: bool,
    pub index_degraded: bool,
}

/// 历史词（doc/12 §7）：include_history 仅当 query 含明确历史词。
pub fn has_history_cue(query: &str) -> bool {
    let q = query.to_lowercase();
    ["以前", "过去", "曾经", "当时", "previously", "used to", "before"]
        .iter()
        .any(|cue| q.contains(cue))
}

/// FTS5 MATCH 转义：每个 token 变 quoted term（doc/13 §6）。
fn fts_match_query(tokens: &[String]) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    let terms: Vec<String> = tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    Some(terms.join(" "))
}

fn is_active_clause(include_history: bool) -> &'static str {
    if include_history {
        "status IN ('active','superseded','expired')"
    } else {
        "status = 'active'"
    }
}

impl Store {
    pub(crate) fn mark_index_dirty(conn: &rusqlite::Connection) -> Result<(), StoreError> {
        conn.execute(
            "UPDATE index_state SET dirty=1, updated_at=?1 WHERE singleton=1",
            params![now_rfc3339()?],
        )?;
        Ok(())
    }

    fn clear_index_dirty(conn: &rusqlite::Connection) -> Result<(), StoreError> {
        conn.execute(
            "UPDATE index_state SET dirty=0, generation=generation+1, updated_at=?1 WHERE singleton=1",
            params![now_rfc3339()?],
        )?;
        Ok(())
    }

    pub fn index_degraded(&self) -> bool {
        self.conn()
            .query_row("SELECT dirty FROM index_state WHERE singleton=1", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|d| d == 1)
            .unwrap_or(true)
    }

    /// 索引事务：删除旧派生行，插入当前 active 行（doc/13 §7）。
    pub(crate) fn reindex_memory(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        claim: &str,
        keep: bool,
    ) -> Result<(), StoreError> {
        let tx = self.conn_mut().transaction()?;
        tx.execute("DELETE FROM memory_fts WHERE memory_id=?1", params![memory_id])?;
        tx.execute(
            "DELETE FROM memory_grams WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        if keep {
            tx.execute(
                "INSERT INTO memory_fts (memory_id, claim) VALUES (?1, ?2)",
                params![memory_id, normalize_v1(claim)],
            )?;
            for gram in cjk_bigrams(claim) {
                tx.execute(
                    "INSERT OR IGNORE INTO memory_grams (tenant_id, user_id, memory_id, gram) VALUES (?1,?2,?3,?4)",
                    params![scope.tenant_id, scope.user_id, memory_id, gram],
                )?;
            }
        }
        Self::clear_index_dirty(&tx)?;
        tx.commit()?;
        Ok(())
    }

    fn evidence_refs_of(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
    ) -> Result<Vec<(String, Option<i64>, Option<i64>)>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT evidence_id, start_byte, end_byte FROM memory_evidence
             WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3 ORDER BY rowid",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, memory_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// POST /v1/memories/remember（doc/12 §5）。claim 固定为 quote 折叠空白。
    pub fn remember(
        &mut self,
        scope: &ScopeKey,
        origin: &Origin,
        user_evidence_id: &str,
        quote: &str,
        kind: MemoryKind,
    ) -> Result<RememberOutcome, StoreError> {
        // 1. 证据属当前 scope、role=user、source_kind=user、host/session 匹配。
        let (host, session, role, source_kind, content) = self
            .get_evidence(scope, user_evidence_id)?
            .ok_or(StoreError::EvidenceNotFound)?;
        if role != "user" || source_kind != "user" {
            return Err(StoreError::EvidenceNotFound);
        }
        if host != origin.host_id || session != origin.session_id {
            return Err(StoreError::EvidenceNotFound);
        }
        // 2. 必须是该会话最新用户事件。
        let (latest_id, _, _) = self
            .latest_user_event(scope, &origin.host_id, &origin.session_id)?
            .ok_or(StoreError::StaleUserEvidence)?;
        if latest_id != user_evidence_id {
            return Err(StoreError::StaleUserEvidence);
        }
        // 3. quote 是原文连续子串。
        let (start, end) = find_quote_span(&content, quote).ok_or(StoreError::QuoteMismatch)?;
        // 4. claim = 折叠空白；长度上限由 handler 校验。
        let claim = fold_whitespace(quote);
        if claim.is_empty() {
            return Err(StoreError::QuoteMismatch);
        }
        let claim_hash = claim_sha256(kind, &claim);
        let now = now_rfc3339()?;

        // 5. 去重：同 scope/kind/hash 且 active → 仅加证据。
        let existing: Option<(String, i64)> = self
            .conn()
            .query_row(
                "SELECT id, version FROM memories
                 WHERE tenant_id=?1 AND user_id=?2 AND kind=?3 AND claim_sha256=?4 AND status='active'",
                params![scope.tenant_id, scope.user_id, kind.as_str(), claim_hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((memory_id, version)) = existing {
            let tx = self.conn_mut().transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO memory_evidence (id, tenant_id, user_id, memory_id, evidence_id, start_byte, end_byte)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    Uuid::now_v7().to_string(),
                    scope.tenant_id,
                    scope.user_id,
                    memory_id,
                    user_evidence_id,
                    start as i64,
                    end as i64
                ],
            )?;
            tx.execute(
                "UPDATE memories SET updated_at=?1 WHERE tenant_id=?2 AND user_id=?3 AND id=?4",
                params![now, scope.tenant_id, scope.user_id, memory_id],
            )?;
            Self::mark_index_dirty(&tx)?;
            tx.commit()?;
            return Ok(RememberOutcome::Dedup { memory_id, version });        }

        // 6. 新建 active memory + evidence + revision + audit，同事务 dirty=1。
        let memory_id = Uuid::now_v7().to_string();
        let tx = self.conn_mut().transaction()?;
        tx.execute(
            "INSERT INTO memories
             (id, tenant_id, user_id, kind, claim, normalized_claim, claim_sha256, source_class,
              status, version, occurred_at, valid_from, valid_until, origin_host_id, origin_agent_id,
              created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,'user_explicit','active',1,NULL,NULL,NULL,?8,?9,?10,?11)",
            params![
                memory_id,
                scope.tenant_id,
                scope.user_id,
                kind.as_str(),
                claim,
                normalize_v1(&claim),
                claim_hash,
                origin.host_id,
                origin.agent_id,
                now,
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO memory_evidence (id, tenant_id, user_id, memory_id, evidence_id, start_byte, end_byte)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                Uuid::now_v7().to_string(),
                scope.tenant_id,
                scope.user_id,
                memory_id,
                user_evidence_id,
                start as i64,
                end as i64
            ],
        )?;
        tx.execute(
            "INSERT INTO memory_revisions
             (tenant_id, user_id, memory_id, version, previous_claim, new_claim, previous_status, new_status,
              actor_kind, actor_id, reason_code, changed_at)
             VALUES (?1,?2,?3,1,NULL,?4,NULL,'active','user',?5,'remember',?6)",
            params![scope.tenant_id, scope.user_id, memory_id, claim, user_evidence_id, now],
        )?;
        tx.execute(
            "INSERT INTO audit_events (id, tenant_id, user_id, actor_kind, actor_id, action, target_id, occurred_at, detail_json)
             VALUES (?1,?2,?3,'user',?4,'memory_remember',?5,?6,?7)",
            params![
                Uuid::now_v7().to_string(),
                scope.tenant_id,
                scope.user_id,
                user_evidence_id,
                memory_id,
                now,
                format!("{{\"kind\":\"{}\",\"via\":\"remember\"}}", kind.as_str())
            ],
        )?;
        Self::mark_index_dirty(&tx)?;
        tx.commit()?;

        // 7. 独立索引事务（失败不回滚规范记忆，保留 dirty）。
        if let Err(e) = self.reindex_memory(scope, &memory_id, &claim, true) {
            eprintln!("[memoryd] 索引更新失败 memory_id={memory_id}: {e}");
        }
        Ok(RememberOutcome::Created { memory_id, version: 1 })
    }

    /// GET /v1/memories/{id}。普通读只返回 active；非 active 视为不存在（doc/12 §5）。
    pub fn get_memory(&self, scope: &ScopeKey, memory_id: &str) -> Result<Option<MemoryRow>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT kind, claim, status, version, occurred_at, valid_until, origin_agent_id, updated_at
                 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((kind, claim, status, version, occurred_at, valid_until, agent, updated_at)) = row else {
            return Ok(None);
        };
        if status != "active" {
            return Ok(None);
        }
        Ok(Some(MemoryRow {
            memory_id: memory_id.to_string(),
            kind,
            claim,
            status,
            version,
            occurred_at,
            valid_until,
            origin_agent_id: agent,
            updated_at,
            evidence_refs: self.evidence_refs_of(scope, memory_id)?,
        }))
    }

    /// 搜索实现（doc/13 §6）。返回 hits 与 index_degraded。
    pub fn search_memories(
        &self,
        scope: &ScopeKey,
        query: &str,
        limit: usize,
        include_history: bool,
    ) -> Result<(Vec<SearchHit>, bool), StoreError> {
        let now = now_rfc3339()?;
        let status_clause = is_active_clause(include_history);
        let mut fts_hits: Vec<(String, f64)> = Vec::new();
        let mut gram_hits: Vec<(String, f64)> = Vec::new();

        // 路 1：FTS5（拉丁文 token）
        let tokens = latin_tokens(query);
        if let Some(m) = fts_match_query(&tokens) {
            let mut stmt = self.conn().prepare(
                "SELECT memory_id, rank FROM memory_fts WHERE memory_fts MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![m, memory_contract::SEARCH_PER_CHANNEL_LIMIT as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
            })?;
            for row in rows {
                let (id, bm25) = row?;
                fts_hits.push((id, bm25));
            }
        }

        // 路 2：Unicode 二元字索引
        let grams = cjk_bigrams(query);
        if !grams.is_empty() {
            let placeholders: Vec<String> = grams.iter().map(|_| "?".to_string()).collect();
            let sql = format!(
                "SELECT memory_id, COUNT(DISTINCT gram) AS hits FROM memory_grams
                 WHERE tenant_id=? AND user_id=? AND gram IN ({})
                 GROUP BY memory_id ORDER BY hits DESC LIMIT ?",
                placeholders.join(",")
            );
            let mut stmt = self.conn().prepare(&sql)?;
            let mut bind: Vec<&dyn rusqlite::ToSql> = Vec::new();
            bind.push(&scope.tenant_id);
            bind.push(&scope.user_id);
            for g in &grams {
                bind.push(g);
            }
            let limit_param = memory_contract::SEARCH_PER_CHANNEL_LIMIT as i64;
            bind.push(&limit_param);
            let rows = stmt.query_map(bind.as_slice(), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for (i, row) in rows.enumerate() {
                let (id, hits) = row?;
                gram_hits.push((id, hits as f64 / grams.len().max(1) as f64 * (1.0 / (i as f64 + 1.0))));
            }
        }

        // 1 字降级：最近 500 条 active 的有界子串（仅在无两路结果时）
        let single_char = is_single_char_query(query);
        let mut contains_hits: Vec<(String, f64)> = Vec::new();
        if fts_hits.is_empty() && gram_hits.is_empty() && single_char {
            let needle = normalize_v1(query);
            let mut stmt = self.conn().prepare(
                "SELECT id, claim FROM memories
                 WHERE tenant_id=? AND user_id=? AND {status}
                 ORDER BY updated_at DESC LIMIT ?",
            )?;
            let rows = stmt.query_map(
                params![scope.tenant_id, scope.user_id, memory_contract::SINGLE_CHAR_SCAN_LIMIT as i64],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?;
            for row in rows {
                let (id, claim) = row?;
                if claim.contains(&needle) {
                    contains_hits.push((id, 1.0));
                }
            }
        }

        // 融合排序
        let mut scores: std::collections::HashMap<String, (f64, &'static str)> = std::collections::HashMap::new();
        if !fts_hits.is_empty() && !gram_hits.is_empty() {
            let mut fts_rank: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
            for (i, (id, _)) in fts_hits.iter().enumerate() {
                fts_rank.insert(id.clone(), i as u32 + 1);
            }
            let mut gram_rank: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
            for (i, (id, _)) in gram_hits.iter().enumerate() {
                gram_rank.insert(id.clone(), i as u32 + 1);
            }
            let ids: std::collections::HashSet<String> =
                fts_rank.keys().chain(gram_rank.keys()).cloned().collect();
            for id in ids {
                let mut ranks = Vec::new();
                if let Some(r) = fts_rank.get(&id) {
                    ranks.push(*r);
                }
                if let Some(r) = gram_rank.get(&id) {
                    ranks.push(*r);
                }
                scores.insert(id, (rrf_score(&ranks), "rrf"));
            }
        } else if !fts_hits.is_empty() {
            for (i, (id, _)) in fts_hits.iter().enumerate() {
                scores.insert(id.clone(), (1.0 / (i as f64 + 1.0), "fts"));
            }
        } else if !gram_hits.is_empty() {
            for (i, (id, s)) in gram_hits.iter().enumerate() {
                scores.insert(id.clone(), (s / (i as f64 + 1.0), "grams"));
            }
        } else {
            for (id, s) in contains_hits {
                scores.insert(id, (s, "contains"));
            }
        }

        // 组装：join 规范表、强制 scope+状态过滤，取 limit
        let mut hits = Vec::new();
        let mut ids: Vec<(String, f64, &'static str)> =
            scores.into_iter().map(|(id, (s, r))| (id, s, r)).collect();
        ids.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (id, score, reason) in ids {
            if hits.len() >= limit {
                break;
            }
            let row = self
                .conn()
                .query_row(
                    &format!(
                        "SELECT kind, claim, status FROM memories
                         WHERE tenant_id=? AND user_id=? AND id=? AND {status_clause}
                           AND (valid_until IS NULL OR valid_until > ?)
                         ORDER BY updated_at DESC"
                    ),
                    params![scope.tenant_id, scope.user_id, id, now],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                )
                .optional()?;
            let Some((kind, claim, status)) = row else { continue };
            let refs = self
                .evidence_refs_of(scope, &id)?
                .into_iter()
                .map(|(eid, _, _)| eid)
                .collect();
            let match_reason = if include_history && status != "active" {
                format!("{reason}+history")
            } else {
                reason.to_string()
            };
            hits.push(SearchHit {
                memory_id: id,
                kind,
                claim,
                status,
                score,
                match_reason,
                evidence_refs: refs,
            });
        }
        Ok((hits, self.index_degraded()))
    }

    /// POST /v1/context/compose（doc/12 §7）。仅 active；指令类优先 2 名额；不截断语义。
    pub fn compose_context(
        &self,
        scope: &ScopeKey,
        _agent_id: &str,
        query: &str,
        max_items: usize,
        max_chars: usize,
    ) -> Result<ComposeResult, StoreError> {
        let (mut hits, degraded) = self.search_memories(scope, query, 20, false)?;
        // 指令类优先占最多 2 个名额（doc/13 §6）；stable sort 保留 score 序。
        hits.sort_by_key(|h| if h.kind == "instruction" { 0 } else { 1 });
        let mut items: Vec<(String, Vec<String>)> = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        let mut truncated = false;
        let header = format!(
            "<agent_memory scope=\"current_user\" generated_at=\"{}\">",
            now_rfc3339()?
        );
        let footer = "</agent_memory>";
        let mut body_chars = 0usize;

        for hit in hits {
            if items.len() >= max_items {
                break;
            }
            let line = format!(
                "- [memory_id={}] {} 来源：用户消息。",
                hit.memory_id, hit.claim
            );
            let entry_len = line.chars().count() + 1;
            if body_chars + entry_len > max_chars.saturating_sub(header.chars().count() + footer.chars().count()) {
                truncated = true;
                continue; // 超长单条跳过，不截断成可能失去否定词的句子（doc/13 §6）
            }
            body_chars += entry_len;
            items.push((hit.memory_id.clone(), hit.evidence_refs.clone()));
            lines.push(line);
        }
        let text = if items.is_empty() {
            String::new()
        } else {
            format!("{header}\n{}\n{footer}", lines.join("\n"))
        };
        Ok(ComposeResult { text, items, truncated, index_degraded: degraded })
    }
}
