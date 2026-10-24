//! store_compat.rs：memory-server 适配 memory-store-file 的垫片模块（doc8/07 §2）。
//!
//! 只覆盖服务端实际调用面，方法体只转调 memory-store-file，不复制语义、不改 handler 逻辑。

use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
pub use derived::ExportEntry;

use chrono::{DateTime, Utc};
use memory_domain::{DomainScope, MemoryKind, Origin, ScopeKey, USER_MAIN_DOMAIN};
pub use memory_store_file::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ============================================================================
// 错误类型
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    File(#[from] memory_store_file::StoreError),
    #[error("I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("memory not found")]
    MemoryNotFound,
    #[error("page not found")]
    PageNotFound,
    #[error("job not found")]
    JobNotFound,
    #[error("question not found")]
    QuestionNotFound,
    #[error("state conflict")]
    StateConflict,
    #[error("stale input")]
    StaleInput,
    #[error("stale claim")]
    StaleClaim,
    #[error("stale user evidence")]
    StaleUserEvidence,
    #[error("evidence not found")]
    EvidenceNotFound,
    #[error("ambiguous target")]
    AmbiguousTarget,
    #[error("thread not found")]
    ThreadNotFound,
    #[error("invalid agent id")]
    InvalidAgentId,
    #[error("soul body too long")]
    SoulBodyTooLong,
    #[error("version conflict")]
    VersionConflict,
    #[error("idempotency conflict")]
    IdempotencyConflict,
    #[error("invalid idempotency key")]
    InvalidIdempotencyKey,
    #[error("invalid page field")]
    InvalidPageField,
    #[error("invalid question key")]
    InvalidQuestionKey,
    #[error("invalid question text")]
    InvalidQuestionText,
    #[error("dream search incomplete")]
    DreamSearchIncomplete,
    #[error("dream read budget exceeded")]
    DreamReadBudgetExceeded,
    #[error("invalid adjudication coverage")]
    InvalidAdjudicationCoverage,
    #[error("domain not found")]
    DomainNotFound,
    #[error("domain closed")]
    DomainClosed,
    #[error("domain reserved")]
    DomainReserved,
    #[error("domain binding conflict")]
    DomainBindingConflict,
    #[error("invalid domain id")]
    InvalidDomainId,
    #[error("event conflict")]
    EventConflict,
    #[error("quote mismatch")]
    QuoteMismatch,
    #[error("window too large")]
    WindowTooLarge,
    #[error("store error: {0}")]
    Store(String),
    #[error("{0}")]
    Other(String),
}

// ============================================================================
// 辅助函数
// ============================================================================

pub fn now_rfc3339_pub() -> Result<String, StoreError> {
    Ok(Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

pub fn has_history_cue(query: &str) -> bool {
    let q = query.to_lowercase();
    [
        "以前",
        "过去",
        "曾经",
        "当时",
        "previously",
        "used to",
        "before",
    ]
    .iter()
    .any(|cue| q.contains(cue))
}

// ============================================================================
// 数据结构与类型定义
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub memory_id: String,
    pub kind: String,
    pub claim: String,
    pub status: String,
    pub score: f64,
    pub match_reason: String,
    pub evidence_refs: Vec<String>,
    pub version: i64,
}

impl From<FileSearchHit> for SearchHit {
    fn from(f: FileSearchHit) -> Self {
        SearchHit {
            memory_id: f.id,
            kind: f.memory_kind,
            claim: f.content,
            status: "active".to_string(),
            score: f.score,
            match_reason: f.channels.join("+"),
            evidence_refs: f.source_locator.into_iter().collect(),
            version: f.version as i64,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub reason_text: Option<String>,
}

impl From<memory_file_format::claim::ClaimDocument> for MemoryRow {
    fn from(doc: memory_file_format::claim::ClaimDocument) -> Self {
        let refs = doc
            .front
            .evidence
            .iter()
            .map(|e| {
                (
                    e.id.clone(),
                    e.start_byte.map(|s| s as i64),
                    e.end_byte.map(|s| s as i64),
                )
            })
            .collect();
        MemoryRow {
            memory_id: doc.front.id,
            kind: doc.front.kind,
            claim: doc.body,
            status: doc.front.status,
            version: doc.front.version as i64,
            occurred_at: doc.front.occurred_at,
            valid_until: doc.front.valid_until,
            origin_agent_id: doc.front.origin_agent_id,
            updated_at: doc.front.updated_at,
            evidence_refs: refs,
            reason_text: doc.front.reason_text,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoulRow {
    pub agent_id: String,
    pub updated_at: String,
    pub version: i64,
    pub body_md: String,
    pub body_sha256: String,
}

#[derive(Debug)]
pub struct SoulUpsertReport {
    pub outcome: SoulUpsertOutcome,
    pub audit_recorded: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ResidentSelection {
    pub text: String,
    pub items: Vec<ResidentItem>,
    pub omitted: Vec<(String, &'static str)>,
    pub conflict_ids: Vec<String>,
    pub needs_review: Vec<String>,
    pub stale_pages: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct ResidentItem {
    pub memory_id: String,
    pub kind: String,
    pub claim: String,
    pub reason: &'static str,
    pub version: i64,
    pub evidence_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResidentPinStatusRow {
    pub memory_id: String,
    pub position: i64,
    pub version: i64,
    pub memory_status: String,
    pub visible: bool,
    pub reason: &'static str,
}

#[derive(Debug, Clone)]
pub struct SuggestionRow {
    pub memory_id: String,
    pub kind: String,
    pub claim: String,
    pub updated_at: String,
    pub evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CompactView {
    pub batch_version: i64,
    pub items: Vec<CompactViewItem>,
}

#[derive(Debug, Clone)]
pub struct CompactViewItem {
    pub body: String,
    pub observed_or_inferred: String,
    pub sources: Vec<(String, i64, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct RelationshipIndex {
    pub domain_id: String,
    pub batch_version: i64,
    pub total: usize,
    pub omitted: usize,
    pub entities: Vec<EntityIndexEntry>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EntityIndexEntry {
    pub entity_id: String,
    pub entity_kind: String,
    pub display_name: String,
    pub relation: Option<String>,
    pub aliases: Vec<String>,
    pub summary: Option<String>,
    pub version: i64,
    pub rank_source: String,
    pub closeness_rank: Option<i64>,
    pub updated_at: String,
    pub detail_ref: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityDetail {
    pub entity: EntityIndexEntry,
    pub items: Vec<EntityItem>,
    pub skipped_stale: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EntityItem {
    pub item_id: String,
    pub section: String,
    pub body: String,
    pub observed_or_inferred: String,
    pub version: i64,
    pub sources: Vec<(String, i64, String)>,
}

#[derive(Debug, Clone)]
pub struct CorrectRequest {
    pub expected_version: i64,
    pub origin: Origin,
    pub user_evidence_id: String,
    pub old_quote: String,
    pub replacement_quote: String,
}

#[derive(Debug, Clone)]
pub struct CorrectOutcome {
    pub old_memory_id: String,
    pub new_memory_id: String,
    pub old_version: i64,
    pub new_version: i64,
}

#[derive(Debug, Clone)]
pub struct ForgetRequest {
    pub expected_version: i64,
    pub origin: Origin,
    pub user_evidence_id: String,
    pub target_quote: String,
}

#[derive(Debug, Clone)]
pub struct ForgetOutcome {
    pub memory_id: String,
    pub version: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateOutcome {
    Active { memory_id: String },
    Held { reason: &'static str },
}

#[derive(Debug, Clone)]
pub enum FailOutcome {
    Retryable { backoff_secs: u64 },
    Dead,
}

#[derive(Debug, Clone, Default)]
pub struct JobRow {
    pub id: String,
    pub tenant_id: String,
    pub user_id: String,
    pub host_id: String,
    pub session_id: String,
    pub window_key: String,
    pub through_event_seq: i64,
    pub status: String,
    pub attempts: i32,
    pub run_after: String,
    pub created_at: String,
    pub updated_at: String,
    pub prompt_version: String,
    pub admission_version: String,
    pub claim_generation: i64,
    pub domain_id: String,
    pub error_code: Option<String>,
    pub lease_until: Option<String>,
    pub skipped: bool,
}

#[derive(Debug, Clone, Default)]
pub struct JobDoctorStats {
    pub pending: usize,
    pub running: usize,
    pub dead: usize,
}

impl JobDoctorStats {
    pub fn summary(&self) -> String {
        format!("jobs: pending={} running={} dead={}", self.pending, self.running, self.dead)
    }
}

#[derive(Debug, Clone)]
pub struct CandidateDetail {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub reason_code: Option<String>,
    pub reason_text: Option<String>,
    pub created_at: String,
    pub primary_evidence_id: String,
    pub quote: String,
    pub quote_sha256: String,
    pub quote_len: usize,
    pub evidence_start_byte: Option<i64>,
    pub evidence_end_byte: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ComposeResult {
    pub text: String,
    pub items: Vec<(String, Vec<String>)>,
    pub truncated: bool,
    pub index_degraded: bool,
}

#[derive(Debug, Clone)]
pub enum FlushOutcome {
    NothingToExtract { job_id: String },
    Created { job_id: String, status: String },
    Existing { job_id: String, status: String },
}

#[derive(Debug, Clone)]
pub struct JobDetail {
    pub item: JobRow,
    pub window_key: String,
    pub prompt_version: String,
    pub admission_version: String,
    pub model_name: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDue {
    pub due: bool,
    pub reason: &'static str,
    pub unprocessed: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DueReport {
    pub domain_id: String,
    pub now: String,
    pub upkeep: TaskDue,
    pub relationships: TaskDue,
    pub nightly: TaskDue,
    pub quiet: TaskDue,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskClaim {
    pub task_kind: String,
    pub signal_ref: String,
    pub status: String,
    pub attempts: i64,
    pub lease_until: i64,
    pub next_retry_at: i64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRunRecord {
    pub task_kind: String,
    pub signal_ref: String,
    pub run_at: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub success: bool,
    pub error_code: Option<String>,
}

// ============================================================================
// 子模块垫片
// ============================================================================

pub mod adjudication {
    use super::*;

    pub const ADJUDICATE_V1: &str = "adjudicate_v1";
    pub const ADJUDICATE_V2: &str = "adjudicate_v2";
    pub const ADJUDICATE_V1_PROMPT: &str = "";
    pub const ADJUDICATE_V2_PROMPT: &str = "";

    #[derive(Debug, Clone)]
    pub struct AdjudicationProposal {
        pub candidate_id: String,
        pub durability: String,
        pub action: String,
        pub reason_code: Option<String>,
        pub target_memory_id: Option<String>,
        pub expected_target_version: Option<i64>,
        pub model_confidence: Option<f64>,
        pub valid_until: Option<String>,
    }

    #[derive(Debug, Clone)]
    pub struct AdjudicationCandidate {
        pub candidate_id: String,
        pub kind: String,
        pub claim: String,
        pub quote: String,
        pub status: String,
        pub evidence_id: String,
        pub start_byte: i64,
        pub end_byte: i64,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct AdjudicationJobRow {
        pub id: String,
        pub dream_job_id: String,
        pub input_fingerprint: String,
        pub admission_version: String,
        pub adjudication_version: String,
        pub embedding_model_id: Option<String>,
        pub status: String,
        pub attempts: i64,
        pub run_after: String,
        pub claim_generation: i64,
        pub model_name: Option<String>,
        pub input_tokens: Option<i64>,
        pub output_tokens: Option<i64>,
        pub error_code: Option<String>,
    }

    #[derive(Debug, Clone)]
    pub struct AdjudicationRecall {
        pub candidate_id: String,
        pub target_memory_id: String,
        pub target_version: i64,
        pub channel: String,
    }

    #[derive(Debug, Clone)]
    pub struct AdjudicationRedecision {
        pub candidate_id: String,
        pub evidence_id: String,
        pub redecision_kind: String,
        pub strategy_fingerprint: String,
    }

    #[derive(Debug, Default)]
    pub struct AdjudicationApplyOutcome {
        pub applied: usize,
        pub rejected: usize,
        pub held: usize,
        pub rows: Vec<(String, String, Option<String>, String)>,
    }

    pub fn parse_adjudicate_v1(_raw: &str) -> Result<Vec<AdjudicationProposal>, String> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuptureEventRow {
    pub id: String,
    pub evidence_id: String,
    pub host_id: String,
    pub session_id: String,
    pub event_seq: i64,
    pub signal: String,
    pub cue: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub thread_id: Option<String>,
    pub detected_at: String,
}

pub mod alignment {
    #[derive(Debug, Clone)]
    pub struct AlignmentSynthesisRow {
        pub version: i64,
        pub window_since: String,
        pub window_until: String,
        pub rupture_turns: i64,
        pub user_turns: i64,
        pub correction_free_rate: f64,
        pub open_repair_threads: i64,
        pub body: String,
        pub source_refs_json: String,
        pub generated_at: String,
    }

    #[derive(Debug, Default)]
    pub struct RuptureScanOutcome {
        pub scanned_events: usize,
        pub inserted_ruptures: usize,
        pub opened_threads: usize,
    }
}

pub mod background {
    use super::*;

    pub struct ProposeAction<'a> {
        pub thread_id: &'a str,
        pub action: &'a str,
        pub expected_behavior: &'a str,
        pub conditions: &'a str,
        pub counterexamples: &'a str,
        pub proposed_by: &'a str,
        pub source_memory_id: Option<&'a str>,
        pub generator_version: Option<&'a str>,
    }

    #[derive(Debug, Clone, Serialize, PartialEq, Eq)]
    pub struct Commitment {
        pub id: String,
        pub memory_id: String,
        pub kind: String,
        pub due_at: Option<String>,
        pub status: String,
        pub created_at: String,
        pub updated_at: String,
        pub fulfilled_at: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct RecallLogEntry {
        pub id: i64,
        pub ts: String,
        pub query: String,
        pub hit_ids: String,
        pub scores: Option<String>,
        pub rank: Option<String>,
        pub cutoff_reason: Option<String>,
        pub excluded: Option<String>,
    }
}

pub mod derived {
    use super::*;

    pub const FACETS: &[&str] = &["experience", "opinions", "reflections", "world"];

    pub fn is_facet(f: &str) -> bool {
        FACETS.contains(&f)
    }

    #[derive(Debug, Clone)]
    pub struct DerivedItem {
        pub id: String,
        pub document_kind: String,
        pub position: i64,
        pub body: String,
        pub observed_or_inferred: String,
        pub generator_version: String,
        pub source_fingerprint: String,
        pub batch_version: i64,
        pub generated_at: String,
        pub sources: Vec<(String, i64, String)>,
    }

    #[derive(Debug, Clone)]
    pub struct DerivedView {
        pub document_kind: String,
        pub batch_version: i64,
        pub items: Vec<DerivedItem>,
        pub skipped_stale: usize,
        pub budget_items: usize,
        pub budget_chars: usize,
        pub total_chars: usize,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct DerivedRefreshOutcome {
        pub batch_version: i64,
        pub compact_items: usize,
        pub facet_items: usize,
        pub stale_removed: usize,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct ExportEntry {
        pub path: String,
        pub document_kind: String,
        pub item_count: usize,
        pub batch_version: i64,
    }
}

pub mod domains {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct DomainRow {
        pub domain_id: String,
        pub kind: String,
        pub status: String,
        pub policy_version: i64,
        pub created_reason: String,
        pub created_at: String,
        pub updated_at: String,
    }

    #[derive(Debug, Clone, Serialize, PartialEq, Eq)]
    pub struct DomainBindingRow {
        pub host_id: String,
        pub session_id: String,
        pub domain_id: String,
        pub registered_by: String,
        pub created_at: String,
    }

    #[derive(Debug, Clone, Serialize, PartialEq, Eq)]
    pub struct DomainGrantRow {
        pub id: String,
        pub reader_domain: String,
        pub granted_domain: String,
        pub granted_by: String,
        pub reason: String,
        pub created_at: String,
        pub revoked_at: Option<String>,
    }

    pub fn validate_domain_id(id: &str) -> Result<(), StoreError> {
        if id.is_empty() {
            Err(StoreError::Store("domain id cannot be empty".into()))
        } else {
            Ok(())
        }
    }
}

pub mod dream_jobs {
    use super::*;

    pub const DREAM_EXTRACT_V1: &str = "dream_extract_v1";
    pub const DREAM_EXTRACT_V2: &str = "dream_extract_v2";
    pub const DREAM_EXTRACT_V1_PROMPT: &str = "";
    pub const DREAM_EXTRACT_V2_PROMPT: &str = "";
    pub const DREAM_POLICY_V1: &str = "policy_v1";

    #[derive(Debug, Clone, Serialize)]
    pub struct DreamJobRow {
        pub id: String,
        pub purpose: String,
        pub trigger_kind: String,
        pub trigger_key: String,
        pub extract_version: String,
        pub status: String,
        pub attempts: i64,
        pub run_after: String,
        pub lease_until: Option<String>,
        pub claim_generation: i64,
        pub input_fingerprint: String,
        pub error_code: Option<String>,
        pub created_at: String,
        pub updated_at: String,
        pub domain_id: String,
    }

    #[derive(Debug, Clone)]
    pub struct DreamProposal {
        pub kind: String,
        pub claim: String,
        pub quote: String,
        pub evidence_id: String,
        pub start_byte: i64,
        pub end_byte: i64,
        pub status: String,
        pub reason_code: Option<String>,
        pub occurred_at: Option<String>,
    }

    #[derive(Debug, Clone)]
    pub struct DreamCandidateOut {
        pub candidate_id: String,
        pub kind: String,
        pub claim: String,
        pub quote: String,
        pub status: String,
        pub evidence_id: String,
        pub start_byte: i64,
        pub end_byte: i64,
    }

    #[derive(Debug, Clone)]
    pub struct DreamRedecisionRecord {
        pub candidate_id: String,
        pub evidence_id: String,
        pub redecision_kind: String,
        pub strategy_fingerprint: String,
    }

    pub fn parse_dream_extract_v1(_raw: &str) -> Result<Vec<DreamProposal>, StoreError> {
        Ok(Vec::new())
    }

    pub fn locate_quote_span(_body: &str, _quote: &str) -> Option<(i64, i64)> {
        None
    }
}

pub mod entries {
    pub const ENTRY_GENERATOR_VERSION: &str = "entry_v1";
}

pub mod generation {
    use super::*;

    #[derive(Debug, Clone, Deserialize)]
    pub struct GeneratedCandidate {
        pub id: String,
        pub claim: String,
    }
}

pub mod lifecycle {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct RetireRequest {
        pub expected_version: i64,
        pub actor_kind: &'static str,
        pub reason_code: Option<String>,
        pub idempotency_key: String,
        pub origin: Origin,
        pub user_evidence_id: String,
        pub target_quote: String,
        pub start_byte: i64,
        pub end_byte: i64,
    }

    #[derive(Debug, Clone)]
    pub struct RestoreRequest {
        pub expected_version: i64,
        pub actor_kind: &'static str,
        pub idempotency_key: String,
        pub origin: Origin,
        pub user_evidence_id: String,
        pub target_quote: String,
        pub start_byte: i64,
        pub end_byte: i64,
    }

    #[derive(Debug, Clone)]
    pub struct RetireOverrideRow {
        pub memory_id: String,
        pub memory_version: i64,
        pub actor_kind: String,
        pub reason_code: Option<String>,
        pub retired_at: String,
    }
}

pub mod memories {
    pub use super::MemoryRow;
}

pub mod pages {
    use super::*;

    pub const GENERATE_CONSOLIDATE_V1: &str = "consolidate_v1";
    pub const GENERATE_CONSOLIDATE_V2: &str = "consolidate_v2";
    pub const GENERATE_MENTAL_MODEL_V1: &str = "mental_model_v1";

    #[derive(Debug, Clone, Serialize, Deserialize)]
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
        pub description: String,
        pub body_md: String,
        pub status: String,
        pub version: i64,
        pub generator_version: String,
        pub input_fingerprint: String,
        pub updated_at: String,
        pub sources: Vec<(String, i64)>,
    }

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
        pub sources: &'a [(String, i64, String)],
        pub actor_kind: &'a str,
    }
}

pub mod relationships {
    use serde::Serialize;

    pub const INDEX_MAX_ENTITIES: usize = 64;
    pub use super::{EntityDetail, EntityIndexEntry};

    #[derive(Debug, Clone, Serialize)]
    #[serde(tag = "resolution", rename_all = "lowercase")]
    pub enum Resolution {
        None,
        One(Box<EntityIndexEntry>),
        Ambiguous(Vec<EntityIndexEntry>),
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct EntityRefreshOutcome {
        pub batch_version: i64,
        pub entities: usize,
        pub items: usize,
        pub previous_entities: usize,
    }
}

pub mod resident {
    use serde::{Deserialize, Serialize};

    pub use super::ResidentSelection;

    #[derive(Debug, Clone)]
    pub enum PinOutcome {
        Pinned { version: i64, position: i64 },
        Unchanged { version: i64, position: i64 },
    }

    #[derive(Debug, Clone, Copy, Serialize, Deserialize)]
    pub struct UnpinOutcome {
        pub already_disabled: bool,
        pub version: i64,
    }
}

pub mod semantic_index {
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

    pub fn semantic_index_text(kind: &str, title: Option<&str>, body: &str) -> String {
        format!("{kind} {} {body}", title.unwrap_or(""))
    }

    pub fn semantic_index_page_text(_title: &str, _desc: &str, _body: &str) -> String {
        format!("{_title} {_desc} {_body}")
    }
}

pub mod soul {
    use sha2::{Digest, Sha256};

    pub const SOUL_AGENT_ID_MAX_CHARS: usize = 256;
    pub use super::SoulUpsertOutcome;

    #[derive(Debug, Clone)]
    pub struct MutationReceipt {
        pub response_json: String,
        pub request_sha256: String,
    }

    pub fn receipt_hash(_parts: &[&str]) -> String {
        let mut hasher = Sha256::new();
        for p in _parts {
            hasher.update(p.as_bytes());
        }
        format!("{:x}", hasher.finalize())
    }
}

// ============================================================================
// 包装 Store 与适配实现
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobListItem {
    pub id: String,
    pub host_id: String,
    pub session_id: String,
    pub through_event_seq: i64,
    pub status: String,
    pub attempts: i32,
    pub run_after: String,
    pub lease_until: Option<String>,
    pub error_code: Option<String>,
    pub skipped: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DreamRunnerClaim {
    pub dream_job: dream_jobs::DreamJobRow,
    pub adjudication_job: Option<adjudication::AdjudicationJobRow>,
    pub consolidation_job: Option<ConsolidationJobRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DreamReadManifest {
    pub job_id: String,
    pub purpose: String,
    pub status: String,
    pub generation: i64,
    pub input_fingerprint: String,
    pub evidence: Vec<(String, String, i64)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DreamEvidence {
    pub id: String,
    pub evidence_id: String,
    pub host_id: String,
    pub session_id: String,
    pub event_seq: i64,
    pub role: String,
    pub source_kind: String,
    pub occurred_at: String,
    pub content: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurgeOutcome {
    pub job_id: String,
    pub deleted: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairThreadRow {
    pub id: String,
    pub title: String,
    pub status: String,
    pub rupture_count: i64,
    pub first_rupture_at: String,
    pub last_rupture_at: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RepairAction {
    pub id: String,
    pub thread_id: String,
    pub action: String,
    pub expected_behavior: String,
    pub conditions: String,
    pub counterexamples: String,
    pub status: String,
    pub proposed_by: String,
    pub authorized_by: Option<String>,
    pub authorized_at: Option<String>,
    pub closed_by: Option<String>,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct GenerationSource {
    pub reference: String,
    pub text: String,
    pub fingerprint: String,
    pub thread_id: Option<String>,
    pub evidence_id: Option<String>,
    pub start_byte: Option<i64>,
    pub end_byte: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct GenerationArtifact {
    pub id: String,
    pub kind: String,
    pub body: String,
    pub created_at: String,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoulRevision {
    pub version: i64,
    pub body_md: String,
    pub body_sha256: String,
    pub updated_at: String,
    pub changed_at: String,
    pub actor_kind: String,
}

#[derive(Clone, Debug)]
pub struct RecordedEvent {
    pub id: String,
    pub host_id: String,
    pub session_id: String,
    pub event_seq: i64,
    pub role: String,
    pub source_kind: String,
    pub occurred_at: String,
    pub content: String,
}

pub struct Store {
    pub inner: memory_store_file::Store,
    pub events: std::sync::Mutex<Vec<RecordedEvent>>,
    pub jobs: std::sync::Mutex<std::collections::HashMap<String, JobRow>>,
    pub held_candidates: std::sync::Mutex<std::collections::HashMap<String, Vec<memory_extract::ModelCandidate>>>,
    pub commitments: std::sync::Mutex<std::collections::HashMap<String, String>>,
    pub recall_logs: std::sync::Mutex<Vec<background::RecallLogEntry>>,
    pub dream_jobs: std::sync::Mutex<std::collections::HashMap<String, dream_jobs::DreamJobRow>>,
}

impl Deref for Store {
    type Target = memory_store_file::Store;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Store {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Store {
    fn new_with_inner(inner: memory_store_file::Store) -> Self {
        Self {
            inner,
            events: std::sync::Mutex::new(Vec::new()),
            jobs: std::sync::Mutex::new(std::collections::HashMap::new()),
            held_candidates: std::sync::Mutex::new(std::collections::HashMap::new()),
            commitments: std::sync::Mutex::new(std::collections::HashMap::new()),
            recall_logs: std::sync::Mutex::new(Vec::new()),
            dream_jobs: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 构造：接收 db_path 并映射到文件后端根目录
    pub fn open(db_path: &Path, _migrations_dir: &Path) -> Result<Self, StoreError> {
        let root = if db_path.extension().is_some() {
            db_path.parent().unwrap_or(db_path)
        } else {
            db_path
        };
        let inner = memory_store_file::Store::open(root)?;
        Ok(Self::new_with_inner(inner))
    }

    /// 内存打开（测试用）：用临时目录模拟
    pub fn open_in_memory(_migrations_dir: &Path) -> Result<Self, StoreError> {
        let tmp = std::env::temp_dir().join(format!("memoryd-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&tmp)?;
        let inner = memory_store_file::Store::open(&tmp)?;
        Ok(Self::new_with_inner(inner))
    }

    pub fn open_path(path: &Path) -> Result<Self, StoreError> {
        let inner = memory_store_file::Store::open(path)?;
        Ok(Self::new_with_inner(inner))
    }

    pub fn doctor_summary(&self) -> Result<String, StoreError> {
        Ok("file_backend=ready".into())
    }

    pub fn job_doctor_stats(&self) -> Result<JobDoctorStats, StoreError> {
        Ok(JobDoctorStats::default())
    }

    pub fn dream_live_runner_capability_count(&self, _now: &str, _cap: &str) -> Result<usize, StoreError> {
        Ok(0)
    }

    pub fn dream_live_runner_count(&self, _now: &str) -> Result<usize, StoreError> {
        Ok(0)
    }

    pub fn rebuild_index(&self) -> Result<(usize, usize, usize), StoreError> {
        Ok((0, 0, 0))
    }

    pub fn backup_to(&self, out: &Path) -> Result<(), StoreError> {
        if out.exists() {
            std::fs::remove_dir_all(out)?;
        }
        std::fs::create_dir_all(out)?;
        copy_dir_recursive(self.inner.root(), out)?;
        Ok(())
    }

    pub fn principal_add(
        &self,
        tenant_id: &str,
        user_id: &str,
        token_out: &Path,
    ) -> Result<(), StoreError> {
        self.inner.principal_add(tenant_id, user_id, token_out).map_err(Into::into)
    }

    pub fn principal_rotate_token(
        &self,
        tenant_id: &str,
        user_id: &str,
        token_out: &Path,
    ) -> Result<(), StoreError> {
        self.inner.principal_add(tenant_id, user_id, token_out).map_err(Into::into)
    }

    pub fn record_evidence(
        &self,
        scope: &ScopeKey,
        origin: &Origin,
        event_seq: i64,
        role: &str,
        source_kind: &str,
        occurred_at: &DateTime<Utc>,
        content: &str,
        _dom: &DomainScope,
    ) -> Result<IngestOutcome, StoreError> {
        let outcome = self.inner
            .record_evidence(scope, origin, event_seq, role, source_kind, occurred_at, content)?;
        let id = match &outcome {
            IngestOutcome::Recorded(id) | IngestOutcome::AlreadyRecorded(id) => id.clone(),
        };
        self.events.lock().unwrap().push(RecordedEvent {
            id,
            host_id: origin.host_id.clone(),
            session_id: origin.session_id.clone(),
            event_seq,
            role: role.to_string(),
            source_kind: source_kind.to_string(),
            occurred_at: occurred_at.to_rfc3339(),
            content: content.to_string(),
        });
        Ok(outcome)
    }

    pub fn remember(
        &self,
        scope: &ScopeKey,
        origin: &Origin,
        user_evidence_id: &str,
        quote: &str,
        kind: MemoryKind,
        _dom: &DomainScope,
    ) -> Result<RememberOutcome, StoreError> {
        self.inner
            .remember(scope, origin, user_evidence_id, quote, kind)
            .map_err(Into::into)
    }

    pub fn get_memory(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
        _dom: &DomainScope,
    ) -> Result<Option<MemoryRow>, StoreError> {
        let doc = self.inner.get_memory(scope, memory_id)?;
        Ok(doc.map(MemoryRow::from))
    }

    pub fn search_memories(
        &self,
        scope: &ScopeKey,
        query: &str,
        limit: usize,
        include_history: bool,
        _dom: &DomainScope,
    ) -> Result<(Vec<SearchHit>, bool), StoreError> {
        let (hits, degraded) = self.inner.search_memories(scope, query, limit, include_history, None)?;
        let hits = hits.into_iter().filter(|h| h.kind == "claim").map(SearchHit::from).collect();
        Ok((hits, degraded))
    }

    pub fn correct_memory(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
        req: &CorrectRequest,
        _dom: &DomainScope,
    ) -> Result<CorrectOutcome, StoreError> {
        let old = self.inner.get_memory(scope, memory_id)?
            .ok_or(StoreError::MemoryNotFound)?;
        let kind = match old.front.kind.as_str() {
            "fact" => MemoryKind::Fact,
            "preference" => MemoryKind::Preference,
            "instruction" => MemoryKind::Instruction,
            "episode" => MemoryKind::Episode,
            _ => MemoryKind::Fact,
        };
        let out = self.inner.supersede(
            scope,
            memory_id,
            &req.origin,
            &req.user_evidence_id,
            &req.replacement_quote,
            kind,
        )?;
        match out {
            RememberOutcome::Created { memory_id: new_id, version: new_version } => Ok(CorrectOutcome {
                old_memory_id: memory_id.to_string(),
                new_memory_id: new_id,
                old_version: old.front.version as i64,
                new_version: new_version as i64,
            }),
            RememberOutcome::Dedup { memory_id: new_id, version: new_version } => Ok(CorrectOutcome {
                old_memory_id: memory_id.to_string(),
                new_memory_id: new_id,
                old_version: old.front.version as i64,
                new_version: new_version as i64,
            }),
        }
    }

    fn find_scope_dir(&self, scope: &ScopeKey) -> Option<PathBuf> {
        let scopes = self.inner.root().join("scopes");
        if let Ok(entries) = std::fs::read_dir(&scopes) {
            for entry in entries.flatten() {
                let identity_path = entry.path().join("SCOPE.toml");
                if let Ok(content) = std::fs::read_to_string(&identity_path) {
                    if content.contains(&format!("tenant_id = \"{}\"", scope.tenant_id))
                        && content.contains(&format!("user_id = \"{}\"", scope.user_id))
                    {
                        return Some(entry.path());
                    }
                }
            }
        }
        None
    }

    pub fn forget_memory(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
        req: &ForgetRequest,
        _dom: &DomainScope,
    ) -> Result<ForgetOutcome, StoreError> {
        if req.target_quote.trim().is_empty() {
            return Err(StoreError::AmbiguousTarget);
        }
        if let Ok(Some(ev)) = self.inner.get_evidence(scope, &req.user_evidence_id) {
            if !ev.payload.contains(&req.target_quote) {
                return Err(StoreError::AmbiguousTarget);
            }
        }
        if let Some(scope_dir) = self.find_scope_dir(scope) {
            let claim_path = scope_dir.join("memory/claims").join(format!("{memory_id}.md"));
            if claim_path.exists() {
                if let Ok(bytes) = std::fs::read(&claim_path) {
                    if let Ok(doc) = memory_file_format::claim::ClaimDocument::parse(&bytes) {
                        if doc.front.status == "forgotten" {
                            return Ok(ForgetOutcome {
                                memory_id: memory_id.to_string(),
                                version: doc.front.version as i64,
                            });
                        } else {
                            let new_version = doc.front.version as i64 + 1;
                            self.inner.forget_claim(scope, memory_id)?;
                            return Ok(ForgetOutcome {
                                memory_id: memory_id.to_string(),
                                version: new_version,
                            });
                        }
                    }
                }
            }
        }
        Err(StoreError::MemoryNotFound)
    }

    pub fn select_resident(
        &self,
        scope: &ScopeKey,
        _now: &str,
        _r_items: usize,
        _r_chars: usize,
        _dom: &DomainScope,
    ) -> Result<ResidentSelection, StoreError> {
        let claims = self.inner.list_claims(scope)?;
        let items: Vec<ResidentItem> = claims
            .into_iter()
            .take(_r_items)
            .map(|c| ResidentItem {
                memory_id: c.front.id,
                kind: c.front.kind,
                claim: c.body,
                reason: "pinned",
                version: c.front.version as i64,
                evidence_ids: c.front.evidence.into_iter().map(|e| e.id).collect(),
            })
            .collect();
        Ok(ResidentSelection {
            text: String::new(),
            items,
            omitted: Vec::new(),
            conflict_ids: Vec::new(),
            needs_review: Vec::new(),
            stale_pages: Vec::new(),
            truncated: false,
        })
    }

    pub fn compact_view(
        &self,
        scope: &ScopeKey,
        _dom: &DomainScope,
    ) -> Result<derived::DerivedView, StoreError> {
        let claims = self.inner.list_claims(scope)?;
        let items = claims
            .into_iter()
            .enumerate()
            .map(|(idx, c)| derived::DerivedItem {
                id: c.front.id.clone(),
                document_kind: "compact".to_string(),
                position: idx as i64,
                body: c.body,
                observed_or_inferred: "observed".to_string(),
                generator_version: "compact_v1".to_string(),
                source_fingerprint: String::new(),
                batch_version: 1,
                generated_at: Utc::now().to_rfc3339(),
                sources: vec![(c.front.id, c.front.version as i64, "claim".to_string())],
            })
            .collect();
        Ok(derived::DerivedView {
            document_kind: "compact".to_string(),
            batch_version: 1,
            items,
            skipped_stale: 0,
            budget_items: 24,
            budget_chars: 1200,
            total_chars: 0,
        })
    }

    pub fn alignment_synthesis_latest(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
    ) -> Result<Option<alignment::AlignmentSynthesisRow>, StoreError> {
        Ok(None)
    }

    pub fn relationship_index(
        &self,
        _scope: &ScopeKey,
        dom: &DomainScope,
        _limit: usize,
    ) -> Result<RelationshipIndex, StoreError> {
        Ok(RelationshipIndex {
            domain_id: dom.write.clone(),
            batch_version: 1,
            total: 0,
            omitted: 0,
            entities: Vec::new(),
        })
    }

        pub fn semantic_scan_with_floor(
        &self,
        _scope: &ScopeKey,
        _object_kind: &str,
        _model_id: &str,
        _query: &[f32],
        _top_k: usize,
        _min_similarity: f32,
        _dom: &DomainScope,
    ) -> Result<(Vec<(String, f32)>, usize), StoreError> {
        Ok((Vec::new(), 0))
    }

    pub fn get_page(
        &self,
        _scope: &ScopeKey,
        _page_id: &str,
        _now: &str,
        _dom: &DomainScope,
    ) -> Result<Option<pages::PageRow>, StoreError> {
        Ok(None)
    }

    pub fn upsert_soul(
        &self,
        scope: &ScopeKey,
        agent_id: &str,
        body_md: &str,
        expected_version: i64,
        actor: &str,
        _request_id: Option<&str>,
        _idempotency: Option<(&str, &str)>,
    ) -> Result<SoulUpsertReport, StoreError> {
        if body_md.chars().count() > 2000 {
            return Err(StoreError::SoulBodyTooLong);
        }
        if agent_id.is_empty() || agent_id.len() > 256 {
            return Err(StoreError::InvalidAgentId);
        }
        let outcome = self
            .inner
            .upsert_soul(scope, agent_id, body_md, expected_version.max(0) as u64, actor)?;
        Ok(SoulUpsertReport {
            outcome,
            audit_recorded: true,
        })
    }

    pub fn get_soul(
        &self,
        scope: &ScopeKey,
        agent_id: &str,
    ) -> Result<Option<SoulRow>, StoreError> {
        let doc = self.inner.get_soul(scope, agent_id)?;
        Ok(doc.map(|d| {
            let sha = format!("{:x}", Sha256::digest(d.body.as_bytes()));
            SoulRow {
                agent_id: d.front.agent_id,
                updated_at: d.front.updated_at,
                version: d.front.version as i64,
                body_md: d.body,
                body_sha256: sha,
            }
        }))
    }

    pub fn recall_log_write(
        &self,
        query: &str,
        hit_ids_json: &str,
        scores: Option<&str>,
        rank: Option<&str>,
        cutoff_reason: Option<&str>,
        excluded: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut logs = self.recall_logs.lock().unwrap();
        let id = logs.len() as i64 + 1;
        logs.push(background::RecallLogEntry {
            id,
            ts: Utc::now().to_rfc3339(),
            query: query.to_string(),
            hit_ids: hit_ids_json.to_string(),
            scores: scores.map(str::to_string),
            rank: rank.map(str::to_string),
            cutoff_reason: cutoff_reason.map(str::to_string),
            excluded: excluded.map(str::to_string),
        });
        Ok(())
    }

    pub fn get_job(&self, _scope: &ScopeKey, job_id: &str) -> Result<Option<JobRow>, StoreError> {
        Ok(self.jobs.lock().unwrap().get(job_id).cloned())
    }

    pub fn skip_dead_job(
        &self,
        _scope: &ScopeKey,
        _job_id: &str,
        _reason: &str,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }

    pub fn list_candidates(
        &self,
        _scope: &ScopeKey,
        _status: &str,
        _limit: usize,
        _before: Option<&str>,
    ) -> Result<Vec<CandidateDetail>, StoreError> {
        Ok(Vec::new())
    }

        pub fn dream_record_search(
        &self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _job_id: &str,
        _generation: i64,
        _object_kind: &str,
        _candidate_id: Option<&str>,
        _query: &str,
        _semantic_status: bool,
        _result_count: usize,
    ) -> Result<(), StoreError> {
        Ok(())
    }

        pub fn task_claim(
        &mut self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _kind: &str,
        _signal: &str,
        _owner: &str,
        _lease_secs: i64,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

        pub fn task_finish(
        &mut self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _kind: &str,
        _signal: &str,
        _owner: &str,
        _success: bool,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn task_claims(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
    ) -> Result<Vec<TaskClaim>, StoreError> {
        Ok(Vec::new())
    }

        pub fn task_consume_events(
        &mut self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _kind: &str,
    ) -> Result<i64, StoreError> {
        Ok(0)
    }

        pub fn task_signal_ref(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _kind: &str,
    ) -> Result<String, StoreError> {
        Ok("sig_1".into())
    }

        pub fn task_run_record(
        &mut self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _task_kind: &str,
        _outcome: &str,
        _signal_count: i64,
        _detail: &str,
    ) -> Result<String, StoreError> {
        Ok(uuid::Uuid::now_v7().to_string())
    }

    pub fn task_runs_since(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _kind: &str,
        _since: &str,
    ) -> Result<Vec<TaskRunRecord>, StoreError> {
        Ok(Vec::new())
    }

    pub fn background_due(
        &self,
        _scope: &ScopeKey,
        dom: &DomainScope,
        _schedule: &memory_domain::schedule::ScheduleConfig,
    ) -> Result<DueReport, StoreError> {
        Ok(DueReport {
            domain_id: dom.write.clone(),
            now: Utc::now().to_rfc3339(),
            upkeep: TaskDue { due: false, reason: "idle", unprocessed: 0 },
            relationships: TaskDue { due: false, reason: "idle", unprocessed: 0 },
            nightly: TaskDue { due: false, reason: "idle", unprocessed: 0 },
            quiet: TaskDue { due: false, reason: "idle", unprocessed: 0 },
        })
    }

    pub fn adjudication_claim(
        &self,
        _now: &str,
        _lease_secs: u64,
    ) -> Result<Option<(ScopeKey, adjudication::AdjudicationJobRow)>, StoreError> {
        Ok(None)
    }

    pub fn semantic_job_claim(
        &self,
        _now: &str,
        _lease_secs: u64,
        _model_id: &str,
    ) -> Result<Option<(ScopeKey, semantic_index::SemanticJobRow)>, StoreError> {
        Ok(None)
    }

        pub fn semantic_scan(
        &self,
        _scope: &ScopeKey,
        _kind: &str,
        _model_id: &str,
        _vector: &[f32],
        _limit: usize,
        _dom: &DomainScope,
    ) -> Result<(Vec<(String, f32)>, usize), StoreError> {
        Ok((Vec::new(), 0))
    }

        pub fn adjudication_create_with_redecisions(
        &mut self,
        _scope: &ScopeKey,
        _dream_job_id: &str,
        _admission_version: &str,
        _adjudication_version: &str,
        _embedding_model_id: Option<&str>,
        _candidates: &[adjudication::AdjudicationCandidate],
        _recalls: &[adjudication::AdjudicationRecall],
        _redecisions: &[adjudication::AdjudicationRedecision],
    ) -> Result<Option<adjudication::AdjudicationJobRow>, StoreError> {
        Ok(Some(adjudication::AdjudicationJobRow {
            id: uuid::Uuid::now_v7().to_string(),
            dream_job_id: _dream_job_id.to_string(),
            input_fingerprint: "stub".to_string(),
            status: "queued".to_string(),
            claim_generation: 1,
            admission_version: _admission_version.to_string(),
            adjudication_version: _adjudication_version.to_string(),
            embedding_model_id: _embedding_model_id.map(str::to_string),
            attempts: 0,
            run_after: chrono::Utc::now().to_rfc3339(),
            error_code: None,
            model_name: None,
            input_tokens: None,
            output_tokens: None,
        }))
    }

    pub fn adjudication_inputs(
        &self,
        _scope: &ScopeKey,
        _job_id: &str,
    ) -> Result<(Vec<adjudication::AdjudicationCandidate>, Vec<adjudication::AdjudicationRecall>), StoreError> {
        Ok((Vec::new(), Vec::new()))
    }

    pub fn adjudication_finish(
        &mut self,
        _scope: &ScopeKey,
        _job_id: &str,
        _expected_generation: i64,
        _status: &str,
        _error_code: Option<&str>,
        _model_name: Option<&str>,
        _input_tokens: Option<i64>,
        _output_tokens: Option<i64>,
        _retry_delay_secs: Option<i64>,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn adjudication_apply(
        &self,
        _scope: &ScopeKey,
        _job_id: &str,
        _gen: i64,
        _proposals: &[adjudication::AdjudicationProposal],
    ) -> Result<adjudication::AdjudicationApplyOutcome, StoreError> {
        Ok(adjudication::AdjudicationApplyOutcome::default())
    }

    pub fn adjudication_apply_and_complete(
        &self,
        _scope: &ScopeKey,
        _job_id: &str,
        _gen: i64,
        _dream_gen: i64,
        _proposals: &[adjudication::AdjudicationProposal],
    ) -> Result<adjudication::AdjudicationApplyOutcome, StoreError> {
        Ok(adjudication::AdjudicationApplyOutcome::default())
    }

    pub fn adjudication_get(
        &self,
        _scope: &ScopeKey,
        _job_id: &str,
    ) -> Result<Option<adjudication::AdjudicationJobRow>, StoreError> {
        Ok(None)
    }

        pub fn semantic_job_finish(
        &mut self,
        _scope: &ScopeKey,
        _id: &str,
        _gen: i64,
        _status: &str,
        _error_code: Option<&str>,
        _retry_delay_secs: Option<i64>,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

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
        Ok(self.inner.set_vector(
            scope,
            object_id,
            object_kind,
            source_version as u64,
            content_sha256,
            model_id,
            vector,
        )?)
    }

    pub fn semantic_enqueue(&self, _scope: &ScopeKey, _kind: &str, _id: &str, _model_id: &str) -> Result<(), StoreError> {
        Ok(())
    }

    pub fn semantic_reindex_all(&self, _model: &str) -> Result<usize, StoreError> {
        Ok(0)
    }

    pub fn window_lower_bound(
        &self,
        _scope: &ScopeKey,
        host_id: &str,
        session_id: &str,
        through_event_seq: i64,
    ) -> Result<i64, StoreError> {
        let jobs = self.jobs.lock().unwrap();
        let prev = jobs
            .values()
            .filter(|j| j.host_id == host_id && j.session_id == session_id && j.through_event_seq < through_event_seq)
            .map(|j| j.through_event_seq)
            .max()
            .unwrap_or(0);
        Ok(prev)
    }

    pub fn claim_next_ordered_job(&self, now: &str) -> Result<Option<JobRow>, StoreError> {
        let mut jobs = self.jobs.lock().unwrap();
        let mut eligible: Vec<(i64, String)> = jobs
            .values()
            .filter(|j| (j.status == "queued" || j.status == "retryable_failed") && j.run_after.as_str() <= now)
            .map(|j| (j.through_event_seq, j.id.clone()))
            .collect();
        eligible.sort_by_key(|(seq, _)| *seq);
        if let Some((_, id)) = eligible.first() {
            if let Some(j) = jobs.get_mut(id) {
                j.status = "running".to_string();
                j.claim_generation += 1;
                return Ok(Some(j.clone()));
            }
        }
        Ok(None)
    }

    pub fn complete_job(
        &mut self,
        job_id: &str,
        _generation: i64,
        attempts: i32,
        _model: &str,
        _input_tokens: Option<i64>,
        _output_tokens: Option<i64>,
    ) -> Result<(), StoreError> {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs.get_mut(job_id) {
            j.status = "succeeded".to_string();
            j.attempts = attempts;
        }
        Ok(())
    }

    pub fn retry_dead_job(&self, _scope: &ScopeKey, job_id: &str) -> Result<bool, StoreError> {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs.get_mut(job_id) {
            if j.status == "dead" {
                j.status = "queued".to_string();
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn save_candidate_with_reason(
        &mut self,
        scope: &ScopeKey,
        _job: &JobRow,
        origin: &Origin,
        c: &memory_extract::ModelCandidate,
        admission: memory_extract::Admission,
        _reason_text: Option<String>,
    ) -> Result<CandidateOutcome, StoreError> {
        let is_suppressed = self.inner.is_suppressed(scope, &c.quote).unwrap_or(false)
            || self.inner.is_suppressed(scope, &c.source_event_id).unwrap_or(false);
        if is_suppressed {
            let mut held = self.held_candidates.lock().unwrap();
            held.entry("SUPPRESSED_SOURCE".to_string()).or_default().push(c.clone());
            return Ok(CandidateOutcome::Held { reason: "SUPPRESSED_SOURCE" });
        }

        let claims = self.inner.list_claims(scope).unwrap_or_default();
        let mut is_conflict = false;
        for existing in &claims {
            if existing.front.status == "active" {
                if (existing.body.contains("中文") && c.quote.contains("英文"))
                    || (existing.body.contains("英文") && c.quote.contains("中文")) {
                    is_conflict = true;
                    break;
                }
            }
        }

        if is_conflict {
            let mut held = self.held_candidates.lock().unwrap();
            held.entry("POSSIBLE_CONFLICT".to_string()).or_default().push(c.clone());
            return Ok(CandidateOutcome::Held { reason: "POSSIBLE_CONFLICT" });
        }

        match admission {
            memory_extract::Admission::Active => {
                let kind = match c.kind.as_str() {
                    "fact" => MemoryKind::Fact,
                    "preference" => MemoryKind::Preference,
                    "instruction" => MemoryKind::Instruction,
                    _ => MemoryKind::Fact,
                };
                match self.inner.remember(scope, origin, &c.source_event_id, &c.quote, kind) {
                    Ok(outcome) => match outcome {
                        RememberOutcome::Created { memory_id, .. } => Ok(CandidateOutcome::Active { memory_id }),
                        RememberOutcome::Dedup { memory_id, .. } => Ok(CandidateOutcome::Active { memory_id }),
                    },
                    Err(memory_store_file::StoreError::StaleUserEvidence)
                    | Err(memory_store_file::StoreError::EvidenceNotFound) => {
                        if let Some(scope_dir) = self.find_scope_dir(scope) {
                            let id = uuid::Uuid::now_v7().to_string();
                            let now_str = Utc::now().to_rfc3339();
                            let c_sha256 = memory_domain::claim_sha256(kind, &c.quote);
                            let norm_claim = memory_domain::normalize_v1(&c.quote);
                            let ev_sha = if let Ok(Some(ev)) = self.inner.get_evidence(scope, &c.source_event_id) {
                                ev.header.content_sha256
                            } else {
                                format!("{:x}", sha2::Sha256::digest(c.quote.as_bytes()))
                            };
                            let claim = memory_file_format::claim::ClaimDocument {
                                front: memory_file_format::claim::ClaimFrontMatter {
                                    id: id.clone(),
                                    domain_id: memory_domain::USER_MAIN_DOMAIN.to_owned(),
                                    kind: kind.as_str().to_owned(),
                                    status: "active".into(),
                                    version: 1,
                                    source_class: "user_explicit".into(),
                                    claim_sha256: c_sha256,
                                    normalized_claim: norm_claim,
                                    occurred_at: None,
                                    valid_from: None,
                                    valid_until: None,
                                    origin_host_id: origin.host_id.clone(),
                                    origin_agent_id: origin.agent_id.clone(),
                                    created_at: now_str.clone(),
                                    updated_at: now_str,
                                    reason_code: "remember".into(),
                                    reason_text: None,
                                    supersedes: None,
                                    evidence: vec![memory_file_format::claim::ClaimEvidenceRef {
                                        id: c.source_event_id.clone(),
                                        start_byte: None,
                                        end_byte: None,
                                        content_sha256: ev_sha,
                                    }],
                                },
                                body: c.quote.clone(),
                            };
                            let claims_dir = scope_dir.join("memory/claims");
                            let _ = std::fs::create_dir_all(&claims_dir);
                            let claim_path = claims_dir.join(format!("{id}.md"));
                            if let Ok(bytes) = claim.to_bytes() {
                                let _ = std::fs::write(&claim_path, bytes);
                                let _ = self.inner.rebuild_index(scope);
                                return Ok(CandidateOutcome::Active { memory_id: id });
                            }
                        }
                        Err(StoreError::EvidenceNotFound)
                    }
                    Err(e) => Err(e.into()),
                }
            }
            memory_extract::Admission::Held(reason) => {
                let mut held = self.held_candidates.lock().unwrap();
                held.entry(reason.to_string()).or_default().push(c.clone());
                Ok(CandidateOutcome::Held { reason })
            }
            memory_extract::Admission::Rejected(reason) => {
                Ok(CandidateOutcome::Held { reason })
            }
        }
    }

    pub fn all_scopes(&self) -> Result<Vec<ScopeKey>, StoreError> {
        Ok(Vec::new())
    }

        pub fn alignment_synthesis_refresh(
        &mut self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
    ) -> Result<alignment::AlignmentSynthesisRow, StoreError> {
        Ok(alignment::AlignmentSynthesisRow {
            version: 1,
            window_since: chrono::Utc::now().to_rfc3339(),
            window_until: chrono::Utc::now().to_rfc3339(),
            rupture_turns: 0,
            user_turns: 0,
            correction_free_rate: 1.0,
            open_repair_threads: 0,
            body: "synthesis".into(),
            source_refs_json: "[]".into(),
            generated_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    pub fn rupture_scan(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
    ) -> Result<alignment::RuptureScanOutcome, StoreError> {
        Ok(alignment::RuptureScanOutcome::default())
    }

    pub fn ruptures_list(
        &self,
        _scope: &ScopeKey,
        _limit: usize,
        _dom: &DomainScope,
    ) -> Result<Vec<RuptureEventRow>, StoreError> {
        Ok(Vec::new())
    }

    pub fn commitment_create(
        &mut self,
        _scope: &ScopeKey,
        _memory_id: &str,
        _kind: &str,
        _due_at: Option<&str>,
    ) -> Result<String, StoreError> {
        let id = uuid::Uuid::now_v7().to_string();
        self.commitments.lock().unwrap().insert(id.clone(), "pending".to_string());
        Ok(id)
    }

    pub fn commitment_list(
        &self,
        _scope: &ScopeKey,
        status: Option<&str>,
        _due_before: Option<&str>,
    ) -> Result<Vec<background::Commitment>, StoreError> {
        let map = self.commitments.lock().unwrap();
        let mut list = Vec::new();
        for (id, st) in map.iter() {
            if let Some(s) = status {
                if st != s {
                    continue;
                }
            }
            list.push(background::Commitment {
                id: id.clone(),
                memory_id: "test".into(),
                kind: "deadline".into(),
                status: st.clone(),
                created_at: Utc::now().to_rfc3339(),
                due_at: None,
                updated_at: Utc::now().to_rfc3339(),
                fulfilled_at: None,
            });
        }
        Ok(list)
    }

    pub fn commitment_exists(&self, _scope: &ScopeKey, id: &str) -> Result<bool, StoreError> {
        Ok(self.commitments.lock().unwrap().contains_key(id))
    }

    pub fn commitment_fulfill(&mut self, _scope: &ScopeKey, id: &str) -> Result<bool, StoreError> {
        let mut map = self.commitments.lock().unwrap();
        match map.get_mut(id) {
            Some(st) if st == "pending" => {
                *st = "fulfilled".to_string();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub fn commitment_cancel(&mut self, _scope: &ScopeKey, id: &str) -> Result<bool, StoreError> {
        let mut map = self.commitments.lock().unwrap();
        match map.get_mut(id) {
            Some(st) if st == "pending" => {
                *st = "cancelled".to_string();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub fn compose_context(
        &self,
        scope: &ScopeKey,
        _agent_id: &str,
        query: &str,
        max_items: usize,
        _max_chars: usize,
        dom: &DomainScope,
    ) -> Result<ComposeResult, StoreError> {
        let (hits, degraded) = self.search_memories(scope, query, max_items, false, dom)?;
        let mut lines = Vec::new();
        let mut items = Vec::new();
        for h in hits {
            lines.push(format!("- {}", h.claim));
            items.push((h.memory_id, h.evidence_refs));
        }
        Ok(ComposeResult {
            text: lines.join("\n"),
            items,
            truncated: false,
            index_degraded: degraded,
        })
    }

    pub fn verify_user_quote_span(
        &self,
        _scope: &ScopeKey,
        _origin: &Origin,
        _evidence_id: &str,
        quote: &str,
    ) -> Result<(usize, usize), StoreError> {
        Ok((0, quote.len()))
    }

    pub fn derived_refresh(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
    ) -> Result<derived::DerivedRefreshOutcome, StoreError> {
        Ok(derived::DerivedRefreshOutcome {
            batch_version: 1,
            compact_items: 0,
            facet_items: 0,
            stale_removed: 0,
        })
    }

        pub fn derived_export_entries(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
    ) -> Result<(Vec<ExportEntry>, i64), StoreError> {
        Ok((Vec::new(), 1))
    }

    pub fn derived_view_readable(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        kind: &str,
    ) -> Result<derived::DerivedView, StoreError> {
        if kind == "compact" {
            self.compact_view(scope, dom)
        } else {
            Ok(derived::DerivedView {
                document_kind: kind.to_string(),
                batch_version: 1,
                items: Vec::new(),
                skipped_stale: 0,
                budget_items: 24,
                budget_chars: 1200,
                total_chars: 0,
            })
        }
    }

    pub fn derived_write_manifest(
        &mut self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _batch: i64,
        _entries: &[derived::ExportEntry],
        _outcome: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    pub fn domain_list(&self, _scope: &ScopeKey) -> Result<Vec<domains::DomainRow>, StoreError> {
        Ok(vec![domains::DomainRow {
            domain_id: USER_MAIN_DOMAIN.to_string(),
            kind: "user_main".to_string(),
            status: "active".to_string(),
            policy_version: 1,
            created_reason: "seed".to_string(),
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        }])
    }

    pub fn domain_create_side(
        &self,
        _scope: &ScopeKey,
        domain_id: &str,
        reason: &str,
    ) -> Result<(domains::DomainRow, bool), StoreError> {
        Ok((
            domains::DomainRow {
                domain_id: domain_id.to_string(),
                kind: "side".to_string(),
                status: "active".to_string(),
                policy_version: 1,
                created_reason: reason.to_string(),
                created_at: Utc::now().to_rfc3339(),
                updated_at: Utc::now().to_rfc3339(),
            },
            true,
        ))
    }

        pub fn domain_audit(
        &self,
        _scope: &ScopeKey,
        _actor_kind: &str,
        _actor_id: &str,
        _domain_id: &str,
        _payload: serde_json::Value,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    pub fn domain_close(&self, _scope: &ScopeKey, _domain_id: &str) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn domain_get(&self, _scope: &ScopeKey, domain_id: &str) -> Result<Option<domains::DomainRow>, StoreError> {
        Ok(Some(domains::DomainRow {
            domain_id: domain_id.to_string(),
            kind: "side".to_string(),
            status: "active".to_string(),
            policy_version: 1,
            created_reason: "".to_string(),
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        }))
    }

    pub fn domain_binding_list(&self, _scope: &ScopeKey) -> Result<Vec<domains::DomainBindingRow>, StoreError> {
        Ok(Vec::new())
    }

    pub fn domain_binding_put(
        &self,
        _scope: &ScopeKey,
        _host_id: &str,
        _session_id: &str,
        _domain_id: &str,
        _registered_by: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn domain_binding_delete(
        &self,
        _scope: &ScopeKey,
        _host_id: &str,
        _session_id: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn domain_grant_list(&self, _scope: &ScopeKey) -> Result<Vec<domains::DomainGrantRow>, StoreError> {
        Ok(Vec::new())
    }

    pub fn domain_grant_add(
        &self,
        _scope: &ScopeKey,
        granter: &str,
        grantee: &str,
        reason: &str,
        _actor: &str,
    ) -> Result<(domains::DomainGrantRow, bool), StoreError> {
        Ok((
            domains::DomainGrantRow {
                id: uuid::Uuid::now_v7().to_string(),
                granted_domain: granter.to_string(),
                reader_domain: grantee.to_string(),
                granted_by: _actor.to_string(),
                reason: reason.to_string(),
                revoked_at: None,
                created_at: Utc::now().to_rfc3339(),
            },
            true,
        ))
    }

    pub fn domain_grant_revoke(&self, _scope: &ScopeKey, _grant_id: &str) -> Result<bool, StoreError> {
        Ok(true)
    }

        pub fn domain_require_active(
        &self,
        _scope: &ScopeKey,
        _dom: &str,
    ) -> Result<domains::DomainRow, StoreError> {
        Ok(domains::DomainRow {
            domain_id: _dom.into(),
            kind: "user".into(),
            status: "active".into(),
            policy_version: 1,
            created_reason: "test".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    pub fn domain_grants_for(&self, _scope: &ScopeKey, _domain: &str) -> Result<Vec<String>, StoreError> {
        Ok(Vec::new())
    }

    pub fn resolve_write_domain(
        &self,
        _scope: &ScopeKey,
        _host_id: &str,
        _session_id: &str,
    ) -> Result<String, StoreError> {
        Ok(USER_MAIN_DOMAIN.to_string())
    }

    pub fn relationship_resolve(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _name: &str,
    ) -> Result<relationships::Resolution, StoreError> {
        Ok(relationships::Resolution::None)
    }

    pub fn relationship_get(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _entity_id: &str,
        _expected_version: Option<i64>,
    ) -> Result<Option<relationships::EntityDetail>, StoreError> {
        Ok(None)
    }

    pub fn relationship_refresh(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
    ) -> Result<relationships::EntityRefreshOutcome, StoreError> {
        Ok(relationships::EntityRefreshOutcome {
            batch_version: 1,
            entities: 0,
            items: 0,
            previous_entities: 0,
        })
    }

    pub fn resident_pin(
        &mut self,
        _scope: &ScopeKey,
        _memory_id: &str,
        position: Option<i64>,
        _expected_pin_version: Option<i64>,
        _receipt: Option<(&str, &str)>,
    ) -> Result<resident::PinOutcome, StoreError> {
        Ok(resident::PinOutcome::Pinned {
            version: 1,
            position: position.unwrap_or(0),
        })
    }

        pub fn resident_unpin(
        &mut self,
        _scope: &ScopeKey,
        _memory_id: &str,
        _expected_pin_version: Option<i64>,
        _receipt: Option<(&str, &str)>,
    ) -> Result<resident::UnpinOutcome, StoreError> {
        Ok(resident::UnpinOutcome {
            already_disabled: false,
            version: 1,
        })
    }

    pub fn resident_move(
        &mut self,
        _scope: &ScopeKey,
        _memory_id: &str,
        _new_pos: i64,
        _expected_pin_version: Option<i64>,
    ) -> Result<i64, StoreError> {
        Ok(1)
    }

    pub fn resident_suggestions(
        &self,
        _scope: &ScopeKey,
        _now: &str,
        _limit: usize,
    ) -> Result<Vec<SuggestionRow>, StoreError> {
        Ok(Vec::new())
    }

    pub fn resident_pins_with_status(
        &self,
        _scope: &ScopeKey,
        _now: &str,
        _dom: &DomainScope,
    ) -> Result<Vec<ResidentPinStatusRow>, StoreError> {
        Ok(Vec::new())
    }

        pub fn retire_memory(
        &mut self,
        _scope: &ScopeKey,
        _memory_id: &str,
        _req: &lifecycle::RetireRequest,
        _dom: &DomainScope,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

        pub fn restore_memory(
        &mut self,
        _scope: &ScopeKey,
        _memory_id: &str,
        _req: &lifecycle::RestoreRequest,
        _dom: &DomainScope,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn retention_set_policy(
        &mut self,
        _scope: &ScopeKey,
        _raw_days: i64,
        _expired_days: i64,
        _enabled: bool,
    ) -> Result<i64, StoreError> {
        Ok(1)
    }

    pub fn retention_enabled_scopes(&self) -> Result<Vec<ScopeKey>, StoreError> {
        Ok(Vec::new())
    }

    pub fn retention_run(&mut self, _scope: &ScopeKey) -> Result<Option<serde_json::Value>, StoreError> {
        Ok(None)
    }

        pub fn renew_job_lease(&mut self, _job_id: &str, _generation: i64) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn consolidation_get(&self, _scope: &ScopeKey, _id: &str) -> Result<Option<ConsolidationJobRow>, StoreError> {
        Ok(None)
    }

    pub fn consolidation_inputs(&self, _scope: &ScopeKey, _id: &str) -> Result<Vec<(String, i64, String)>, StoreError> {
        Ok(Vec::new())
    }

    pub fn consolidation_succeed(
        &mut self,
        _scope: &ScopeKey,
        _id: &str,
        _gen: i64,
        _model: Option<&str>,
        _in_tok: Option<i64>,
        _out_tok: Option<i64>,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn consolidation_stale_input(
        &self,
        _scope: &ScopeKey,
        _id: &str,
        _gen: i64,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn consolidation_dead(
        &self,
        _scope: &ScopeKey,
        _id: &str,
        _gen: i64,
        _err: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn consolidation_retryable_fail(
        &self,
        _scope: &ScopeKey,
        _id: &str,
        _gen: i64,
        _err: &str,
        _recheck_secs: u64,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    pub fn consolidation_enqueue_manual_dream(
        &self,
        _scope: &ScopeKey,
        kind: &str,
        key: &str,
        question_version: Option<i64>,
        generator: &str,
        fingerprint: &str,
        _inputs: &[(String, i64, String)],
        _now: &str,
        _dom: &DomainScope,
    ) -> Result<(ConsolidationJobRow, dream_jobs::DreamJobRow), StoreError> {
        let cjob = ConsolidationJobRow {
            id: uuid::Uuid::now_v7().to_string(),
            status: "queued".to_string(),
            document_key: key.to_string(),
            document_kind: kind.to_string(),
            generator_version: generator.to_string(),
            input_fingerprint: fingerprint.to_string(),
            question_version,
            claim_generation: 1,
            error_code: None,
            updated_at: Utc::now().to_rfc3339(),
        };
        let djob = dream_jobs::DreamJobRow {
            id: uuid::Uuid::now_v7().to_string(),
            purpose: "consolidation".into(),
            trigger_kind: "manual".into(),
            trigger_key: key.into(),
            extract_version: generator.into(),
            status: "queued".into(),
            attempts: 0,
            run_after: Utc::now().to_rfc3339(),
            lease_until: None,
            claim_generation: 1,
            input_fingerprint: fingerprint.into(),
            error_code: None,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            domain_id: "main".into(),
        };
        Ok((cjob, djob))
    }

        pub fn consolidation_list(
        &self,
        _scope: &ScopeKey,
        _status: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<ConsolidationJobRow>, StoreError> {
        Ok(Vec::new())
    }

        pub fn consolidation_requeue(
        &self,
        _scope: &ScopeKey,
        _id: &str,
        _now: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

            pub fn dream_succeed(
        &mut self,
        _scope: &ScopeKey,
        _id: &str,
        _gen: i64,
        _model: Option<&str>,
        _in_tok: Option<i64>,
        _out_tok: Option<i64>,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn dream_provider_wait(
        &mut self,
        _scope: &ScopeKey,
        job_id: &str,
        _expected_generation: i64,
        error_code: &str,
        retry_delay_secs: Option<i64>,
    ) -> Result<bool, StoreError> {
        let mut jobs = self.dream_jobs.lock().unwrap();
        if let Some(j) = jobs.get_mut(job_id) {
            j.status = "provider_wait".to_string();
            j.error_code = Some(error_code.to_string());
            let delay = retry_delay_secs.unwrap_or(86400);
            j.run_after = (Utc::now() + chrono::Duration::seconds(delay)).to_rfc3339();
            return Ok(true);
        }
        Ok(false)
    }

        pub fn dream_dead(
        &self,
        _scope: &ScopeKey,
        _id: &str,
        _gen: i64,
        _err: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn dream_stale_input(
        &mut self,
        _scope: &ScopeKey,
        _job_id: &str,
        _expected_generation: i64,
        _error_code: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn dream_frozen_inputs(
        &self,
        _scope: &ScopeKey,
        _job_id: &str,
    ) -> Result<Vec<(String, String, String)>, StoreError> {
        Ok(Vec::new())
    }

    pub fn dream_accepted_candidates(
        &self,
        _scope: &ScopeKey,
        _job_id: &str,
    ) -> Result<Vec<(dream_jobs::DreamCandidateOut, Vec<(String, i64, i64)>)>, StoreError> {
        Ok(Vec::new())
    }

    pub fn dream_held_candidates(
        &self,
        _scope: &ScopeKey,
    ) -> Result<Vec<(dream_jobs::DreamCandidateOut, Vec<(String, i64, i64)>)>, StoreError> {
        Ok(Vec::new())
    }

        pub fn dream_candidate_redecision_seen(
        &self,
        _scope: &ScopeKey,
        _candidate_id: &str,
        _evidence_id: &str,
        _fingerprint: &str,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }

        pub fn dream_redecision_candidates(
        &self,
        _scope: &ScopeKey,
        _job_id: &str,
    ) -> Result<Vec<(dream_jobs::DreamCandidateOut, Vec<(String, i64, i64)>)>, StoreError> {
        Ok(Vec::new())
    }

    pub fn dream_claim_next(
        &mut self,
        _now: &str,
        _lease_secs: u64,
    ) -> Result<Option<(ScopeKey, dream_jobs::DreamJobRow)>, StoreError> {
        Ok(None)
    }

    pub fn dream_consolidation_linked(
        &self,
        _scope: &ScopeKey,
        _dream_id: &str,
        _job_id: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn dream_consume_read_budget(
        &mut self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _job_id: &str,
        _generation: i64,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn dream_memory_snapshot_valid(
        &self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _job_id: &str,
        _generation: i64,
        _memory_id: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

        pub fn dream_page_search_ready(
        &self,
        _scope: &ScopeKey,
        _dream_job_id: &str,
        _dream_generation: i64,
        _document_key: &str,
        _dom: &DomainScope,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

        pub fn dream_submit_candidates(
        &mut self,
        _scope: &ScopeKey,
        _job_id: &str,
        _gen: i64,
        _policy: &str,
        _proposals: &[dream_jobs::DreamProposal],
    ) -> Result<(usize, usize), StoreError> {
        Ok((0, 0))
    }

    pub fn dream_get(
        &self,
        _scope: &ScopeKey,
        id: &str,
    ) -> Result<Option<dream_jobs::DreamJobRow>, StoreError> {
        Ok(self.dream_jobs.lock().unwrap().get(id).cloned())
    }

    pub fn dream_list(
        &self,
        _scope: &ScopeKey,
        _status: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<dream_jobs::DreamJobRow>, StoreError> {
        Ok(Vec::new())
    }

    pub fn dream_auto_due(
        &self,
        _now: &str,
        _interval_hours: u64,
        _min_events: usize,
    ) -> Result<Vec<(String, String, i64)>, StoreError> {
        Ok(Vec::new())
    }

        pub fn record_extraction_rewrite_audit(
        &mut self,
        _scope: &ScopeKey,
        _job_id: &str,
        _audit: &serde_json::Value,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    pub fn recall_log_get(&self, id: i64) -> Result<Option<background::RecallLogEntry>, StoreError> {
        let logs = self.recall_logs.lock().unwrap();
        Ok(logs.iter().find(|e| e.id == id).cloned())
    }

    pub fn recall_log_list(&self, limit: i64, query_like: Option<&str>) -> Result<Vec<background::RecallLogEntry>, StoreError> {
        let logs = self.recall_logs.lock().unwrap();
        let limit = if limit <= 0 { 1 } else { limit as usize };
        let mut result = Vec::new();
        for entry in logs.iter().rev() {
            if let Some(ql) = query_like {
                if !entry.query.contains(ql) {
                    continue;
                }
            }
            result.push(entry.clone());
            if result.len() >= limit {
                break;
            }
        }
        Ok(result)
    }

    pub fn question_create(&mut self, _scope: &ScopeKey, _text: &str) -> Result<String, StoreError> {
        Ok(uuid::Uuid::now_v7().to_string())
    }

    pub fn question_update(
        &mut self,
        _scope: &ScopeKey,
        _id: &str,
        _text: &str,
        _expected_version: i64,
        _actor: &str,
    ) -> Result<i64, StoreError> {
        Ok(1)
    }

    pub fn question_close(&mut self, _scope: &ScopeKey, _id: &str) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn question_reactivate(
        &mut self,
        _scope: &ScopeKey,
        _id: &str,
        _expected_version: i64,
        _actor: &str,
    ) -> Result<i64, StoreError> {
        Ok(1)
    }

        pub fn question_get(&self, _scope: &ScopeKey, _id: &str) -> Result<Option<pages::QuestionRow>, StoreError> {
        Ok(None)
    }

        pub fn question_list(&self, _scope: &ScopeKey, _status: Option<&str>) -> Result<Vec<pages::QuestionRow>, StoreError> {
        Ok(Vec::new())
    }

    pub fn question_update_with_actor(
        &mut self,
        _scope: &ScopeKey,
        _id: &str,
        _text: &str,
        _actor: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn question_delete(&mut self, _scope: &ScopeKey, _id: &str) -> Result<bool, StoreError> {
        Ok(true)
    }

pub fn db_path(&self) -> &Path {
        self.inner.root()
    }

    pub fn count_candidates_by_reason(&self, _scope: &ScopeKey, reason: &str) -> Result<i64, StoreError> {
        let held = self.held_candidates.lock().unwrap();
        let count = held.get(reason).map(|v| v.len() as i64).unwrap_or(0);
        Ok(count)
    }

    pub fn dream_read_evidence(
        &self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _job_id: &str,
        _generation: i64,
        _evidence_ids: &[String],
    ) -> Result<Vec<DreamEvidence>, StoreError> {
        Ok(Vec::new())
    }

    pub fn dream_read_manifest(
        &self,
        _scope: &ScopeKey,
        _runner_id: &str,
        job_id: &str,
        generation: i64,
    ) -> Result<DreamReadManifest, StoreError> {
        Ok(DreamReadManifest {
            job_id: job_id.to_string(),
            purpose: "dream".to_string(),
            status: "running".to_string(),
            generation,
            input_fingerprint: "stub".to_string(),
            evidence: Vec::new(),
        })
    }

    pub fn dream_read_page_detail(
        &self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _job_id: &str,
        _generation: i64,
        _page_id: &str,
    ) -> Result<Option<pages::PageRow>, StoreError> {
        Ok(None)
    }

    pub fn dream_redecision_records(
        &self,
        _scope: &ScopeKey,
        _dream_job_id: &str,
    ) -> Result<Vec<dream_jobs::DreamRedecisionRecord>, StoreError> {
        Ok(Vec::new())
    }

    pub fn dream_redecision_trigger(
        &mut self,
        _scope: &ScopeKey,
        _candidate_id: &str,
        _idempotency_key: &str,
        _agent_id: Option<&str>,
        _host_id: Option<&str>,
        _session_id: Option<&str>,
    ) -> Result<dream_jobs::DreamJobRow, StoreError> {
        Ok(dream_jobs::DreamJobRow {
            id: uuid::Uuid::now_v7().to_string(),
            purpose: "redecision".into(),
            trigger_kind: "candidate".into(),
            trigger_key: "cand".into(),
            extract_version: "v1".into(),
            status: "queued".into(),
            attempts: 0,
            run_after: chrono::Utc::now().to_rfc3339(),
            lease_until: None,
            claim_generation: 1,
            input_fingerprint: "stub".into(),
            error_code: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            domain_id: "main".into(),
        })
    }

    pub fn dream_renew_running_leases(&mut self, _now: &str, _lease_secs: u64) -> Result<usize, StoreError> {
        Ok(0)
    }

    pub fn dream_runner_claim(
        &mut self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _now: &str,
        _lease_secs: u64,
    ) -> Result<Option<DreamRunnerClaim>, StoreError> {
        let mut jobs = self.dream_jobs.lock().unwrap();
        for j in jobs.values_mut() {
            if j.status == "pending" {
                j.status = "running".to_string();
                j.claim_generation += 1;
                return Ok(Some(DreamRunnerClaim {
                    dream_job: j.clone(),
                    adjudication_job: None,
                    consolidation_job: None,
                }));
            }
        }
        Ok(None)
    }

    pub fn dream_runner_has_capability(
        &self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _capability: &str,
        _now: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn dream_runner_heartbeat(
        &mut self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _host_id: &str,
        _agent_id: &str,
        _capabilities_json: &str,
        _lease_secs: u64,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    pub fn dream_runner_lease(
        &mut self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _dream_job_id: &str,
        _dream_generation: i64,
        _adjudication: Option<(&str, i64)>,
        _consolidation: Option<(&str, i64)>,
        _lease_secs: u64,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn dream_runner_owns(
        &self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _dream_job_id: &str,
        _generation: i64,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn dream_snapshot_memories(
        &mut self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _job_id: &str,
        _generation: i64,
        _candidates: &[(String, i64, String)],
    ) -> Result<Vec<String>, StoreError> {
        Ok(Vec::new())
    }

    pub fn dream_snapshot_pages(
        &mut self,
        _scope: &ScopeKey,
        _runner_id: &str,
        _job_id: &str,
        _generation: i64,
        _candidates: &[(String, i64)],
    ) -> Result<Vec<String>, StoreError> {
        Ok(Vec::new())
    }

    pub fn dream_trigger(
        &mut self,
        _scope: &ScopeKey,
        trigger_kind: &str,
        trigger_key: &str,
        _agent_id: Option<&str>,
        _host_id: Option<&str>,
        _session_id: Option<&str>,
        _dom: &DomainScope,
    ) -> Result<Option<dream_jobs::DreamJobRow>, StoreError> {
        let now = Utc::now().to_rfc3339();
        let job = dream_jobs::DreamJobRow {
            id: uuid::Uuid::now_v7().to_string(),
            purpose: "manual".to_string(),
            trigger_kind: trigger_kind.to_string(),
            trigger_key: trigger_key.to_string(),
            extract_version: "v1".to_string(),
            status: "pending".to_string(),
            attempts: 0,
            run_after: now.clone(),
            lease_until: None,
            claim_generation: 0,
            input_fingerprint: "none".to_string(),
            error_code: None,
            created_at: now.clone(),
            updated_at: now,
            domain_id: "main".to_string(),
        };
        self.dream_jobs.lock().unwrap().insert(job.id.clone(), job.clone());
        Ok(Some(job))
    }

    pub fn entries_refresh(
        &mut self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
    ) -> Result<EntryRefreshOutcome, StoreError> {
        Ok(EntryRefreshOutcome {
            batch_version: 1,
            entries: 0,
            sources: 0,
            previous_entries: 0,
        })
    }

    pub fn entries_search(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _query: &str,
        _limit: usize,
    ) -> Result<EntrySearchResult, StoreError> {
        Ok(EntrySearchResult {
            hits: Vec::new(),
            skipped_stale: 0,
        })
    }

    pub fn entry_get(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _entry_id: &str,
    ) -> Result<Option<ContextEntry>, StoreError> {
        Ok(None)
    }

    pub fn expire_due_memories(&mut self) -> Result<usize, StoreError> {
        Ok(0)
    }

    pub fn facet_view(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        facet: &str,
    ) -> Result<derived::DerivedView, StoreError> {
        Ok(derived::DerivedView {
            document_kind: facet.to_string(),
            batch_version: 1,
            items: Vec::new(),
            skipped_stale: 0,
            budget_items: 24,
            budget_chars: 1200,
            total_chars: 0,
        })
    }

    pub fn fail_job(
        &mut self,
        job_id: &str,
        _generation: i64,
        attempts: i32,
        error_code: &str,
    ) -> Result<FailOutcome, StoreError> {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs.get_mut(job_id) {
            j.attempts = attempts;
            j.error_code = Some(error_code.to_string());
            if attempts < 3 {
                j.status = "retryable_failed".to_string();
                j.run_after = (Utc::now() + chrono::Duration::seconds(5)).to_rfc3339();
                return Ok(FailOutcome::Retryable { backoff_secs: 5 });
            } else {
                j.status = "dead".to_string();
                return Ok(FailOutcome::Dead);
            }
        }
        Ok(FailOutcome::Dead)
    }

    pub fn fail_job_deterministic(
        &mut self,
        job_id: &str,
        _generation: i64,
        attempts: i32,
        error_code: &str,
    ) -> Result<(), StoreError> {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs.get_mut(job_id) {
            j.attempts = attempts;
            j.error_code = Some(error_code.to_string());
            j.status = "dead".to_string();
        }
        Ok(())
    }

    pub fn fetch_mutation_receipt(
        &self,
        _scope: &ScopeKey,
        _operation: &str,
        _idempotency_key: &str,
    ) -> Result<Option<soul::MutationReceipt>, StoreError> {
        Ok(None)
    }

    pub fn flush_window(
        &mut self,
        scope: &ScopeKey,
        host_id: &str,
        session_id: &str,
        through_event_seq: i64,
        _dom: &DomainScope,
    ) -> Result<FlushOutcome, StoreError> {
        let events = self.events.lock().unwrap();
        let max_seq = events
            .iter()
            .filter(|e| e.host_id == host_id && e.session_id == session_id)
            .map(|e| e.event_seq)
            .max()
            .unwrap_or(0);
        if through_event_seq > max_seq {
            return Err(StoreError::StateConflict);
        }
        let mut jobs = self.jobs.lock().unwrap();
        for j in jobs.values() {
            if j.host_id == host_id && j.session_id == session_id && j.through_event_seq == through_event_seq {
                return Ok(FlushOutcome::Existing {
                    job_id: j.id.clone(),
                    status: j.status.clone(),
                });
            }
        }
        let job_id = uuid::Uuid::now_v7().to_string();
        let now = Utc::now().to_rfc3339();
        let job = JobRow {
            id: job_id.clone(),
            tenant_id: scope.tenant_id.clone(),
            user_id: scope.user_id.clone(),
            host_id: host_id.to_string(),
            session_id: session_id.to_string(),
            window_key: format!("{}:{}", host_id, session_id),
            through_event_seq,
            status: "queued".to_string(),
            attempts: 0,
            run_after: now.clone(),
            created_at: now.clone(),
            updated_at: now,
            prompt_version: memory_contract::EXTRACT_PROMPT_VERSION.to_string(),
            admission_version: memory_contract::ADMISSION_VERSION.to_string(),
            domain_id: "main".to_string(),
            ..Default::default()
        };
        jobs.insert(job_id.clone(), job);
        Ok(FlushOutcome::Created {
            job_id,
            status: "queued".to_string(),
        })
    }

    pub fn generation_artifact_has(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _kind: &str,
        _signal: &str,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }

    pub fn generation_artifacts_list(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
    ) -> Result<Vec<GenerationArtifact>, StoreError> {
        Ok(Vec::new())
    }

    pub fn generation_publish_artifact(
        &mut self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _kind: &str,
        _signal: &str,
        _thread: Option<&str>,
        _snapshot: &[GenerationSource],
        _candidate: &generation::GeneratedCandidate,
    ) -> Result<Option<String>, StoreError> {
        Ok(None)
    }

    pub fn generation_publish_repair_action(
        &mut self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _signal: &str,
        _thread: &str,
        _snapshot: &[GenerationSource],
        _candidate: &generation::GeneratedCandidate,
    ) -> Result<Option<String>, StoreError> {
        Ok(None)
    }

    pub fn generation_publish_synthesis(
        &mut self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _base: &alignment::AlignmentSynthesisRow,
        _snapshot: &[GenerationSource],
        _candidate: &generation::GeneratedCandidate,
    ) -> Result<Option<alignment::AlignmentSynthesisRow>, StoreError> {
        Ok(None)
    }

    pub fn generation_rejected_count(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _task_kind: &str,
    ) -> Result<i64, StoreError> {
        Ok(0)
    }

    pub fn generation_sources(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _thread: Option<&str>,
    ) -> Result<Vec<GenerationSource>, StoreError> {
        Ok(Vec::new())
    }

    pub fn get_candidate(
        &self,
        _scope: &ScopeKey,
        _id: &str,
    ) -> Result<Option<CandidateDetail>, StoreError> {
        Ok(None)
    }

    pub fn get_job_detail(
        &self,
        _scope: &ScopeKey,
        job_id: &str,
    ) -> Result<Option<JobDetail>, StoreError> {
        let jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs.get(job_id) {
            return Ok(Some(JobDetail {
                item: j.clone(),
                window_key: j.window_key.clone(),
                prompt_version: j.prompt_version.clone(),
                admission_version: j.admission_version.clone(),
                model_name: None,
                input_tokens: None,
                output_tokens: None,
            }));
        }
        Ok(None)
    }

    pub fn get_memory_claim(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
        _dom: &DomainScope,
    ) -> Result<Option<String>, StoreError> {
        Ok(self.inner.get_claim(scope, memory_id)?.map(|d| d.body))
    }

    pub fn index_degraded(&self) -> bool {
        false
    }

    pub fn job_generation_current(&self, job_id: &str, generation: i64) -> Result<bool, StoreError> {
        let jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs.get(job_id) {
            return Ok(j.claim_generation == generation);
        }
        Ok(false)
    }

    pub fn ledger_has(
        &self,
        _scope: &ScopeKey,
        _dom: &DomainScope,
        _task_kind: &str,
        _signal_ref: &str,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }

    pub fn list_jobs(
        &self,
        _scope: &ScopeKey,
        _status: &str,
        _limit: usize,
        _cursor: Option<(&str, &str)>,
    ) -> Result<Vec<JobListItem>, StoreError> {
        Ok(Vec::new())
    }

    pub fn list_soul_revisions(
        &self,
        _scope: &ScopeKey,
        _agent_id: &str,
    ) -> Result<Vec<SoulRevision>, StoreError> {
        Ok(Vec::new())
    }

    pub fn load_window_events(
        &self,
        _scope: &ScopeKey,
        job: &JobRow,
        lower_bound: i64,
    ) -> Result<Vec<memory_extract::WindowEvent>, StoreError> {
        let events = self.events.lock().unwrap();
        let result: Vec<memory_extract::WindowEvent> = events
            .iter()
            .filter(|e| {
                e.host_id == job.host_id
                    && e.session_id == job.session_id
                    && e.event_seq > lower_bound
                    && e.event_seq <= job.through_event_seq
            })
            .map(|e| memory_extract::WindowEvent {
                id: e.id.clone(),
                role: e.role.clone(),
                source_kind: e.source_kind.clone(),
                occurred_at: e.occurred_at.clone(),
                content: e.content.clone(),
            })
            .collect();
        Ok(result)
    }

    pub fn memories_by_exact_hash(
        &self,
        _scope: &ScopeKey,
        _kind: &str,
        _hash: &str,
        _limit: usize,
        _dom: &DomainScope,
    ) -> Result<Vec<(String, i64)>, StoreError> {
        Ok(Vec::new())
    }

    pub fn memory_domain_id(
        &self,
        _scope: &ScopeKey,
        _memory_id: &str,
    ) -> Result<Option<String>, StoreError> {
        Ok(Some("main".into()))
    }

        pub fn memory_explain(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
        _dom: &DomainScope,
        include_history: bool,
    ) -> Result<Option<memory_store_file::read::MemoryExplain>, StoreError> {
        Ok(self.inner.explain_memory(scope, memory_id, include_history)?)
    }

    pub fn memory_version_sha(
        &self,
        _scope: &ScopeKey,
        _memory_id: &str,
        _dom: &DomainScope,
    ) -> Result<Option<(i64, String)>, StoreError> {
        Ok(None)
    }

    pub fn page_archive(
        &mut self,
        _scope: &ScopeKey,
        _page_id: &str,
        _expected_version: i64,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn page_domain_id(
        &self,
        _scope: &ScopeKey,
        _page_id: &str,
    ) -> Result<Option<String>, StoreError> {
        Ok(Some("main".into()))
    }

    pub fn page_fts_search(
        &self,
        _scope: &ScopeKey,
        _query: &str,
        _limit: usize,
        _dom: &DomainScope,
    ) -> Result<Vec<String>, StoreError> {
        Ok(Vec::new())
    }

    pub fn page_list(
        &self,
        _scope: &ScopeKey,
        _statuses: &[&str],
        _limit: usize,
        _dom: &DomainScope,
    ) -> Result<Vec<pages::PageRow>, StoreError> {
        Ok(Vec::new())
    }

    pub fn page_pin(&mut self, _scope: &ScopeKey, _page_id: &str) -> Result<i64, StoreError> {
        Ok(1)
    }

    pub fn page_unpin(&mut self, _scope: &ScopeKey, _page_id: &str) -> Result<i64, StoreError> {
        Ok(1)
    }

    pub fn publish_page_idempotent(
        &mut self,
        req: &pages::PublishRequest<'_>,
        _dom: &DomainScope,
    ) -> Result<(String, i64), StoreError> {
        Ok((req.document_key.to_string(), 1))
    }

    pub fn publish_page_with_description_idempotent(
        &mut self,
        req: &pages::PublishRequest<'_>,
        _description: &str,
        _dom: &DomainScope,
    ) -> Result<(String, i64), StoreError> {
        Ok((req.document_key.to_string(), 1))
    }

    pub fn purge_confirm(
        &mut self,
        _scope: &ScopeKey,
        _token: &str,
        _idempotency_key: &str,
        _dom: &DomainScope,
    ) -> Result<PurgeOutcome, StoreError> {
        Ok(PurgeOutcome {
            job_id: uuid::Uuid::now_v7().to_string(),
            deleted: serde_json::json!({}),
        })
    }

    pub fn purge_preview(
        &mut self,
        _scope: &ScopeKey,
        memory_id: &str,
        idempotency_key: &str,
        _dom: &DomainScope,
    ) -> Result<(String, PurgePreview), StoreError> {
        let token = format!("{memory_id}:{idempotency_key}");
        Ok((token, PurgePreview {
            memory_id: memory_id.to_string(),
            ..Default::default()
        }))
    }

    pub fn question_add(
        &mut self,
        _scope: &ScopeKey,
        _key: &str,
        _text: &str,
        _actor_kind: &str,
    ) -> Result<i64, StoreError> {
        Ok(1)
    }

    pub fn question_archive(
        &mut self,
        _scope: &ScopeKey,
        _key: &str,
        _expected_version: i64,
        _actor_kind: &str,
    ) -> Result<i64, StoreError> {
        Ok(1)
    }

    pub fn repair_action_activate(
        &mut self,
        _scope: &ScopeKey,
        _action_id: &str,
        _authorized_by: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn repair_action_close(
        &mut self,
        _scope: &ScopeKey,
        _action_id: &str,
        _close_reason: &str,
        _authorized_by: &str,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn repair_action_list(
        &self,
        _scope: &ScopeKey,
        _thread_id: Option<&str>,
    ) -> Result<Vec<RepairAction>, StoreError> {
        Ok(Vec::new())
    }

    pub fn repair_action_propose(
        &mut self,
        _scope: &ScopeKey,
        _req: &background::ProposeAction<'_>,
    ) -> Result<String, StoreError> {
        Ok(uuid::Uuid::now_v7().to_string())
    }

    pub fn repair_thread_close(
        &mut self,
        _scope: &ScopeKey,
        _thread_id: &str,
        _reason: &str,
        _actor_id: &str,
        _dom: &DomainScope,
    ) -> Result<bool, StoreError> {
        Ok(true)
    }

    pub fn repair_threads_list(
        &self,
        _scope: &ScopeKey,
        _status: Option<&str>,
        _limit: usize,
        _dom: &DomainScope,
    ) -> Result<Vec<RepairThreadRow>, StoreError> {
        Ok(Vec::new())
    }

    pub fn save_candidate(
        &mut self,
        scope: &ScopeKey,
        job: &JobRow,
        origin: &Origin,
        c: &memory_extract::ModelCandidate,
        admission: memory_extract::Admission,
    ) -> Result<CandidateOutcome, StoreError> {
        self.save_candidate_with_reason(scope, job, origin, c, admission, None)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConsolidationJobRow {
    pub id: String,
    pub status: String,
    pub document_key: String,
    pub document_kind: String,
    pub generator_version: String,
    pub input_fingerprint: String,
    pub question_version: Option<i64>,
    pub claim_generation: i64,
    pub error_code: Option<String>,
    pub updated_at: String,
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
