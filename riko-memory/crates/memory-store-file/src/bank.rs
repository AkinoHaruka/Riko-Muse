//! V2-D1 derived bank facets for file storage (`experience`, `opinions`, `reflections`, `world`).
//!
//! Provides deterministic facet mapping (`facet_v1`), bank document serialization conforming
//! to Markdown conventions with source references, and real-time freshness validation.

use crate::{
    now, raw_content_hash, read_bytes, suppressed, ClaimDocument, StoreError, Suppression,
};
use memory_domain::USER_MAIN_DOMAIN;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const FACET_EXPERIENCE: &str = "experience";
pub const FACET_OPINIONS: &str = "opinions";
pub const FACET_REFLECTIONS: &str = "reflections";
pub const FACET_WORLD: &str = "world";

pub const FACETS: [&str; 4] = [
    FACET_EXPERIENCE,
    FACET_OPINIONS,
    FACET_REFLECTIONS,
    FACET_WORLD,
];

pub const BANK_DIR: &str = "memory/bank";

const PREFERENCE_MARKERS: [&str; 8] = [
    "喜欢",
    "偏好",
    "讨厌",
    "更愿意",
    "习惯",
    "prefer",
    "favorite",
    "dislike",
];

/// Deterministic facet assignment (`facet_v1` rule from doc7/06 §2).
pub fn facets_of(
    kind: &str,
    claim: &str,
    occurred_at: Option<&str>,
    status: &str,
    has_supersede_edge: bool,
) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    let push = |f: &'static str, out: &mut Vec<&'static str>| {
        if !out.contains(&f) {
            out.push(f);
        }
    };
    match kind {
        "episode" => push(FACET_EXPERIENCE, &mut out),
        "fact" => {
            push(FACET_WORLD, &mut out);
            if occurred_at.is_some() {
                push(FACET_EXPERIENCE, &mut out);
            }
        }
        "preference" => push(FACET_OPINIONS, &mut out),
        "instruction" => {
            push(FACET_WORLD, &mut out);
            let lowered = claim.to_lowercase();
            if PREFERENCE_MARKERS
                .iter()
                .any(|m| claim.contains(m) || lowered.contains(m))
            {
                push(FACET_OPINIONS, &mut out);
            }
        }
        _ => {}
    }
    // Reflections: only superseded items with an actual supersede edge.
    if status == "superseded" && has_supersede_edge {
        push(FACET_REFLECTIONS, &mut out);
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BankItem {
    pub memory_id: String,
    pub version: u64,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BankFacetView {
    pub facet: String,
    pub items: Vec<BankItem>,
    pub skipped_stale: usize,
}

pub struct BankStore;

impl BankStore {
    pub fn bank_dir(scope_dir: &Path) -> PathBuf {
        scope_dir.join(BANK_DIR)
    }

    pub fn facet_path(scope_dir: &Path, facet: &str) -> PathBuf {
        Self::bank_dir(scope_dir).join(format!("{facet}.md"))
    }

    pub fn is_valid_facet(facet: &str) -> bool {
        FACETS.contains(&facet)
    }

    /// Render a bank markdown document with standard front matter and source references.
    pub fn render_bank_facet(facet: &str, claims: &[&ClaimDocument]) -> String {
        let mut text = format!(
            "---\nriko_format: memory_bank_v1\ndomain_id: {}\nfacet: {}\ngenerated_at: {}\n---\n\n# Bank: {}\n",
            USER_MAIN_DOMAIN,
            facet,
            now(),
            facet,
        );
        for claim in claims {
            let one_line = claim.body.replace(['\r', '\n'], " ");
            text.push_str(&format!(
                "\n<!-- riko:ref memory={} version={} -->\n- {}\n",
                claim.front.id, claim.front.version, one_line
            ));
        }
        text
    }

    /// Parse a bank markdown document into individual items with memory refs.
    pub fn parse_bank_file(bytes: &[u8]) -> Result<Vec<(String, u64, String)>, StoreError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| StoreError::Metadata("non-utf8 bank file".into()))?;
        let mut items = Vec::new();
        let mut current_ref: Option<(String, u64)> = None;

        for line in text.lines() {
            if let Some(inner) = line
                .strip_prefix("<!-- riko:ref ")
                .and_then(|v| v.strip_suffix(" -->"))
            {
                let mut parts = inner.split(' ');
                let mem_id = parts
                    .next()
                    .and_then(|v| v.strip_prefix("memory="))
                    .unwrap_or_default();
                let ver: u64 = parts
                    .next()
                    .and_then(|v| v.strip_prefix("version="))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if !mem_id.is_empty() && ver > 0 {
                    current_ref = Some((mem_id.to_string(), ver));
                }
            } else if let Some(content) = line.strip_prefix("- ") {
                if let Some((mem_id, ver)) = current_ref.take() {
                    items.push((mem_id, ver, content.to_string()));
                }
            }
        }
        Ok(items)
    }

    /// Read bank facet view and instantly verify source freshness.
    /// If an item's source is missing, forgotten, suppressed, expired, or version drifted,
    /// it is skipped immediately (incrementing `skipped_stale`).
    pub fn read_view(
        scope_dir: &Path,
        facet: &str,
        all_claims: &[ClaimDocument],
        suppressions: &[Suppression],
        now_time: &str,
    ) -> Result<BankFacetView, StoreError> {
        if !Self::is_valid_facet(facet) {
            return Err(StoreError::Metadata(format!("unknown bank facet: {facet}")));
        }
        let path = Self::facet_path(scope_dir, facet);
        if !path.exists() {
            return Ok(BankFacetView {
                facet: facet.to_string(),
                items: Vec::new(),
                skipped_stale: 0,
            });
        }
        let bytes = read_bytes(&path)?;
        let parsed = Self::parse_bank_file(&bytes)?;

        let mut items = Vec::new();
        let mut skipped_stale = 0usize;

        for (mem_id, ver, _declared_body) in parsed {
            // Instant verification against live canonical claims
            let Some(claim) = all_claims.iter().find(|c| c.front.id == mem_id) else {
                skipped_stale += 1;
                continue;
            };

            // Version must match exactly
            if claim.front.version != ver {
                skipped_stale += 1;
                continue;
            }

            // Must not be forgotten or suppressed
            if claim.front.status == "forgotten"
                || suppressed(suppressions, &claim.front.id)
                || suppressed(suppressions, &raw_content_hash(&claim.body))
                || suppressed(suppressions, &claim.front.claim_sha256)
            {
                skipped_stale += 1;
                continue;
            }

            // Facet-specific freshness validation
            if facet == FACET_REFLECTIONS {
                if claim.front.status != "superseded" {
                    skipped_stale += 1;
                    continue;
                }
            } else {
                if claim.front.status != "active" {
                    skipped_stale += 1;
                    continue;
                }
                if let Some(ref until) = claim.front.valid_until {
                    if until.as_str() <= now_time {
                        skipped_stale += 1;
                        continue;
                    }
                }
            }

            items.push(BankItem {
                memory_id: mem_id,
                version: ver,
                body: claim.body.clone(),
            });
        }

        Ok(BankFacetView {
            facet: facet.to_string(),
            items,
            skipped_stale,
        })
    }

    /// Refresh bank facet files based on current claims.
    /// Returns true if any bank file was modified.
    pub fn refresh_all(
        scope_dir: &Path,
        all_claims: &[ClaimDocument],
        suppressions: &[Suppression],
        now_time: &str,
    ) -> Result<bool, StoreError> {
        let bank_dir = Self::bank_dir(scope_dir);
        fs::create_dir_all(&bank_dir)?;

        // Find which superseded claims have an active superseding edge
        let superseded_ids: std::collections::HashSet<&str> = all_claims
            .iter()
            .filter_map(|c| c.front.supersedes.as_deref())
            .collect();

        let mut any_modified = false;

        for facet in FACETS {
            let mut matching_claims: Vec<&ClaimDocument> = Vec::new();

            for claim in all_claims {
                if claim.front.status == "forgotten"
                    || suppressed(suppressions, &claim.front.id)
                    || suppressed(suppressions, &raw_content_hash(&claim.body))
                    || suppressed(suppressions, &claim.front.claim_sha256)
                {
                    continue;
                }

                if facet != FACET_REFLECTIONS {
                    if claim.front.status != "active" {
                        continue;
                    }
                    if let Some(ref until) = claim.front.valid_until {
                        if until.as_str() <= now_time {
                            continue;
                        }
                    }
                }

                let has_supersede_edge = superseded_ids.contains(claim.front.id.as_str());
                let assigned = facets_of(
                    &claim.front.kind,
                    &claim.body,
                    claim.front.occurred_at.as_deref(),
                    &claim.front.status,
                    has_supersede_edge,
                );

                if assigned.contains(&facet) {
                    matching_claims.push(claim);
                }
            }

            // Deterministic sort: updated_at desc, id asc
            matching_claims.sort_by(|a, b| {
                b.front
                    .updated_at
                    .cmp(&a.front.updated_at)
                    .then_with(|| a.front.id.cmp(&b.front.id))
            });

            let path = Self::facet_path(scope_dir, facet);

            let need_write = if path.exists() {
                match read_bytes(&path) {
                    Ok(existing) => {
                        if let Ok(parsed) = Self::parse_bank_file(&existing) {
                            if parsed.len() == matching_claims.len()
                                && parsed.iter().zip(matching_claims.iter()).all(|(p, c)| {
                                    p.0 == c.front.id && p.1 == c.front.version
                                })
                            {
                                false
                            } else {
                                true
                            }
                        } else {
                            true
                        }
                    }
                    Err(_) => true,
                }
            } else {
                true
            };

            if need_write {
                let rendered = Self::render_bank_facet(facet, &matching_claims);
                crate::atomic_replace(&path, rendered.as_bytes())?;
                any_modified = true;
            }
        }

        Ok(any_modified)
    }
}
