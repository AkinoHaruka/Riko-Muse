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

#[derive(Debug)]
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
    /// 命中时的当前版本（D6-3 bundle source_versions；doc6/04 §4）。
    pub version: i64,
}

pub struct ComposeResult {
    pub text: String,
    pub items: Vec<(String, Vec<String>)>,
    pub truncated: bool,
    pub index_degraded: bool,
}

/// POST /v1/memories/{id}/correct 请求（doc/12 §6）。
pub struct CorrectRequest {
    pub expected_version: i64,
    pub origin: Origin,
    pub user_evidence_id: String,
    pub old_quote: String,
    pub replacement_quote: String,
}

pub struct CorrectOutcome {
    pub old_memory_id: String,
    pub old_version: i64,
    pub new_memory_id: String,
    pub new_version: i64,
}

/// POST /v1/memories/{id}/forget 请求（doc/12 §6）。
pub struct ForgetRequest {
    pub expected_version: i64,
    pub origin: Origin,
    pub user_evidence_id: String,
    pub target_quote: String,
}

pub struct ForgetOutcome {
    pub memory_id: String,
    pub version: i64,
}

/// 遗忘动词（doc/12 §6）：首版中文/英文固定集。
pub fn has_forget_cue(text: &str) -> bool {
    let t = text.to_lowercase();
    ["忘记", "删除记忆", "不要再记得", "forget", "delete this memory"]
        .iter()
        .any(|w| t.contains(w))
}

/// 保存指令与目标 quote 的直接相邻判定（doc5/04 §2）：消息去首尾空白后以
/// 「请记住」「记住」「请帮我记住」或 `Remember` 开始（英文大小写折叠），允许其后
/// 一个 `:`/`：` 与空白，随后内容必须恰为待保存 quote（可带一个句末标点）。
/// 保存的 quote 不改写；前一句的保存意图不扩展到后一句。
fn has_adjacent_save_instruction(message: &str, quote: &str) -> bool {
    const PREFIXES: [&str; 3] = ["请帮我记住", "请记住", "记住"];
    const EN_PREFIX: &str = "remember";
    const TRAILING: [char; 6] = ['。', '.', '！', '!', '？', '?'];
    let msg = message.trim();
    let rest: &str = {
        // 中文按字面前缀；英文对整条消息做小写折叠后定位前缀长度。
        let mut found: Option<&str> = None;
        for p in PREFIXES {
            if let Some(r) = msg.strip_prefix(p) {
                found = Some(r);
                break;
            }
        }
        if found.is_none() && msg.to_lowercase().starts_with(EN_PREFIX) {
            found = msg.get(EN_PREFIX.len()..);
        }
        match found {
            Some(r) => r,
            None => return false,
        }
    };
    let rest = match rest.strip_prefix([':', '：']) {
        Some(r) => r.trim_start(),
        None => rest,
    };
    if rest == quote {
        return true;
    }
    // quote 后可带一个句末标点（保存的 quote 不含该标点时）。
    rest.strip_suffix(TRAILING).map(|r| r.trim_end() == quote).unwrap_or(false)
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

    pub(crate) fn evidence_refs_of(
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
        // 3.5 直写内容护栏已全部解除（用户产品决定，2026-09-25 深夜）：memory_remember
        // 不再做内容类别判定、不再要求相邻保存指令、不再限制单命题——Agent 可主动
        // 写入任何内容。保留的仅是协议级校验：scope、最新用户证据、quote 逐字 span、
        // 去重与审计。原 doc5/04 §2 窄门与 doc5/09 决策 B 由用户决定撤销，护栏待
        // 用户后续统一重写（历史实现见 git 4cb7eb5 / ea23c29 之前的 remembers 门）。
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

    /// 只读取当前 claim（resident export 等视图用）；跨 scope/不存在返回 None。
    pub fn get_memory_claim(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let claim = self
            .conn()
            .query_row(
                "SELECT claim FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(claim)
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

        // 历史路：派生索引按 doc/11 §1 排除 superseded/expired，历史查询直接对
        // 规范表做有界子串扫描（forgotten 永不返回）。
        let mut history_hits: Vec<(String, f64)> = Vec::new();
        if include_history {
            let tokens = latin_tokens(query);
            let needle = normalize_v1(query);
            let mut stmt = self.conn().prepare(
                "SELECT id, claim FROM memories
                 WHERE tenant_id=? AND user_id=? AND status IN ('superseded','expired')
                 ORDER BY updated_at DESC LIMIT ?",
            )?;
            let rows = stmt.query_map(
                params![scope.tenant_id, scope.user_id, memory_contract::SINGLE_CHAR_SCAN_LIMIT as i64],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?;
            for row in rows {
                let (id, claim) = row?;
                let nclaim = normalize_v1(&claim);
                let token_match = !tokens.is_empty() && tokens.iter().all(|t| nclaim.contains(t));
                let substring_match = !needle.is_empty() && nclaim.contains(&needle);
                if token_match || substring_match {
                    history_hits.push((id, 0.5));
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
        for (id, s) in history_hits {
            scores.entry(id).or_insert((s, "history"));
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
                        "SELECT kind, claim, status, version FROM memories
                         WHERE tenant_id=? AND user_id=? AND id=? AND {status_clause}
                           AND (valid_until IS NULL OR valid_until > ?)
                         ORDER BY updated_at DESC"
                    ),
                    params![scope.tenant_id, scope.user_id, id, now],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some((kind, claim, status, version)) = row else { continue };
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
                version,
            });
        }
        Ok((hits, self.index_degraded()))
    }

    fn memory_row_for_update(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
    ) -> Result<Option<(String, String, i64, String, String)>, StoreError> {
        // (kind, claim, version, status, claim_sha256)
        let row = self
            .conn()
            .query_row(
                "SELECT kind, claim, version, status, claim_sha256 FROM memories
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// 校验「最新真实用户消息」前提（correct/forget 共用）。
    fn check_latest_user_evidence(
        &self,
        scope: &ScopeKey,
        origin: &Origin,
        user_evidence_id: &str,
    ) -> Result<(String, i64, i64), StoreError> {
        let (host, session, role, source_kind, content) = self
            .get_evidence(scope, user_evidence_id)?
            .ok_or(StoreError::EvidenceNotFound)?;
        if role != "user" || source_kind != "user" || host != origin.host_id || session != origin.session_id {
            return Err(StoreError::StaleUserEvidence);
        }
        let (latest_id, _, _) = self
            .latest_user_event(scope, &origin.host_id, &origin.session_id)?
            .ok_or(StoreError::StaleUserEvidence)?;
        if latest_id != user_evidence_id {
            return Err(StoreError::StaleUserEvidence);
        }
        // 返回 content 与 span（整个事件）
        let len = content.len() as i64;
        Ok((content, 0, len))
    }

    /// POST /v1/memories/{id}/correct（doc/12 §6）。
    pub fn correct_memory(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        req: &CorrectRequest,
    ) -> Result<CorrectOutcome, StoreError> {
        let Some((kind, claim, version, status, claim_hash)) = self.memory_row_for_update(scope, memory_id)? else {
            return Err(StoreError::MemoryNotFound);
        };
        if status != "active" {
            return Err(StoreError::MemoryNotFound);
        }
        if version != req.expected_version {
            return Err(StoreError::VersionConflict);
        }
        let (content, _, _) = self.check_latest_user_evidence(scope, &req.origin, &req.user_evidence_id)?;
        // 最新用户事件须同时包含 old_quote 与 replacement_quote；old_quote 须在旧 claim 中出现。
        let Some((rstart, rend)) = find_quote_span(&content, &req.replacement_quote) else {
            return Err(StoreError::QuoteMismatch);
        };
        if find_quote_span(&content, &req.old_quote).is_none() {
            return Err(StoreError::QuoteMismatch);
        }
        if find_quote_span(&claim, &req.old_quote).is_none() {
            return Err(StoreError::AmbiguousTarget);
        }
        let new_claim = fold_whitespace(&req.replacement_quote);
        let new_hash = claim_sha256(
            match kind.as_str() {
                "preference" => MemoryKind::Preference,
                "instruction" => MemoryKind::Instruction,
                "episode" => MemoryKind::Episode,
                _ => MemoryKind::Fact,
            },
            &new_claim,
        );
        let now = now_rfc3339()?;
        let new_memory_id = Uuid::now_v7().to_string();
        let tx = self.conn_mut().transaction()?;
        // 旧记忆 superseded + 乐观锁。
        let n = tx.execute(
            "UPDATE memories SET status='superseded', version=version+1, updated_at=?1
             WHERE tenant_id=?2 AND user_id=?3 AND id=?4 AND version=?5",
            params![now, scope.tenant_id, scope.user_id, memory_id, req.expected_version],
        )?;
        if n == 0 {
            return Err(StoreError::VersionConflict);
        }
        tx.execute(
            "INSERT INTO memory_revisions
             (tenant_id, user_id, memory_id, version, previous_claim, new_claim, previous_status, new_status,
              actor_kind, actor_id, reason_code, changed_at)
             VALUES (?1,?2,?3,?4,?5,?6,'active','superseded','user',?7,'user_correct',?8)",
            params![scope.tenant_id, scope.user_id, memory_id, version + 1, claim, new_claim, req.user_evidence_id, now],
        )?;
        // 新记忆 active。
        tx.execute(
            "INSERT INTO memories
             (id, tenant_id, user_id, kind, claim, normalized_claim, claim_sha256, source_class,
              status, version, occurred_at, valid_from, valid_until, origin_host_id, origin_agent_id, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,'user_explicit','active',1,NULL,NULL,NULL,?8,?9,?10,?11)",
            params![
                new_memory_id, scope.tenant_id, scope.user_id, kind, new_claim,
                normalize_v1(&new_claim), new_hash, req.origin.host_id, req.origin.agent_id, now, now
            ],
        )?;
        tx.execute(
            "INSERT INTO memory_evidence (id, tenant_id, user_id, memory_id, evidence_id, start_byte, end_byte)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![Uuid::now_v7().to_string(), scope.tenant_id, scope.user_id, new_memory_id, req.user_evidence_id, rstart as i64, rend as i64],
        )?;
        tx.execute(
            "INSERT INTO memory_revisions
             (tenant_id, user_id, memory_id, version, previous_claim, new_claim, previous_status, new_status,
              actor_kind, actor_id, reason_code, changed_at)
             VALUES (?1,?2,?3,1,NULL,?4,NULL,'active','user',?5,'user_correct',?6)",
            params![scope.tenant_id, scope.user_id, new_memory_id, new_claim, req.user_evidence_id, now],
        )?;
        tx.execute(
            "INSERT INTO memory_relations (tenant_id, user_id, from_memory_id, to_memory_id, kind, created_at)
             VALUES (?1,?2,?3,?4,'supersedes',?5)",
            params![scope.tenant_id, scope.user_id, new_memory_id, memory_id, now],
        )?;
        tx.execute(
            "INSERT INTO audit_events (id, tenant_id, user_id, actor_kind, actor_id, action, target_id, occurred_at, detail_json)
             VALUES (?1,?2,?3,'user',?4,'memory_correct',?5,?6,?7)",
            params![
                Uuid::now_v7().to_string(), scope.tenant_id, scope.user_id, req.user_evidence_id,
                new_memory_id, now,
                format!("{{\"old_memory_id\":\"{memory_id}\",\"old_claim_hash\":\"{claim_hash}\"}}")
            ],
        )?;
        Self::mark_index_dirty(&tx)?;
        tx.commit()?;
        // 索引事务：旧删新插。
        if let Err(e) = self.reindex_memory(scope, memory_id, &claim, false) {
            eprintln!("[memoryd] 索引删除失败 memory_id={memory_id}: {e}");
        }
        if let Err(e) = self.reindex_memory(scope, &new_memory_id, &new_claim, true) {
            eprintln!("[memoryd] 索引插入失败 memory_id={new_memory_id}: {e}");
        }
        Ok(CorrectOutcome {
            old_memory_id: memory_id.to_string(),
            old_version: version + 1,
            new_memory_id,
            new_version: 1,
        })
    }

    /// POST /v1/memories/{id}/forget（doc/12 §6）。已 forgotten 的幂等确认不卡版本。
    pub fn forget_memory(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        req: &ForgetRequest,
    ) -> Result<ForgetOutcome, StoreError> {
        let Some((kind, claim, version, status, claim_hash)) = self.memory_row_for_update(scope, memory_id)? else {
            return Err(StoreError::MemoryNotFound);
        };
        let _ = kind;
        let (content, _, _) = self.check_latest_user_evidence(scope, &req.origin, &req.user_evidence_id)?;
        if !has_forget_cue(&content) {
            return Err(StoreError::AmbiguousTarget);
        }
        // G-13（doc2/04 §3）：target_quote 必须同时定位到用户最新消息正文与目标 claim，
        // 否则带 ID + 泛称"忘记"可能误删未被用户明确指认的记忆。
        if req.target_quote.trim().is_empty() || find_quote_span(&content, &req.target_quote).is_none() {
            return Err(StoreError::AmbiguousTarget);
        }
        if find_quote_span(&claim, &req.target_quote).is_none() {
            return Err(StoreError::AmbiguousTarget);
        }
        if status == "forgotten" {
            // 幂等确认：同一目标再次明确请求 → 返回当前状态（doc/12 §6）。
            return Ok(ForgetOutcome { memory_id: memory_id.to_string(), version });
        }
        if status != "active" {
            return Err(StoreError::MemoryNotFound);
        }
        if version != req.expected_version {
            return Err(StoreError::VersionConflict);
        }
        let now = now_rfc3339()?;
        let refs = self.evidence_refs_of(scope, memory_id)?;
        let tx = self.conn_mut().transaction()?;
        let n = tx.execute(
            "UPDATE memories SET status='forgotten', version=version+1, updated_at=?1
             WHERE tenant_id=?2 AND user_id=?3 AND id=?4 AND version=?5",
            params![now, scope.tenant_id, scope.user_id, memory_id, req.expected_version],
        )?;
        if n == 0 {
            return Err(StoreError::VersionConflict);
        }
        tx.execute(
            "INSERT INTO memory_revisions
             (tenant_id, user_id, memory_id, version, previous_claim, new_claim, previous_status, new_status,
              actor_kind, actor_id, reason_code, changed_at)
             VALUES (?1,?2,?3,?4,?5,?6,'active','forgotten','user',?7,'user_forget',?8)",
            params![scope.tenant_id, scope.user_id, memory_id, version + 1, claim, claim, req.user_evidence_id, now],
        )?;
        // 抑制源：每个证据引用一行，防后台重放复活（doc/13 §7）。
        for (evidence_id, _, _) in &refs {
            tx.execute(
                "INSERT OR IGNORE INTO suppressed_sources
                 (tenant_id, user_id, evidence_id, claim_sha256, forgotten_memory_id, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![scope.tenant_id, scope.user_id, evidence_id, claim_hash, memory_id, now],
            )?;
        }
        tx.execute(
            "INSERT INTO audit_events (id, tenant_id, user_id, actor_kind, actor_id, action, target_id, occurred_at, detail_json)
             VALUES (?1,?2,?3,'user',?4,'memory_forget',?5,?6,'{\"raw_evidence_retained\":true}')",
            params![Uuid::now_v7().to_string(), scope.tenant_id, scope.user_id, req.user_evidence_id, memory_id, now],
        )?;
        Self::mark_index_dirty(&tx)?;
        tx.commit()?;
        if let Err(e) = self.reindex_memory(scope, memory_id, &claim, false) {
            eprintln!("[memoryd] 索引删除失败 memory_id={memory_id}: {e}");
        }
        Ok(ForgetOutcome { memory_id: memory_id.to_string(), version: version + 1 })
    }

    /// 从 active 规范表全量重建派生索引（CLI rebuild-index，doc/09）。
    pub fn rebuild_index(&mut self) -> Result<(u64, u64), StoreError> {
        let tx = self.conn_mut().transaction()?;
        tx.execute("DELETE FROM memory_fts", [])?;
        let grams = tx.execute("DELETE FROM memory_grams", [])?;
        let mut rows: Vec<(String, String, String, String)> = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT m.id, m.claim, m.tenant_id, m.user_id FROM memories m WHERE m.status='active'",
            )?;
            let mut q = stmt.query([])?;
            while let Some(row) = q.next()? {
                rows.push((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?));
            }
        }
        let mut fts = 0u64;
        for (id, claim, tenant_id, user_id) in &rows {
            tx.execute(
                "INSERT INTO memory_fts (memory_id, claim) VALUES (?1, ?2)",
                params![id, normalize_v1(claim)],
            )?;
            fts += 1;
            for gram in cjk_bigrams(claim) {
                tx.execute(
                    "INSERT OR IGNORE INTO memory_grams (tenant_id, user_id, memory_id, gram) VALUES (?1,?2,?3,?4)",
                    params![tenant_id, user_id, id, gram],
                )?;
            }
        }
        Self::clear_index_dirty(&tx)?;
        tx.commit()?;
        Ok((fts, grams as u64))
    }

    /// SQLite 在线一致性备份：VACUUM INTO（停机维护语义见 doc/09）。
    pub fn backup_to(&mut self, target: &std::path::Path) -> Result<(), StoreError> {
        let path = target.to_string_lossy().replace('\'', "''");
        self.conn_mut()
            .execute(&format!("VACUUM INTO '{path}'"), [])?;
        Ok(())
    }

    /// POST /v1/context/compose（doc/12 §7）。仅 active；指令类优先；不截断语义。
    pub fn compose_context(
        &self,
        scope: &ScopeKey,
        _agent_id: &str,
        query: &str,
        max_items: usize,
        max_chars: usize,
    ) -> Result<ComposeResult, StoreError> {
        let (hits, degraded) = self.search_memories(scope, query, 20, false)?;
        // doc/13 §6：长期指令先占最多 2 个名额，再填当前查询相关记录。
        // 指令名额独立于查询——词法未命中的 active 指令也必须进入，否则
        // 「以后回答请始终用中文」在无关查询（如闲聊）下失效
        //（2026-09-25 真实模型实测暴露的契约落差）。
        let mut seen = std::collections::HashSet::new();
        let mut ordered: Vec<SearchHit> = Vec::new();
        let mut instruction_slots = 0usize;
        // 先收查询命中的指令（既是指令又相关，按 score 序）。
        for h in hits.iter().filter(|h| h.kind == "instruction") {
            if instruction_slots >= 2 {
                break;
            }
            seen.insert(h.memory_id.clone());
            ordered.push(h.clone());
            instruction_slots += 1;
        }
        // 名额未满时按 updated_at DESC 补齐未命中的 active 指令。
        if instruction_slots < 2 {
            for h in self.active_instruction_hits(scope, 2)? {
                if seen.insert(h.memory_id.clone()) {
                    ordered.push(h);
                    instruction_slots += 1;
                    if instruction_slots >= 2 {
                        break;
                    }
                }
            }
        }
        // 再填非指令的查询相关记录。
        for h in hits.iter().filter(|h| h.kind != "instruction") {
            if seen.insert(h.memory_id.clone()) {
                ordered.push(h.clone());
            }
        }
        let mut items: Vec<(String, Vec<String>)> = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        let mut truncated = false;
        let header = format!(
            "<agent_memory scope=\"current_user\" generated_at=\"{}\">",
            now_rfc3339()?
        );
        let footer = "</agent_memory>";
        let mut body_chars = 0usize;

        for hit in ordered {
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

    /// active 指令类记忆（compose 指令名额补齐用），updated_at DESC 固定序。
    fn active_instruction_hits(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let now = now_rfc3339()?;
        let ids: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT id FROM memories
                 WHERE tenant_id=?1 AND user_id=?2 AND kind='instruction' AND status='active'
                   AND (valid_until IS NULL OR valid_until > ?3)
                 ORDER BY updated_at DESC, id ASC LIMIT ?4",
            )?;
            let rows = stmt.query_map(
                params![scope.tenant_id, scope.user_id, now, limit as i64],
                |r| r.get::<_, String>(0),
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut hits = Vec::new();
        for id in ids {
            let row = self
                .conn()
                .query_row(
                    "SELECT kind, claim, status, version FROM memories
                     WHERE tenant_id=? AND user_id=? AND id=? AND status='active'
                       AND (valid_until IS NULL OR valid_until > ?)",
                    params![scope.tenant_id, scope.user_id, id, now],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some((kind, claim, status, version)) = row else { continue };
            let _ = kind;
            let refs = self
                .evidence_refs_of(scope, &id)?
                .into_iter()
                .map(|(eid, _, _)| eid)
                .collect();
            hits.push(SearchHit {
                memory_id: id,
                kind: "instruction".into(),
                claim,
                status,
                score: 0.0,
                match_reason: "instruction".into(),
                evidence_refs: refs,
                version,
            });
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IngestOutcome;
    use memory_domain::{MemoryKind, Origin};

    fn migrations_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join("migrations")
    }

    fn setup(tag: &str) -> (Store, ScopeKey) {
        let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
        let dir = std::env::temp_dir().join(format!("am-mem-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        store.principal_add("t", "u", &dir.join("u.token")).unwrap();
        let token = std::fs::read_to_string(dir.join("u.token")).unwrap();
        let scope = store.verify_token(token.trim()).unwrap().unwrap();
        (store, scope)
    }

    fn origin() -> Origin {
        Origin { host_id: "dsh".into(), agent_id: "a".into(), session_id: "s".into() }
    }

    fn ingest_user(store: &mut Store, scope: &ScopeKey, seq: i64, content: &str) -> String {
        let t = chrono::Utc::now();
        match store.record_evidence(scope, &origin(), seq, "user", "user", &t, content).unwrap() {
            IngestOutcome::Recorded(id) => id,
            IngestOutcome::AlreadyRecorded(id) => id,
        }
    }

    #[test]
    fn compose_includes_active_instruction_without_lexical_hit() {
        // doc/13 §6：长期指令先占最多 2 个名额，独立于查询词法命中。
        // 回归（2026-09-25 真实模型实测）：无关查询下 active 指令缺失 → 注入失效。
        let (mut store, scope) = setup("compose-instr");
        let ev = ingest_user(&mut store, &scope, 8, "以后回答请始终用中文。今天先聊到这");
        store
            .remember(&scope, &origin(), &ev, "以后回答请始终用中文", MemoryKind::Instruction)
            .unwrap();
        // 与指令零词法重叠的查询也要召回该指令。
        let r = store.compose_context(&scope, "a", "怎么做蛋炒饭", 5, 2000).unwrap();
        assert_eq!(r.items.len(), 1);
        assert!(r.text.contains("以后回答请始终用中文"));
        assert!(r.items[0].1.iter().any(|e| e == &ev), "注入项须携带证据引用");
        // 指令名额上限 2：第三条指令不得进入名额。
        let ev2 = ingest_user(&mut store, &scope, 16, "以后回答要给代码示例");
        store.remember(&scope, &origin(), &ev2, "以后回答要给代码示例", MemoryKind::Instruction).unwrap();
        let ev3 = ingest_user(&mut store, &scope, 24, "以后先说明风险再动手");
        store.remember(&scope, &origin(), &ev3, "以后先说明风险再动手", MemoryKind::Instruction).unwrap();
        let r2 = store.compose_context(&scope, "a", "怎么做蛋炒饭", 5, 2000).unwrap();
        // 三条指令但名额只有 2：文本恰好包含两条 claim（最新的 updated_at DESC 两条）。
        assert!(r2.text.contains("以后回答要给代码示例"));
        assert!(r2.text.contains("以后先说明风险再动手"));
        assert!(!r2.text.contains("以后回答请始终用中文"), "最旧的一条被挤出 2 个名额");
    }

    #[test]
    fn remember_direct_write_no_content_gates() {
        // 用户产品决定（2026-09-25 深夜）：直写内容护栏全部解除——任何内容、
        // 有无保存指令、单命题或复合句，均按协议校验后 active；Agent 可主动写入。
        // 保留协议级校验：最新证据、quote 逐字 span、跨用户隔离、幂等去重。
        let (mut store, scope) = setup("remember-open");
        let o = origin();
        let created = |r: Result<RememberOutcome, StoreError>| -> String {
            match r.unwrap() {
                RememberOutcome::Created { memory_id, .. } => memory_id,
                other => panic!("期望 Created，实际 {other:?}"),
            }
        };
        // Agent 主动写入（无任何保存指令）：普通偏好。
        let ev1 = ingest_user(&mut store, &scope, 1, "我喜欢暗色主题");
        created(store.remember(&scope, &o, &ev1, "我喜欢暗色主题", MemoryKind::Preference));
        // 健康、凭据、时间性、第三人：无指令均 active（原四类门解除）。
        let ev2 = ingest_user(&mut store, &scope, 2, "我对花生过敏");
        let id1 = created(store.remember(&scope, &o, &ev2, "我对花生过敏", MemoryKind::Fact));
        let ev3 = ingest_user(&mut store, &scope, 3, "我的密码：abcd1234");
        created(store.remember(&scope, &o, &ev3, "我的密码：abcd1234", MemoryKind::Fact));
        let ev4 = ingest_user(&mut store, &scope, 4, "我今年九月开始新工作");
        created(store.remember(&scope, &o, &ev4, "我今年九月开始新工作", MemoryKind::Fact));
        let ev5 = ingest_user(&mut store, &scope, 5, "我姐在成都教书");
        created(store.remember(&scope, &o, &ev5, "我姐在成都教书", MemoryKind::Fact));
        // 复合命题（原 doc5/09 决策 B 拒绝样本）→ active。
        let ev6 = ingest_user(&mut store, &scope, 6, "我对芒果过敏，以后别再推荐含芒果的甜品");
        created(store.remember(&scope, &o, &ev6, "我对芒果过敏，以后别再推荐含芒果的甜品", MemoryKind::Preference));
        // 幂等不变：同 claim 重复 remember → Dedup（仅加证据，不新建）。
        let ev7 = ingest_user(&mut store, &scope, 7, "我对花生过敏");
        match store.remember(&scope, &o, &ev7, "我对花生过敏", MemoryKind::Fact).unwrap() {
            RememberOutcome::Dedup { memory_id, .. } => assert_eq!(memory_id, id1),
            other => panic!("期望 Dedup，实际 {other:?}"),
        }
        // quote 不逐字仍拒绝（协议校验，非内容护栏）。
        let ev8 = ingest_user(&mut store, &scope, 8, "今天聊到这");
        assert!(matches!(
            store.remember(&scope, &o, &ev8, "这句话不在消息里", MemoryKind::Fact),
            Err(StoreError::QuoteMismatch)
        ));
        // stale 证据仍拒绝。
        assert!(matches!(
            store.remember(&scope, &o, &ev2, "我对花生过敏", MemoryKind::Fact),
            Err(StoreError::StaleUserEvidence)
        ));
        // 跨用户隔离不变：另一 scope 的 evidence id → EvidenceNotFound，不泄露存在性。
        let dir = std::env::temp_dir().join(format!("am-mem-open-u2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        store.principal_add("t", "u2", &dir.join("u2.token")).unwrap();
        let scope2 = ScopeKey { tenant_id: "t".into(), user_id: "u2".into() };
        assert!(matches!(
            store.remember(&scope2, &o, &ev2, "我对花生过敏", MemoryKind::Fact),
            Err(StoreError::EvidenceNotFound)
        ));
    }
}
