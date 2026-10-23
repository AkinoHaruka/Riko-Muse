//! F2' direct file storage for the core `file_store_v1` objects.
//!
//! This crate is deliberately parallel to `memory-store-sqlite`. It does not
//! implement generations, manifests, receipts, leases, pins, grants, or a
//! second domain layer. A scope lock serializes writers; each changed file is
//! replaced by a same-directory temporary file and `rename`.

use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use fs2::FileExt;
use memory_domain::{
    claim_sha256, find_quote_span, normalize_v1, MemoryKind, Origin, ScopeKey, USER_MAIN_DOMAIN,
};
use memory_file_format::claim::{ClaimDocument, ClaimEvidenceRef, ClaimFrontMatter};
use memory_file_format::evidence::{DailyNote, EvidenceBlock, EvidenceHeader};
use memory_file_format::format::FormatDefinition;
use memory_file_format::quarantine::FormatError;
use memory_file_format::soul::{SoulDocument, SoulFrontMatter, SOUL_ACTOR_KINDS};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

const SCOPE_FILE: &str = "SCOPE.toml";
const SUPPRESSIONS_FILE: &str = "SUPPRESSIONS";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("file store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("file_store_v1 format error: {0}")]
    Format(#[from] FormatError),
    #[error("invalid store metadata: {0}")]
    Metadata(String),
    #[error("path is unsafe or crosses a symlink: {0}")]
    UnsafePath(String),
    #[error("principal already exists")]
    PrincipalExists,
    #[error("scope does not exist")]
    ScopeNotFound,
    #[error("token output already exists: {0}")]
    TokenFileExists(String),
    #[error("event key already exists with different content")]
    EventConflict,
    #[error("evidence does not exist in this scope")]
    EvidenceNotFound,
    #[error("claim does not exist in this scope")]
    ClaimNotFound,
    #[error("the evidence is not the latest user event")]
    StaleUserEvidence,
    #[error("quote is not a contiguous substring of the evidence")]
    QuoteMismatch,
    #[error("content is suppressed after forget")]
    Suppressed,
    #[error("version conflict")]
    VersionConflict,
    #[error("invalid agent id or actor kind")]
    InvalidSoulIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    Recorded(String),
    AlreadyRecorded(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RememberOutcome {
    Created { memory_id: String, version: u64 },
    Dedup { memory_id: String, version: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoulUpsertOutcome {
    Created { version: u64 },
    Updated { version: u64 },
    Unchanged { version: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrincipalMap {
    scope_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeIdentity {
    tenant_id: String,
    user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Suppression {
    object_id: String,
    content_sha256: String,
}

/// A filesystem-backed store. It contains no in-process mutex: independent
/// handles and processes coordinate through each scope's `LOCK` file.
#[derive(Debug, Clone)]
pub struct FileStore {
    root: PathBuf,
}

/// Symmetrical alias matching `memory_store_sqlite::Store`.
pub type Store = FileStore;

struct ScopeLock {
    file: File,
}

impl Drop for ScopeLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl FileStore {
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        if root.exists() && fs::symlink_metadata(root)?.file_type().is_symlink() {
            return Err(StoreError::UnsafePath(root.display().to_string()));
        }
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        let store = Self { root };
        store.ensure_dir(Path::new("principals"))?;
        store.ensure_dir(Path::new("scopes"))?;
        let format = store.root.join("FORMAT.toml");
        if format.exists() {
            store.reject_symlink(&format)?;
            FormatDefinition::parse(&read_bytes(&format)?)?;
        } else {
            atomic_replace(&format, &FormatDefinition::default().to_bytes()?)?;
        }
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// SQLite-compatible operator shape: generate a token and write it once.
    pub fn principal_add(
        &self,
        tenant_id: &str,
        user_id: &str,
        token_out: &Path,
    ) -> Result<(), StoreError> {
        let mut random = [0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut random);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
        self.principal_add_with_token(tenant_id, user_id, &token)?;
        write_new_token(token_out, &token)
    }

    /// Deterministic-token entry point for provisioning tests and import tools.
    /// The clear token is never stored below the store root.
    pub fn principal_add_with_token(
        &self,
        tenant_id: &str,
        user_id: &str,
        token: &str,
    ) -> Result<String, StoreError> {
        if tenant_id.is_empty() || user_id.is_empty() || token.is_empty() {
            return Err(StoreError::Metadata("empty principal field".into()));
        }
        if self
            .find_scope(&ScopeKey {
                tenant_id: tenant_id.to_owned(),
                user_id: user_id.to_owned(),
            })?
            .is_some()
        {
            return Err(StoreError::PrincipalExists);
        }
        let digest = sha256_hex(token.as_bytes());
        let mapping_path = self.root.join("principals").join(format!("{digest}.toml"));
        if mapping_path.exists() {
            return Err(StoreError::PrincipalExists);
        }
        let scope_id = Uuid::now_v7().to_string();
        let relative = Path::new("scopes").join(&scope_id);
        self.ensure_dir(&relative)?;
        for dir in ["memory", "memory/claims", "agents"] {
            self.ensure_dir(&relative.join(dir))?;
        }
        let scope_dir = self.root.join(&relative);
        atomic_replace(
            &scope_dir.join(SCOPE_FILE),
            &toml_bytes(&ScopeIdentity {
                tenant_id: tenant_id.to_owned(),
                user_id: user_id.to_owned(),
            })?,
        )?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(scope_dir.join("LOCK"))?;
        atomic_replace(
            &mapping_path,
            &toml_bytes(&PrincipalMap {
                scope_id: scope_id.clone(),
            })?,
        )?;
        Ok(scope_id)
    }

    pub fn verify_token(&self, token: &str) -> Result<Option<ScopeKey>, StoreError> {
        let path = self
            .root
            .join("principals")
            .join(format!("{}.toml", sha256_hex(token.as_bytes())));
        if !path.exists() {
            return Ok(None);
        }
        self.reject_symlink(&path)?;
        let map: PrincipalMap = read_toml(&path)?;
        validate_uuid(&map.scope_id)?;
        let identity_path = self.root.join("scopes").join(map.scope_id).join(SCOPE_FILE);
        self.reject_symlink(&identity_path)?;
        let identity: ScopeIdentity = read_toml(&identity_path)?;
        Ok(Some(ScopeKey {
            tenant_id: identity.tenant_id,
            user_id: identity.user_id,
        }))
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
    ) -> Result<IngestOutcome, StoreError> {
        if event_seq < 0 {
            return Err(StoreError::Metadata("negative event_seq".into()));
        }
        let scope_dir = self.scope_dir(scope)?;
        let _lock = self.lock_scope(&scope_dir)?;
        let fingerprint = sha256_hex(content.as_bytes());
        if self.is_suppressed_dir(&scope_dir, &fingerprint)? {
            return Err(StoreError::Suppressed);
        }
        let all = self.all_evidence_dir(&scope_dir)?;
        for block in &all {
            if block.header.content_sha256 == fingerprint {
                return Ok(IngestOutcome::AlreadyRecorded(block.header.id.clone()));
            }
            if block.header.host_id == origin.host_id
                && block.header.session_id == origin.session_id
                && block.header.event_seq == event_seq as u64
            {
                return Err(StoreError::EventConflict);
            }
        }
        let id = Uuid::now_v7().to_string();
        let block = EvidenceBlock::new(
            EvidenceHeader {
                id: id.clone(),
                host_id: origin.host_id.clone(),
                agent_id: origin.agent_id.clone(),
                session_id: origin.session_id.clone(),
                event_seq: event_seq as u64,
                role: role.to_owned(),
                source_kind: source_kind.to_owned(),
                domain_id: USER_MAIN_DOMAIN.to_owned(),
                occurred_at: timestamp(occurred_at),
                content_bytes: 0,
                content_sha256: String::new(),
                received_at: now(),
            },
            content.to_owned(),
        );
        let date = occurred_at.format("%Y-%m-%d").to_string();
        let path = scope_dir.join("memory").join(format!("{date}.md"));
        let mut note = if path.exists() {
            self.reject_symlink(&path)?;
            DailyNote::parse(&read_bytes(&path)?)?
        } else {
            DailyNote {
                date,
                blocks: Vec::new(),
            }
        };
        note.blocks.push(block);
        atomic_replace(&path, &note.to_bytes()?)?;
        Ok(IngestOutcome::Recorded(id))
    }

    pub fn get_evidence(
        &self,
        scope: &ScopeKey,
        evidence_id: &str,
    ) -> Result<Option<EvidenceBlock>, StoreError> {
        validate_uuid(evidence_id)?;
        let scope_dir = self.scope_dir(scope)?;
        for block in self.all_evidence_dir(&scope_dir)? {
            if block.header.id == evidence_id {
                if self.is_suppressed_dir(&scope_dir, &block.header.content_sha256)? {
                    return Ok(None);
                }
                return Ok(Some(block));
            }
        }
        Ok(None)
    }

    pub fn list_daily_evidence(
        &self,
        scope: &ScopeKey,
        date: &str,
    ) -> Result<Vec<EvidenceBlock>, StoreError> {
        validate_date(date)?;
        let scope_dir = self.scope_dir(scope)?;
        let path = scope_dir.join("memory").join(format!("{date}.md"));
        if !path.exists() {
            return Ok(Vec::new());
        }
        self.reject_symlink(&path)?;
        let note = DailyNote::parse(&read_bytes(&path)?)?;
        let suppressions = self.read_suppressions(&scope_dir)?;
        Ok(note
            .blocks
            .into_iter()
            .filter(|b| !suppressed(&suppressions, &b.header.content_sha256))
            .collect())
    }

    pub fn remember(
        &self,
        scope: &ScopeKey,
        origin: &Origin,
        user_evidence_id: &str,
        quote: &str,
        kind: MemoryKind,
    ) -> Result<RememberOutcome, StoreError> {
        let scope_dir = self.scope_dir(scope)?;
        let _lock = self.lock_scope(&scope_dir)?;
        self.remember_locked(&scope_dir, origin, user_evidence_id, quote, kind, None)
    }

    pub fn supersede(
        &self,
        scope: &ScopeKey,
        old_memory_id: &str,
        origin: &Origin,
        user_evidence_id: &str,
        quote: &str,
        kind: MemoryKind,
    ) -> Result<RememberOutcome, StoreError> {
        validate_uuid(old_memory_id)?;
        let scope_dir = self.scope_dir(scope)?;
        let _lock = self.lock_scope(&scope_dir)?;
        let mut old = self
            .read_claim_file(&scope_dir, old_memory_id)?
            .ok_or(StoreError::ClaimNotFound)?;
        if old.front.status != "active" {
            return Err(StoreError::VersionConflict);
        }
        let outcome = self.remember_locked(
            &scope_dir,
            origin,
            user_evidence_id,
            quote,
            kind,
            Some(old_memory_id),
        )?;
        old.front.status = "superseded".into();
        old.front.version += 1;
        old.front.updated_at = now();
        old.front.reason_code = "superseded".into();
        let old_path = claim_path(&scope_dir, old_memory_id)?;
        atomic_replace(&old_path, &old.to_bytes()?)?;
        self.write_overview(&scope_dir)?;
        Ok(outcome)
    }

    pub fn get_claim(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
    ) -> Result<Option<ClaimDocument>, StoreError> {
        validate_uuid(memory_id)?;
        let scope_dir = self.scope_dir(scope)?;
        let Some(claim) = self.read_claim_file(&scope_dir, memory_id)? else {
            return Ok(None);
        };
        if claim.front.status == "forgotten"
            || self.is_suppressed_dir(&scope_dir, &raw_content_hash(&claim.body))?
            || self.is_suppressed_dir(&scope_dir, memory_id)?
        {
            return Ok(None);
        }
        Ok(Some(claim))
    }

    /// Read non-forgotten, non-suppressed claims in deterministic creation order.
    pub fn list_claims(&self, scope: &ScopeKey) -> Result<Vec<ClaimDocument>, StoreError> {
        let scope_dir = self.scope_dir(scope)?;
        let suppressions = self.read_suppressions(&scope_dir)?;
        let mut claims = self
            .all_claims_dir(&scope_dir)?
            .into_iter()
            .filter(|c| {
                c.front.status != "forgotten"
                    && !suppressed(&suppressions, &c.front.id)
                    && !suppressed(&suppressions, &raw_content_hash(&c.body))
            })
            .collect::<Vec<_>>();
        claims.sort_by(|a, b| a.front.created_at.cmp(&b.front.created_at));
        Ok(claims)
    }

    /// Read the curated MEMORY.md overview document if it exists.
    pub fn get_overview(
        &self,
        scope: &ScopeKey,
    ) -> Result<Option<memory_file_format::overview::MemoryOverview>, StoreError> {
        let scope_dir = self.scope_dir(scope)?;
        let path = scope_dir.join("MEMORY.md");
        if !path.exists() {
            return Ok(None);
        }
        self.reject_symlink(&path)?;
        let bytes = read_bytes(&path)?;
        Ok(Some(memory_file_format::overview::MemoryOverview::parse(
            &bytes,
        )?))
    }

    /// F2' checkpoint only: mark the claim forgotten and add one simple
    /// `(object id, raw content hash)` suppression line. F4' owns full closure.
    /// Calling forget on an already forgotten claim is idempotent and does not bump version.
    pub fn forget_claim(&self, scope: &ScopeKey, memory_id: &str) -> Result<(), StoreError> {
        validate_uuid(memory_id)?;
        let scope_dir = self.scope_dir(scope)?;
        let _lock = self.lock_scope(&scope_dir)?;
        let mut claim = self
            .read_claim_file(&scope_dir, memory_id)?
            .ok_or(StoreError::ClaimNotFound)?;
        let content_hash = raw_content_hash(&claim.body);
        let mut records = self.read_suppressions(&scope_dir)?;
        if !records.iter().any(|r| r.object_id == memory_id) {
            records.push(Suppression {
                object_id: memory_id.to_owned(),
                content_sha256: content_hash,
            });
            self.write_suppressions(&scope_dir, &records)?;
        }
        if claim.front.status == "forgotten" {
            return Ok(());
        }
        claim.front.status = "forgotten".into();
        claim.front.version += 1;
        claim.front.updated_at = now();
        claim.front.reason_code = "forgotten".into();
        atomic_replace(&claim_path(&scope_dir, memory_id)?, &claim.to_bytes()?)?;
        self.write_overview(&scope_dir)
    }

    pub fn get_soul(
        &self,
        scope: &ScopeKey,
        agent_id: &str,
    ) -> Result<Option<SoulDocument>, StoreError> {
        let scope_dir = self.scope_dir(scope)?;
        let path = soul_dir(&scope_dir, agent_id)?.join("SOUL.md");
        if !path.exists() {
            return Ok(None);
        }
        self.reject_symlink(&path)?;
        let doc = SoulDocument::parse(&read_bytes(&path)?)?;
        if doc.front.agent_id != agent_id {
            return Err(StoreError::UnsafePath("Soul identity mismatch".into()));
        }
        Ok(Some(doc))
    }

    pub fn upsert_soul(
        &self,
        scope: &ScopeKey,
        agent_id: &str,
        body: &str,
        expected_version: u64,
        actor_kind: &str,
    ) -> Result<SoulUpsertOutcome, StoreError> {
        if agent_id.is_empty()
            || agent_id.chars().count() > 256
            || !SOUL_ACTOR_KINDS.contains(&actor_kind)
        {
            return Err(StoreError::InvalidSoulIdentity);
        }
        let scope_dir = self.scope_dir(scope)?;
        let _lock = self.lock_scope(&scope_dir)?;
        let dir = soul_dir(&scope_dir, agent_id)?;
        self.ensure_absolute_dir(&dir)?;
        self.ensure_absolute_dir(&dir.join("history"))?;
        let current_path = dir.join("SOUL.md");
        let existing = if current_path.exists() {
            self.reject_symlink(&current_path)?;
            Some(SoulDocument::parse(&read_bytes(&current_path)?)?)
        } else {
            None
        };
        let (version, outcome) = match existing {
            None if expected_version == 0 => (1, SoulUpsertOutcome::Created { version: 1 }),
            None => return Err(StoreError::VersionConflict),
            Some(ref old) if old.front.version != expected_version => {
                return Err(StoreError::VersionConflict)
            }
            Some(ref old) if old.body == body => {
                return Ok(SoulUpsertOutcome::Unchanged {
                    version: old.front.version,
                })
            }
            Some(ref old) => {
                let v = old.front.version + 1;
                (v, SoulUpsertOutcome::Updated { version: v })
            }
        };
        let doc = SoulDocument {
            front: SoulFrontMatter {
                agent_id: agent_id.to_owned(),
                version,
                body_sha256: raw_content_hash(body),
                updated_at: now(),
                actor_kind: actor_kind.to_owned(),
            },
            body: body.to_owned(),
        };
        let bytes = doc.to_bytes()?;
        atomic_replace(&dir.join("history").join(format!("{version}.md")), &bytes)?;
        atomic_replace(&current_path, &bytes)?;
        Ok(outcome)
    }

    pub fn get_soul_revision(
        &self,
        scope: &ScopeKey,
        agent_id: &str,
        version: u64,
    ) -> Result<Option<SoulDocument>, StoreError> {
        if version == 0 {
            return Ok(None);
        }
        let scope_dir = self.scope_dir(scope)?;
        let path = soul_dir(&scope_dir, agent_id)?
            .join("history")
            .join(format!("{version}.md"));
        if !path.exists() {
            return Ok(None);
        }
        self.reject_symlink(&path)?;
        Ok(Some(SoulDocument::parse(&read_bytes(&path)?)?))
    }

    fn remember_locked(
        &self,
        scope_dir: &Path,
        origin: &Origin,
        evidence_id: &str,
        quote: &str,
        kind: MemoryKind,
        supersedes: Option<&str>,
    ) -> Result<RememberOutcome, StoreError> {
        validate_uuid(evidence_id)?;
        let evidence = self
            .all_evidence_dir(scope_dir)?
            .into_iter()
            .find(|b| b.header.id == evidence_id)
            .ok_or(StoreError::EvidenceNotFound)?;
        if evidence.header.role != "user"
            || evidence.header.source_kind != "user"
            || evidence.header.host_id != origin.host_id
            || evidence.header.session_id != origin.session_id
        {
            return Err(StoreError::EvidenceNotFound);
        }
        let latest = self
            .all_evidence_dir(scope_dir)?
            .into_iter()
            .filter(|b| {
                b.header.host_id == origin.host_id
                    && b.header.session_id == origin.session_id
                    && b.header.role == "user"
                    && b.header.source_kind == "user"
            })
            .max_by_key(|b| b.header.event_seq)
            .ok_or(StoreError::StaleUserEvidence)?;
        if latest.header.id != evidence_id {
            return Err(StoreError::StaleUserEvidence);
        }
        let (start, end) =
            find_quote_span(&evidence.payload, quote).ok_or(StoreError::QuoteMismatch)?;
        if quote.trim().is_empty() {
            return Err(StoreError::QuoteMismatch);
        }
        if self.is_suppressed_dir(scope_dir, &evidence.header.content_sha256)?
            || self.is_suppressed_dir(scope_dir, &evidence.header.id)?
        {
            return Err(StoreError::Suppressed);
        }
        // F1 claim bodies are canonical containers around the original UTF-8
        // bytes. Normalization is metadata-only and must not rewrite the body.
        let body = quote.to_owned();
        let raw_hash = raw_content_hash(&body);
        if self.is_suppressed_dir(scope_dir, &raw_hash)? {
            return Err(StoreError::Suppressed);
        }
        for existing in self.all_claims_dir(scope_dir)? {
            if supersedes == Some(existing.front.id.as_str()) {
                continue;
            }
            if existing.front.status == "active"
                && existing.front.kind == kind.as_str()
                && existing.front.claim_sha256 == claim_sha256(kind, &body)
            {
                return Ok(RememberOutcome::Dedup {
                    memory_id: existing.front.id,
                    version: existing.front.version,
                });
            }
        }
        let id = Uuid::now_v7().to_string();
        let time = now();
        let claim = ClaimDocument {
            front: ClaimFrontMatter {
                id: id.clone(),
                domain_id: USER_MAIN_DOMAIN.to_owned(),
                kind: kind.as_str().to_owned(),
                status: "active".into(),
                version: 1,
                source_class: "user_explicit".into(),
                claim_sha256: claim_sha256(kind, &body),
                normalized_claim: normalize_v1(&body),
                occurred_at: Some(evidence.header.occurred_at.clone()),
                valid_from: None,
                valid_until: None,
                origin_host_id: origin.host_id.clone(),
                origin_agent_id: origin.agent_id.clone(),
                created_at: time.clone(),
                updated_at: time,
                reason_code: if supersedes.is_some() {
                    "supersede".into()
                } else {
                    "remember".into()
                },
                reason_text: None,
                supersedes: supersedes.map(str::to_owned),
                evidence: vec![ClaimEvidenceRef {
                    id: evidence_id.to_owned(),
                    start_byte: Some(start as u64),
                    end_byte: Some(end as u64),
                    content_sha256: evidence.header.content_sha256,
                }],
            },
            body,
        };
        atomic_replace(&claim_path(scope_dir, &id)?, &claim.to_bytes()?)?;
        self.write_overview(scope_dir)?;
        Ok(RememberOutcome::Created {
            memory_id: id,
            version: 1,
        })
    }

    fn write_overview(&self, scope_dir: &Path) -> Result<(), StoreError> {
        let suppressions = self.read_suppressions(scope_dir)?;
        let mut claims = self
            .all_claims_dir(scope_dir)?
            .into_iter()
            .filter(|c| {
                c.front.status == "active"
                    && !suppressed(&suppressions, &c.front.id)
                    && !suppressed(&suppressions, &raw_content_hash(&c.body))
            })
            .collect::<Vec<_>>();
        claims.sort_by(|a, b| a.front.created_at.cmp(&b.front.created_at));
        let mut text = format!(
            "---\nriko_format: memory_overview_v1\ndomain_id: user_main\nsource_generation: direct\ngenerated_at: {}\n---\n\n# Memory\n\n## Curated\n",
            now()
        );
        for claim in claims {
            text.push_str(&format!(
                "\n<!-- riko:ref memory={} version={} -->\n- {}\n",
                claim.front.id,
                claim.front.version,
                claim.body.replace(['\r', '\n'], " ")
            ));
        }
        text.push_str("\n## Compact\n");
        // Parse before commit so MEMORY.md always conforms to the F1 grammar.
        memory_file_format::overview::MemoryOverview::parse(text.as_bytes())?;
        atomic_replace(&scope_dir.join("MEMORY.md"), text.as_bytes())?;
        Ok(())
    }

    fn scope_dir(&self, scope: &ScopeKey) -> Result<PathBuf, StoreError> {
        self.find_scope(scope)?.ok_or(StoreError::ScopeNotFound)
    }

    fn find_scope(&self, scope: &ScopeKey) -> Result<Option<PathBuf>, StoreError> {
        let scopes = self.root.join("scopes");
        self.reject_symlink(&scopes)?;
        for entry in fs::read_dir(scopes)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() || !entry.file_type()?.is_dir() {
                return Err(StoreError::UnsafePath(entry.path().display().to_string()));
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| StoreError::UnsafePath("non-UTF8 scope id".into()))?;
            validate_uuid(&name)?;
            let identity_path = entry.path().join(SCOPE_FILE);
            self.reject_symlink(&identity_path)?;
            let identity: ScopeIdentity = read_toml(&identity_path)?;
            if identity.tenant_id == scope.tenant_id && identity.user_id == scope.user_id {
                return Ok(Some(entry.path()));
            }
        }
        Ok(None)
    }

    fn lock_scope(&self, scope_dir: &Path) -> Result<ScopeLock, StoreError> {
        let path = scope_dir.join("LOCK");
        if path.exists() {
            self.reject_symlink(&path)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        file.lock_exclusive()?;
        Ok(ScopeLock { file })
    }

    fn all_evidence_dir(&self, scope_dir: &Path) -> Result<Vec<EvidenceBlock>, StoreError> {
        let dir = scope_dir.join("memory");
        self.reject_symlink(&dir)?;
        let mut out = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".md") || name.len() != 13 || validate_date(&name[..10]).is_err() {
                continue;
            }
            self.reject_symlink(&entry.path())?;
            out.extend(DailyNote::parse(&read_bytes(&entry.path())?)?.blocks);
        }
        Ok(out)
    }

    fn all_claims_dir(&self, scope_dir: &Path) -> Result<Vec<ClaimDocument>, StoreError> {
        let dir = scope_dir.join("memory/claims");
        self.reject_symlink(&dir)?;
        let mut out = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() || !entry.file_type()?.is_file() {
                return Err(StoreError::UnsafePath(entry.path().display().to_string()));
            }
            if entry.path().extension().and_then(|v| v.to_str()) != Some("md") {
                continue;
            }
            out.push(ClaimDocument::parse(&read_bytes(&entry.path())?)?);
        }
        Ok(out)
    }

    fn read_claim_file(
        &self,
        scope_dir: &Path,
        memory_id: &str,
    ) -> Result<Option<ClaimDocument>, StoreError> {
        let path = claim_path(scope_dir, memory_id)?;
        if !path.exists() {
            return Ok(None);
        }
        self.reject_symlink(&path)?;
        let claim = ClaimDocument::parse(&read_bytes(&path)?)?;
        if claim.front.id != memory_id {
            return Err(StoreError::UnsafePath("claim identity mismatch".into()));
        }
        Ok(Some(claim))
    }

    fn read_suppressions(&self, scope_dir: &Path) -> Result<Vec<Suppression>, StoreError> {
        let path = scope_dir.join("memory").join(SUPPRESSIONS_FILE);
        if !path.exists() {
            return Ok(Vec::new());
        }
        self.reject_symlink(&path)?;
        let text = String::from_utf8(read_bytes(&path)?)
            .map_err(|_| StoreError::Metadata("non-UTF8 suppressions".into()))?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let (object_id, hash) = line
                    .split_once('\t')
                    .ok_or_else(|| StoreError::Metadata("bad suppression record".into()))?;
                validate_uuid(object_id)?;
                validate_hash(hash)?;
                Ok(Suppression {
                    object_id: object_id.to_owned(),
                    content_sha256: hash.to_owned(),
                })
            })
            .collect()
    }

    fn write_suppressions(
        &self,
        scope_dir: &Path,
        records: &[Suppression],
    ) -> Result<(), StoreError> {
        let mut text = String::new();
        for record in records {
            text.push_str(&record.object_id);
            text.push('\t');
            text.push_str(&record.content_sha256);
            text.push('\n');
        }
        atomic_replace(
            &scope_dir.join("memory").join(SUPPRESSIONS_FILE),
            text.as_bytes(),
        )?;
        Ok(())
    }

    fn is_suppressed_dir(&self, scope_dir: &Path, hash: &str) -> Result<bool, StoreError> {
        Ok(suppressed(&self.read_suppressions(scope_dir)?, hash))
    }

    fn ensure_dir(&self, relative: &Path) -> Result<(), StoreError> {
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(StoreError::UnsafePath(relative.display().to_string()));
        }
        let mut current = self.root.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                unreachable!()
            };
            current.push(name);
            if current.exists() {
                let meta = fs::symlink_metadata(&current)?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(StoreError::UnsafePath(current.display().to_string()));
                }
            } else {
                fs::create_dir(&current)?;
            }
        }
        Ok(())
    }

    fn ensure_absolute_dir(&self, path: &Path) -> Result<(), StoreError> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| StoreError::UnsafePath(path.display().to_string()))?;
        self.ensure_dir(relative)
    }

    fn reject_symlink(&self, path: &Path) -> Result<(), StoreError> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| StoreError::UnsafePath(path.display().to_string()))?;
        let mut current = self.root.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(StoreError::UnsafePath(path.display().to_string()));
            };
            current.push(name);
            let meta = fs::symlink_metadata(&current)?;
            if meta.file_type().is_symlink() {
                return Err(StoreError::UnsafePath(current.display().to_string()));
            }
        }
        Ok(())
    }
}

fn suppressed(records: &[Suppression], key: &str) -> bool {
    records
        .iter()
        .any(|r| r.object_id == key || r.content_sha256 == key)
}

fn claim_path(scope_dir: &Path, id: &str) -> Result<PathBuf, StoreError> {
    validate_uuid(id)?;
    Ok(scope_dir.join("memory/claims").join(format!("{id}.md")))
}

fn soul_dir(scope_dir: &Path, agent_id: &str) -> Result<PathBuf, StoreError> {
    if agent_id.is_empty() || agent_id.chars().count() > 256 {
        return Err(StoreError::InvalidSoulIdentity);
    }
    // User-controlled agent IDs never become path components.
    Ok(scope_dir
        .join("agents")
        .join(format!("a-{}", sha256_hex(agent_id.as_bytes()))))
}

fn raw_content_hash(text: &str) -> String {
    sha256_hex(text.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn timestamp(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn now() -> String {
    timestamp(&Utc::now())
}

fn validate_uuid(value: &str) -> Result<(), StoreError> {
    let parsed = Uuid::parse_str(value)
        .map_err(|_| StoreError::UnsafePath(format!("invalid generated id: {value}")))?;
    if parsed.to_string() != value {
        return Err(StoreError::UnsafePath(format!("non-canonical id: {value}")));
    }
    Ok(())
}

fn validate_hash(value: &str) -> Result<(), StoreError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(StoreError::Metadata("invalid sha256".into()))
    }
}

fn validate_date(date: &str) -> Result<(), StoreError> {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|_| ())
        .map_err(|_| StoreError::UnsafePath("invalid date".into()))
}

fn toml_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    toml::to_string(value)
        .map(String::into_bytes)
        .map_err(|e| StoreError::Metadata(e.to_string()))
}

fn read_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, StoreError> {
    let bytes = read_bytes(path)?;
    toml::from_str(
        std::str::from_utf8(&bytes).map_err(|_| StoreError::Metadata("non-UTF8 TOML".into()))?,
    )
    .map_err(|e| StoreError::Metadata(e.to_string()))
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, StoreError> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn write_new_token(path: &Path, token: &str) -> Result<(), StoreError> {
    if path.exists() {
        return Err(StoreError::TokenFileExists(path.display().to_string()));
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(token.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    atomic_replace_with(path, bytes, || Ok(()))
}

fn atomic_replace_with<F>(path: &Path, bytes: &[u8], before_rename: F) -> Result<(), StoreError>
where
    F: FnOnce() -> Result<(), StoreError>,
{
    let parent = path
        .parent()
        .ok_or_else(|| StoreError::UnsafePath(path.display().to_string()))?;
    let temp = parent.join(format!(".riko-tmp-{}", Uuid::now_v7()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        before_rename()?;
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests;
