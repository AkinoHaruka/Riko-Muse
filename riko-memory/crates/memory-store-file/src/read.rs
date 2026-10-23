//! Read path: visibility filtering, explain models, search (lexical + vector RRF + stale scan fallback),
//! and context bundle compilation conforming to the 4-segment budget.

use crate::index::{IndexFreshness, ProjectionIndex};
use crate::vectors::{cosine_similarity, VectorStore};
use crate::{now, raw_content_hash, read_bytes, suppressed, EvidenceBlock, StoreError, Suppression};
use memory_domain::{normalize_v1, USER_MAIN_DOMAIN};
use memory_file_format::claim::ClaimDocument;
use memory_file_format::overview::MemoryOverview;
use memory_recall::{cjk_bigrams, is_single_char_query, latin_tokens, rrf_score};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const BUNDLE_TOTAL_CHARS: usize = 8000;
pub const BUNDLE_COMPACT_CHARS: usize = 1200;
pub const BUNDLE_ALIGNMENT_CHARS: usize = 600;
pub const BUNDLE_RELATIONSHIP_ITEMS: usize = 8;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EvidenceDetail {
    pub evidence_id: String,
    pub start_byte: Option<i64>,
    pub end_byte: Option<i64>,
    pub quote: String,
    pub span_exact: bool,
    pub role: String,
    pub source_kind: String,
    pub occurred_at: String,
    pub host_id: String,
    pub session_id: String,
    pub suppressed: bool,
    pub tombstoned: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct ExplainRelations {
    pub superseded_by: Option<String>,
    pub supersedes: Option<String>,
    pub retired: bool,
    pub retirement_reason_code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryExplain {
    pub memory_id: String,
    pub kind: String,
    pub claim: String,
    pub status: String,
    pub version: i64,
    pub domain_id: String,
    pub source_class: String,
    pub occurred_at: Option<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub evidence: Vec<EvidenceDetail>,
    pub speaker: Option<String>,
    pub subject: Option<String>,
    pub subject_source: &'static str,
    pub reason_code: Option<String>,
    pub reason_text: Option<String>,
    pub relations: ExplainRelations,
    pub stable_ref: String,
    pub visible: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileSearchHit {
    pub id: String,
    pub kind: String,        // "claim" | "entry"
    pub memory_kind: String, // "fact", "preference", etc. (or "dialogue")
    pub content: String,
    pub score: f64,
    pub channels: Vec<String>,
    pub occurred_at: Option<String>,
    pub version: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileBundleResult {
    pub segments: serde_json::Value,
    pub budget: serde_json::Value,
    pub resident: serde_json::Value,
    pub retrieved: serde_json::Value,
    pub lexical_status: &'static str,
    pub semantic_status: &'static str,
}

pub fn speaker_of(role: &str, source_kind: &str) -> Option<String> {
    let r = role.to_ascii_lowercase();
    match r.as_str() {
        "user" => Some("user".into()),
        "assistant" => Some("assistant".into()),
        "system" => Some("system".into()),
        "plugin" => Some("plugin".into()),
        _ => {
            let s = source_kind.to_ascii_lowercase();
            match s.as_str() {
                "user" => Some("user".into()),
                "assistant" => Some("assistant".into()),
                "system" => Some("system".into()),
                "plugin" => Some("plugin".into()),
                _ => None,
            }
        }
    }
}

pub fn is_claim_visible(
    claim: &ClaimDocument,
    suppressions: &[Suppression],
    all_evidence: &[EvidenceBlock],
    include_history: bool,
    now_time: &str,
) -> bool {
    // 1. Status
    if claim.front.status == "forgotten" {
        return false;
    }
    if !include_history && claim.front.status != "active" {
        return false;
    }

    // 2. valid_until
    if !include_history {
        if let Some(ref until) = claim.front.valid_until {
            if until.as_str() <= now_time {
                return false;
            }
        }
    }

    // 3. Suppressions
    let raw_hash = raw_content_hash(&claim.body);
    if suppressed(suppressions, &claim.front.id)
        || suppressed(suppressions, &raw_hash)
        || suppressed(suppressions, &claim.front.claim_sha256)
    {
        return false;
    }

    // 4. Evidence existence and suppressions
    let has_valid_source = claim.front.evidence.iter().any(|ev_ref| {
        if suppressed(suppressions, &ev_ref.id) || suppressed(suppressions, &ev_ref.content_sha256) {
            return false;
        }
        all_evidence.iter().any(|b| b.header.id == ev_ref.id)
    });

    has_valid_source
}

pub struct FileReader;

impl FileReader {
    /// Read and explain a memory claim with full source spans and relation graph.
    pub fn explain(
        scope_dir: &Path,
        memory_id: &str,
        include_history: bool,
        all_evidence: &[EvidenceBlock],
        all_claims: &[ClaimDocument],
        suppressions: &[Suppression],
    ) -> Result<Option<MemoryExplain>, StoreError> {
        let claim_p = scope_dir.join("memory/claims").join(format!("{memory_id}.md"));
        if !claim_p.exists() {
            return Ok(None);
        }
        let bytes = read_bytes(&claim_p)?;
        let claim = match ClaimDocument::parse(&bytes) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };

        if claim.front.status == "forgotten" {
            return Ok(None);
        }
        let raw_hash = raw_content_hash(&claim.body);
        if suppressed(suppressions, memory_id) || suppressed(suppressions, &raw_hash) {
            return Ok(None);
        }

        let now_time = now();
        let visible = is_claim_visible(&claim, suppressions, all_evidence, include_history, &now_time);
        if !include_history && !visible {
            return Ok(None);
        }

        let mut evidence_details = Vec::new();
        let mut first_speaker = None;

        for ev_ref in &claim.front.evidence {
            let is_supp = suppressed(suppressions, &ev_ref.id)
                || suppressed(suppressions, &ev_ref.content_sha256);
            if let Some(block) = all_evidence.iter().find(|b| b.header.id == ev_ref.id) {
                if first_speaker.is_none() {
                    first_speaker = speaker_of(&block.header.role, &block.header.source_kind);
                }
                let (quote, span_exact) = match (ev_ref.start_byte, ev_ref.end_byte) {
                    (Some(s), Some(e)) if s <= e && (e as usize) <= block.payload.len() => {
                        let sub = &block.payload[s as usize..e as usize];
                        (sub.to_string(), true)
                    }
                    _ => (block.payload.clone(), false),
                };
                evidence_details.push(EvidenceDetail {
                    evidence_id: ev_ref.id.clone(),
                    start_byte: ev_ref.start_byte.map(|v| v as i64),
                    end_byte: ev_ref.end_byte.map(|v| v as i64),
                    quote,
                    span_exact,
                    role: block.header.role.clone(),
                    source_kind: block.header.source_kind.clone(),
                    occurred_at: block.header.occurred_at.clone(),
                    host_id: block.header.host_id.clone(),
                    session_id: block.header.session_id.clone(),
                    suppressed: is_supp,
                    tombstoned: false,
                });
            } else {
                evidence_details.push(EvidenceDetail {
                    evidence_id: ev_ref.id.clone(),
                    start_byte: ev_ref.start_byte.map(|v| v as i64),
                    end_byte: ev_ref.end_byte.map(|v| v as i64),
                    quote: String::new(),
                    span_exact: false,
                    role: "unknown".into(),
                    source_kind: "unknown".into(),
                    occurred_at: String::new(),
                    host_id: String::new(),
                    session_id: String::new(),
                    suppressed: is_supp,
                    tombstoned: true,
                });
            }
        }

        let superseded_by = all_claims
            .iter()
            .find(|c| c.front.supersedes.as_deref() == Some(memory_id) && c.front.status != "forgotten")
            .map(|c| c.front.id.clone());

        Ok(Some(MemoryExplain {
            memory_id: claim.front.id.clone(),
            kind: claim.front.kind.clone(),
            claim: claim.body.clone(),
            status: claim.front.status.clone(),
            version: claim.front.version as i64,
            domain_id: claim.front.domain_id.clone(),
            source_class: claim.front.source_class.clone(),
            occurred_at: claim.front.occurred_at.clone(),
            valid_from: claim.front.valid_from.clone(),
            valid_until: claim.front.valid_until.clone(),
            created_at: claim.front.created_at.clone(),
            updated_at: claim.front.updated_at.clone(),
            evidence: evidence_details,
            speaker: first_speaker,
            subject: None,
            subject_source: "none",
            reason_code: Some(claim.front.reason_code.clone()),
            reason_text: claim.front.reason_text.clone(),
            relations: ExplainRelations {
                superseded_by,
                supersedes: claim.front.supersedes.clone(),
                retired: false,
                retirement_reason_code: None,
            },
            stable_ref: format!("riko://memory/{}", claim.front.id),
            visible,
        }))
    }

    /// Hybrid search: lexical (FTS5 + CJK bigrams) + vector channel merged via RRF.
    /// If index is missing or stale, gracefully falls back to bounded direct full scan
    /// and flags `degraded = true`.
    pub fn search(
        scope_dir: &Path,
        query: &str,
        limit: usize,
        include_history: bool,
        query_vector: Option<&[f32]>,
        all_evidence: &[EvidenceBlock],
        all_claims: &[ClaimDocument],
        suppressions: &[Suppression],
    ) -> Result<(Vec<FileSearchHit>, bool), StoreError> {
        let freshness = ProjectionIndex::check_freshness(scope_dir)?;
        if freshness == IndexFreshness::Fresh {
            Self::search_with_index(
                scope_dir,
                query,
                limit,
                include_history,
                query_vector,
                all_evidence,
                all_claims,
                suppressions,
            )
        } else {
            // Degraded scan fallback
            let hits = Self::search_direct_scan(
                query,
                limit,
                include_history,
                query_vector,
                scope_dir,
                all_evidence,
                all_claims,
                suppressions,
            )?;
            Ok((hits, true))
        }
    }

    fn search_with_index(
        scope_dir: &Path,
        query: &str,
        limit: usize,
        include_history: bool,
        query_vector: Option<&[f32]>,
        all_evidence: &[EvidenceBlock],
        all_claims: &[ClaimDocument],
        suppressions: &[Suppression],
    ) -> Result<(Vec<FileSearchHit>, bool), StoreError> {
        let db_path = ProjectionIndex::db_path(scope_dir);
        let conn = Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| StoreError::Metadata(e.to_string()))?;

        let mut lexical_ranked = Vec::new();
        let mut seen_lexical = HashSet::new();

        // 1. FTS5 path (latin tokens)
        let tokens = latin_tokens(query);
        if !tokens.is_empty() {
            let fts_query = tokens.join(" ");
            let mut stmt = conn
                .prepare(
                    "SELECT object_id, object_kind, rank FROM fts_items
                     WHERE fts_items MATCH ?1
                     ORDER BY rank, CASE object_kind WHEN 'claim' THEN 0 ELSE 1 END
                     LIMIT 100",
                )
                .map_err(|e| StoreError::Metadata(e.to_string()))?;

            let rows = stmt
                .query_map([&fts_query], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| StoreError::Metadata(e.to_string()))?;

            for r in rows {
                if let Ok((id, kind)) = r {
                    let key = format!("{kind}:{id}");
                    if seen_lexical.insert(key.clone()) {
                        lexical_ranked.push((id, kind));
                    }
                }
            }
        }

        // 2. CJK bigrams path
        let grams = cjk_bigrams(query);
        if !grams.is_empty() {
            let placeholders = grams.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT object_id, object_kind, COUNT(DISTINCT gram) AS hits FROM cjk_grams
                 WHERE gram IN ({placeholders})
                 GROUP BY object_id, object_kind
                 ORDER BY hits DESC, CASE object_kind WHEN 'claim' THEN 0 ELSE 1 END, object_id ASC
                 LIMIT 100"
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StoreError::Metadata(e.to_string()))?;

            let params_slice: Vec<&dyn rusqlite::ToSql> = grams
                .iter()
                .map(|g| g as &dyn rusqlite::ToSql)
                .collect();

            let rows = stmt
                .query_map(params_slice.as_slice(), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| StoreError::Metadata(e.to_string()))?;

            for r in rows {
                if let Ok((id, kind)) = r {
                    let key = format!("{kind}:{id}");
                    if seen_lexical.insert(key.clone()) {
                        lexical_ranked.push((id, kind));
                    }
                }
            }
        }

        // 3. Single-char query fallback if lexical empty
        if lexical_ranked.is_empty() && is_single_char_query(query) {
            let normalized = normalize_v1(query);
            let mut stmt = conn
                .prepare("SELECT object_id, object_kind, preview FROM projected_items LIMIT 500")
                .map_err(|e| StoreError::Metadata(e.to_string()))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| StoreError::Metadata(e.to_string()))?;

            for r in rows {
                if let Ok((id, kind, preview)) = r {
                    if preview.contains(&normalized) {
                        let key = format!("{kind}:{id}");
                        if seen_lexical.insert(key.clone()) {
                            lexical_ranked.push((id, kind));
                        }
                    }
                }
            }
        }

        // 4. Vector channel (if query_vector is present)
        let mut semantic_ranked = Vec::new();
        if let Some(qv) = query_vector {
            let vectors_dir = ProjectionIndex::vectors_dir(scope_dir);
            if vectors_dir.exists() {
                if let Ok(vec_hits) = VectorStore::scan_vectors(&vectors_dir, qv, 0.60) {
                    for (id, kind, _sim) in vec_hits {
                        semantic_ranked.push((id, kind));
                    }
                }
            }
        }

        // 5. Global RRF Fusion
        let mut rrf_table: HashMap<String, (Vec<u32>, Vec<&'static str>)> = HashMap::new();
        for (rank, (id, kind)) in lexical_ranked.iter().enumerate() {
            let key = format!("{kind}:{id}");
            let entry = rrf_table.entry(key).or_insert_with(|| (Vec::new(), Vec::new()));
            entry.0.push(rank as u32 + 1);
            if !entry.1.contains(&"lexical") {
                entry.1.push("lexical");
            }
        }
        for (rank, (id, kind)) in semantic_ranked.iter().enumerate() {
            let key = format!("{kind}:{id}");
            let entry = rrf_table.entry(key).or_insert_with(|| (Vec::new(), Vec::new()));
            entry.0.push(rank as u32 + 1);
            if !entry.1.contains(&"semantic") {
                entry.1.push("semantic");
            }
        }

        let mut scored: Vec<(String, f64, Vec<&'static str>)> = rrf_table
            .into_iter()
            .map(|(k, (ranks, channels))| {
                let score = rrf_score(&ranks);
                (k, score, channels)
            })
            .collect();

        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let a_is_claim = a.0.starts_with("claim:");
                    let b_is_claim = b.0.starts_with("claim:");
                    b_is_claim.cmp(&a_is_claim)
                })
        });

        // 6. Source verification & visibility gate
        let now_time = now();
        let mut hits = Vec::new();

        for (key, score, channels) in scored {
            if hits.len() >= limit {
                break;
            }
            let Some((kind, id)) = key.split_once(':') else {
                continue;
            };

            if kind == "claim" {
                if let Some(claim) = all_claims.iter().find(|c| c.front.id == id) {
                    if is_claim_visible(claim, suppressions, all_evidence, include_history, &now_time) {
                        hits.push(FileSearchHit {
                            id: claim.front.id.clone(),
                            kind: "claim".to_string(),
                            memory_kind: claim.front.kind.clone(),
                            content: claim.body.clone(),
                            score,
                            channels: channels.into_iter().map(|c| c.to_string()).collect(),
                            occurred_at: claim.front.occurred_at.clone(),
                            version: claim.front.version,
                        });
                    }
                }
            } else if kind == "entry" {
                if let Some(block) = all_evidence.iter().find(|b| b.header.id == id) {
                    if !suppressed(suppressions, &block.header.id)
                        && !suppressed(suppressions, &block.header.content_sha256)
                    {
                        hits.push(FileSearchHit {
                            id: block.header.id.clone(),
                            kind: "entry".to_string(),
                            memory_kind: "dialogue".to_string(),
                            content: block.payload.clone(),
                            score,
                            channels: channels.into_iter().map(|c| c.to_string()).collect(),
                            occurred_at: Some(block.header.occurred_at.clone()),
                            version: 1,
                        });
                    }
                }
            }
        }

        Ok((hits, false))
    }

    /// Degraded bounded full scan fallback directly on canonical source files.
    fn search_direct_scan(
        query: &str,
        limit: usize,
        include_history: bool,
        query_vector: Option<&[f32]>,
        scope_dir: &Path,
        all_evidence: &[EvidenceBlock],
        all_claims: &[ClaimDocument],
        suppressions: &[Suppression],
    ) -> Result<Vec<FileSearchHit>, StoreError> {
        let now_time = now();
        let norm_q = normalize_v1(query);
        let q_tokens = latin_tokens(query);
        let q_bigrams = cjk_bigrams(query);

        let mut candidate_scores: Vec<(FileSearchHit, f64)> = Vec::new();

        // 1. Scan claims
        for claim in all_claims {
            if !is_claim_visible(claim, suppressions, all_evidence, include_history, &now_time) {
                continue;
            }
            let text = &claim.body;
            let norm_text = normalize_v1(text);
            let mut match_score = 0.0_f64;
            let mut channels = Vec::new();

            // substring
            if norm_text.contains(&norm_q) {
                match_score += 1.0;
                channels.push("lexical".to_string());
            } else {
                // tokens
                let tokens_hit = q_tokens.iter().filter(|&t| norm_text.contains(t)).count();
                if tokens_hit > 0 {
                    match_score += tokens_hit as f64 / q_tokens.len().max(1) as f64;
                    channels.push("lexical".to_string());
                }
                // bigrams
                let bigrams_hit = q_bigrams.iter().filter(|&g| norm_text.contains(g)).count();
                if bigrams_hit > 0 {
                    match_score += bigrams_hit as f64 / q_bigrams.len().max(1) as f64;
                    if !channels.contains(&"lexical".to_string()) {
                        channels.push("lexical".to_string());
                    }
                }
            }

            // Optional vector similarity if available in shards
            if let Some(qv) = query_vector {
                let vdir = ProjectionIndex::vectors_dir(scope_dir);
                if let Ok(records) = VectorStore::load_records(&vdir) {
                    if let Some(rec) = records.iter().find(|r| r.object_id == claim.front.id) {
                        let sim = cosine_similarity(&rec.vector, qv);
                        if sim >= 0.60 {
                            match_score += sim as f64;
                            channels.push("semantic".to_string());
                        }
                    }
                }
            }

            if match_score > 0.0 {
                candidate_scores.push((
                    FileSearchHit {
                        id: claim.front.id.clone(),
                        kind: "claim".to_string(),
                        memory_kind: claim.front.kind.clone(),
                        content: claim.body.clone(),
                        score: match_score,
                        channels,
                        occurred_at: claim.front.occurred_at.clone(),
                        version: claim.front.version,
                    },
                    match_score,
                ));
            }
        }

        // 2. Scan conversation entries
        for block in all_evidence {
            if suppressed(suppressions, &block.header.id)
                || suppressed(suppressions, &block.header.content_sha256)
            {
                continue;
            }
            let text = &block.payload;
            let norm_text = normalize_v1(text);
            let mut match_score = 0.0_f64;
            let mut channels = Vec::new();

            if norm_text.contains(&norm_q) {
                match_score += 1.0;
                channels.push("lexical".to_string());
            } else {
                let tokens_hit = q_tokens.iter().filter(|&t| norm_text.contains(t)).count();
                if tokens_hit > 0 {
                    match_score += tokens_hit as f64 / q_tokens.len().max(1) as f64;
                    channels.push("lexical".to_string());
                }
                let bigrams_hit = q_bigrams.iter().filter(|&g| norm_text.contains(g)).count();
                if bigrams_hit > 0 {
                    match_score += bigrams_hit as f64 / q_bigrams.len().max(1) as f64;
                    if !channels.contains(&"lexical".to_string()) {
                        channels.push("lexical".to_string());
                    }
                }
            }

            if let Some(qv) = query_vector {
                let vdir = ProjectionIndex::vectors_dir(scope_dir);
                if let Ok(records) = VectorStore::load_records(&vdir) {
                    if let Some(rec) = records.iter().find(|r| r.object_id == block.header.id) {
                        let sim = cosine_similarity(&rec.vector, qv);
                        if sim >= 0.60 {
                            match_score += sim as f64;
                            channels.push("semantic".to_string());
                        }
                    }
                }
            }

            if match_score > 0.0 {
                candidate_scores.push((
                    FileSearchHit {
                        id: block.header.id.clone(),
                        kind: "entry".to_string(),
                        memory_kind: "dialogue".to_string(),
                        content: block.payload.clone(),
                        score: match_score,
                        channels,
                        occurred_at: Some(block.header.occurred_at.clone()),
                        version: 1,
                    },
                    match_score,
                ));
            }
        }

        candidate_scores.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let a_is_claim = a.0.kind == "claim";
                    let b_is_claim = b.0.kind == "claim";
                    b_is_claim.cmp(&a_is_claim)
                })
        });
        let hits = candidate_scores
            .into_iter()
            .take(limit)
            .map(|(hit, _)| hit)
            .collect();

        Ok(hits)
    }

    /// Build context bundle conforming to the 4-segment budget (compact → alignment → relationships → retrieved).
    pub fn context_bundle(
        scope_dir: &Path,
        _agent_id: &str,
        query: &str,
        overview: Option<&MemoryOverview>,
        alignment_doc: Option<&str>,
        relationships_data: Option<&[(String, String, String)]>, // (id, name, relation)
        compact_max_items: Option<usize>,
        all_evidence: &[EvidenceBlock],
        all_claims: &[ClaimDocument],
        suppressions: &[Suppression],
    ) -> Result<FileBundleResult, StoreError> {
        let mut used_total_chars = 0usize;

        // 1. Compact segment
        let mut compact_seg_items = Vec::new();
        let mut compact_seg_chars = 0usize;
        let mut compact_seg_omitted = 0usize;
        let mut compact_ids = HashSet::new();

        let max_compact = compact_max_items.unwrap_or(usize::MAX);
        if let Some(ov) = overview {
            for ref_item in &ov.curated {
                if compact_seg_items.len() >= max_compact {
                    compact_seg_omitted += 1;
                    continue;
                }
                if let Some(claim) = all_claims.iter().find(|c| c.front.id == ref_item.memory_id) {
                    let now_time = now();
                    if is_claim_visible(claim, suppressions, all_evidence, false, &now_time) {
                        let cost = claim.body.chars().count();
                        if compact_seg_chars + cost > BUNDLE_COMPACT_CHARS
                            || used_total_chars + compact_seg_chars + cost > BUNDLE_TOTAL_CHARS
                        {
                            compact_seg_omitted += 1;
                            continue;
                        }
                        compact_seg_chars += cost;
                        compact_ids.insert(claim.front.id.clone());
                        compact_seg_items.push(serde_json::json!({
                            "body": claim.body,
                            "observed_or_inferred": "observed",
                            "stable_refs": [format!("riko://memory/{}", claim.front.id)],
                        }));
                    }
                }
            }
        }
        used_total_chars += compact_seg_chars;

        let compact_segment = serde_json::json!({
            "segment": "compact",
            "domain_id": USER_MAIN_DOMAIN,
            "version": 1,
            "char_count": compact_seg_chars,
            "complete": compact_seg_omitted == 0,
            "omitted": compact_seg_omitted,
            "items": compact_seg_items,
        });

        // 2. Alignment segment
        let alignment_segment = if let Some(text) = alignment_doc {
            let body: String = text.chars().take(BUNDLE_ALIGNMENT_CHARS).collect();
            let alignment_chars = body.chars().count();
            used_total_chars += alignment_chars;
            serde_json::json!({
                "segment": "alignment",
                "domain_id": USER_MAIN_DOMAIN,
                "version": 1,
                "char_count": alignment_chars,
                "complete": body.len() == text.len(),
                "body": body,
            })
        } else {
            serde_json::json!({
                "segment": "alignment",
                "domain_id": USER_MAIN_DOMAIN,
                "version": 0,
                "char_count": 0,
                "complete": true,
            })
        };

        // 3. Relationships segment
        let mut rel_seg_items = Vec::new();
        let mut rel_seg_chars = 0usize;
        let mut rel_seg_omitted = 0usize;

        if let Some(entities) = relationships_data {
            for (id, name, relation) in entities {
                if rel_seg_items.len() >= BUNDLE_RELATIONSHIP_ITEMS {
                    rel_seg_omitted += 1;
                    continue;
                }
                let cost = name.chars().count() + relation.chars().count();
                if used_total_chars + rel_seg_chars + cost > BUNDLE_TOTAL_CHARS {
                    rel_seg_omitted += 1;
                    continue;
                }
                rel_seg_chars += cost;
                rel_seg_items.push(serde_json::json!({
                    "entity_id": id,
                    "display_name": name,
                    "relation": relation,
                }));
            }
        }
        used_total_chars += rel_seg_chars;

        let relationships_segment = serde_json::json!({
            "segment": "relationships",
            "domain_id": USER_MAIN_DOMAIN,
            "batch_version": 1,
            "total": rel_seg_items.len() + rel_seg_omitted,
            "char_count": rel_seg_chars,
            "complete": rel_seg_omitted == 0,
            "omitted": rel_seg_omitted,
            "items": rel_seg_items,
        });

        // 4. Retrieved segment (with search & deduplication against compact)
        let mut retrieved_text = String::new();
        let mut retrieved_items = Vec::new();
        let mut retrieved_omitted = Vec::new();
        let mut deduped_count = 0usize;
        let mut retrieved_truncated = false;

        let (hits, degraded) = Self::search(
            scope_dir,
            query,
            50,
            false,
            None,
            all_evidence,
            all_claims,
            suppressions,
        )?;

        let retrieved_budget = BUNDLE_TOTAL_CHARS.saturating_sub(used_total_chars);
        let mut retrieved_chars = 0usize;

        for hit in hits {
            // Deduplicate against compact
            if hit.kind == "claim" && compact_ids.contains(&hit.id) {
                deduped_count += 1;
                continue;
            }

            let entry = format!("- [{}: {}] {}", hit.kind, hit.id, hit.content);
            let cost = entry.chars().count();

            if retrieved_chars + cost > retrieved_budget {
                retrieved_omitted.push(serde_json::json!({
                    "id": hit.id,
                    "reason": "TOTAL_CHAR_LIMIT",
                }));
                retrieved_truncated = true;
                continue;
            }

            retrieved_chars += cost + if retrieved_items.is_empty() { 0 } else { 1 };
            if retrieved_text.is_empty() {
                retrieved_text = entry;
            } else {
                retrieved_text.push('\n');
                retrieved_text.push_str(&entry);
            }

            retrieved_items.push(serde_json::json!({
                "kind": hit.kind,
                "id": hit.id,
                "memory_kind": hit.memory_kind,
                "content": hit.content,
                "channels": hit.channels,
            }));
        }

        let retrieved_segment = serde_json::json!({
            "segment": "retrieved",
            "domain_id": USER_MAIN_DOMAIN,
            "char_count": retrieved_chars,
            "complete": !retrieved_truncated,
            "omitted": retrieved_omitted.len(),
            "deduped": deduped_count,
            "text": retrieved_text,
        });

        let segments = serde_json::json!([
            compact_segment,
            alignment_segment,
            relationships_segment,
            retrieved_segment,
        ]);

        let lexical_status = if degraded { "index_degraded" } else { "ok" };

        Ok(FileBundleResult {
            segments,
            budget: serde_json::json!({
                "total_chars": BUNDLE_TOTAL_CHARS,
                "used_chars": used_total_chars + retrieved_chars,
            }),
            resident: serde_json::json!({
                "compact_count": compact_seg_items.len(),
                "compact_chars": compact_seg_chars,
            }),
            retrieved: serde_json::json!({
                "text": retrieved_text,
                "items": retrieved_items,
                "omitted": retrieved_omitted,
                "truncated": retrieved_truncated,
                "deduped": deduped_count,
            }),
            lexical_status,
            semantic_status: "ok",
        })
    }
}
