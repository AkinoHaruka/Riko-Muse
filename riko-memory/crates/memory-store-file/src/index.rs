//! Projection index: SQLite-backed FTS5 + CJK bigrams + object metadata projection,
//! along with manifest fingerprint tracking and index rebuild protocols.

use crate::vectors::VectorStore;
use crate::{
    atomic_replace, now, raw_content_hash, read_bytes, sha256_hex, suppressed, validate_date,
    validate_uuid, DailyNote, StoreError, Suppression, SUPPRESSIONS_FILE,
};
use memory_file_format::claim::ClaimDocument;
use memory_recall::cjk_bigrams;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const INDEX_FORMAT_VERSION: &str = "file_index_v1";
pub const INDEX_BUILDER_VERSION: &str = "f3prime_v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexManifest {
    pub format_version: String,
    pub builder_version: String,
    pub created_at: String,
    pub source_fingerprints: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexFreshness {
    Fresh,
    Stale,
    Missing,
    Corrupt,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexRebuildReport {
    pub claims_indexed: usize,
    pub entries_indexed: usize,
    pub vectors_retained: usize,
    pub source_files_count: usize,
}

pub struct ProjectionIndex;

impl ProjectionIndex {
    pub const INDEX_DIR: &'static str = "index";
    pub const MANIFEST_FILE: &'static str = "manifest.json";
    pub const LEXICAL_DB_FILE: &'static str = "lexical.db";
    pub const VECTORS_DIR: &'static str = "vectors";

    pub fn index_dir(scope_dir: &Path) -> PathBuf {
        scope_dir.join(Self::INDEX_DIR)
    }

    pub fn manifest_path(scope_dir: &Path) -> PathBuf {
        Self::index_dir(scope_dir).join(Self::MANIFEST_FILE)
    }

    pub fn db_path(scope_dir: &Path) -> PathBuf {
        Self::index_dir(scope_dir).join(Self::LEXICAL_DB_FILE)
    }

    pub fn vectors_dir(scope_dir: &Path) -> PathBuf {
        Self::index_dir(scope_dir).join(Self::VECTORS_DIR)
    }

    /// Compute SHA256 fingerprints of all canonical source files under a scope.
    pub fn compute_source_fingerprints(
        scope_dir: &Path,
    ) -> Result<BTreeMap<String, String>, StoreError> {
        let mut fingerprints = BTreeMap::new();

        // 1. Daily notes
        let memory_dir = scope_dir.join("memory");
        if memory_dir.exists() {
            for entry in fs::read_dir(&memory_dir)? {
                let entry = entry?;
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy();
                if name.len() == 13 && name.ends_with(".md") && validate_date(&name[..10]).is_ok() {
                    let bytes = read_bytes(&entry.path())?;
                    let rel = format!("memory/{name}");
                    fingerprints.insert(rel, sha256_hex(&bytes));
                }
            }
            // 2. SUPPRESSIONS
            let supp_path = memory_dir.join(SUPPRESSIONS_FILE);
            if supp_path.exists() {
                let bytes = read_bytes(&supp_path)?;
                fingerprints.insert(format!("memory/{SUPPRESSIONS_FILE}"), sha256_hex(&bytes));
            }
        }

        // 3. Claims
        let claims_dir = scope_dir.join("memory/claims");
        if claims_dir.exists() {
            for entry in fs::read_dir(&claims_dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    let id_part = &name[..name.len().saturating_sub(3)];
                    if validate_uuid(id_part).is_ok() {
                        let bytes = read_bytes(&entry.path())?;
                        let rel = format!("memory/claims/{name}");
                        fingerprints.insert(rel, sha256_hex(&bytes));
                    }
                }
            }
        }

        // 4. MEMORY.md if present
        let mem_overview = scope_dir.join("MEMORY.md");
        if mem_overview.exists() {
            let bytes = read_bytes(&mem_overview)?;
            fingerprints.insert("MEMORY.md".to_string(), sha256_hex(&bytes));
        }

        // 5. Alignment if present
        let align = scope_dir.join("memory/alignment.md");
        if align.exists() {
            let bytes = read_bytes(&align)?;
            fingerprints.insert("memory/alignment.md".to_string(), sha256_hex(&bytes));
        }

        // 6. Upkeep ledger if present
        let ledger = scope_dir.join("memory/LEDGER");
        if ledger.exists() {
            let bytes = read_bytes(&ledger)?;
            fingerprints.insert("memory/LEDGER".to_string(), sha256_hex(&bytes));
        }

        // 7. Bank facets if present
        let bank_dir = scope_dir.join("memory/bank");
        if bank_dir.exists() {
            for entry in fs::read_dir(&bank_dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    let bytes = read_bytes(&entry.path())?;
                    let rel = format!("memory/bank/{name}");
                    fingerprints.insert(rel, sha256_hex(&bytes));
                }
            }
        }

        Ok(fingerprints)
    }

    /// Check the freshness of the projection index against the canonical source files.
    pub fn check_freshness(scope_dir: &Path) -> Result<IndexFreshness, StoreError> {
        let manifest_p = Self::manifest_path(scope_dir);
        let db_p = Self::db_path(scope_dir);
        if !manifest_p.exists() || !db_p.exists() {
            return Ok(IndexFreshness::Missing);
        }

        let manifest_bytes = match read_bytes(&manifest_p) {
            Ok(b) => b,
            Err(_) => return Ok(IndexFreshness::Corrupt),
        };
        let manifest: IndexManifest = match serde_json::from_slice(&manifest_bytes) {
            Ok(m) => m,
            Err(_) => return Ok(IndexFreshness::Corrupt),
        };

        if manifest.format_version != INDEX_FORMAT_VERSION {
            return Ok(IndexFreshness::Stale);
        }

        // Verify SQLite db integrity
        let conn = match Connection::open_with_flags(
            &db_p,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(c) => c,
            Err(_) => return Ok(IndexFreshness::Corrupt),
        };
        if conn
            .query_row("SELECT count(*) FROM projected_items", [], |_| Ok(()))
            .is_err()
        {
            return Ok(IndexFreshness::Corrupt);
        }

        let current_fingerprints = Self::compute_source_fingerprints(scope_dir)?;
        if manifest.source_fingerprints != current_fingerprints {
            return Ok(IndexFreshness::Stale);
        }

        Ok(IndexFreshness::Fresh)
    }

    /// Open or create the index SQLite connection with FTS5 and tables initialized.
    pub fn open_db(db_path: &Path) -> Result<Connection, StoreError> {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)
            .map_err(|e| StoreError::Metadata(format!("cannot open projection sqlite db: {e}")))?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS projected_items (
                object_id TEXT PRIMARY KEY,
                object_kind TEXT NOT NULL,
                version INTEGER NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                valid_until TEXT,
                occurred_at TEXT,
                updated_at TEXT,
                content_sha256 TEXT NOT NULL,
                preview TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_projected_kind ON projected_items(object_kind);
            CREATE INDEX IF NOT EXISTS idx_projected_status ON projected_items(status);

            CREATE VIRTUAL TABLE IF NOT EXISTS fts_items USING fts5(
                object_id UNINDEXED,
                object_kind UNINDEXED,
                content
            );

            CREATE TABLE IF NOT EXISTS cjk_grams (
                object_id TEXT NOT NULL,
                object_kind TEXT NOT NULL,
                gram TEXT NOT NULL,
                PRIMARY KEY(object_id, object_kind, gram)
            );
            CREATE INDEX IF NOT EXISTS idx_cjk_grams_gram ON cjk_grams(gram);
            ",
        )
        .map_err(|e| StoreError::Metadata(format!("cannot initialize projection schema: {e}")))?;

        Ok(conn)
    }

    /// Fully rebuild the projection index from canonical files.
    pub fn rebuild(scope_dir: &Path) -> Result<IndexRebuildReport, StoreError> {
        let index_dir = Self::index_dir(scope_dir);
        let vectors_dir = Self::vectors_dir(scope_dir);
        fs::create_dir_all(&index_dir)?;

        let db_file = Self::db_path(scope_dir);
        let temp_db_file = index_dir.join(format!(".tmp-lexical-{}.db", uuid::Uuid::now_v7()));

        let mut conn = Self::open_db(&temp_db_file)?;

        // Read suppressions
        let supp_file = scope_dir.join("memory").join(SUPPRESSIONS_FILE);
        let suppressions = if supp_file.exists() {
            let bytes = read_bytes(&supp_file)?;
            let text = String::from_utf8(bytes).map_err(|_| StoreError::Metadata("non-utf8".into()))?;
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| {
                    let (id, hash) = l.split_once('\t')?;
                    Some(Suppression {
                        object_id: id.to_string(),
                        content_sha256: hash.to_string(),
                    })
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let tx = conn
            .transaction()
            .map_err(|e| StoreError::Metadata(e.to_string()))?;

        let mut claims_indexed = 0;
        let mut entries_indexed = 0;
        let mut active_hashes = HashSet::new();

        // 1. Index claims
        let claims_dir = scope_dir.join("memory/claims");
        if claims_dir.exists() {
            for entry in fs::read_dir(&claims_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let bytes = read_bytes(&path)?;
                let Ok(claim) = ClaimDocument::parse(&bytes) else {
                    continue;
                };
                if claim.front.status == "forgotten" {
                    continue;
                }
                let raw_hash = raw_content_hash(&claim.body);
                if suppressed(&suppressions, &claim.front.id)
                    || suppressed(&suppressions, &raw_hash)
                    || suppressed(&suppressions, &claim.front.claim_sha256)
                {
                    continue;
                }

                // Check that at least one evidence source is not suppressed
                let has_unsuppressed_source = claim.front.evidence.iter().any(|ev| {
                    !suppressed(&suppressions, &ev.id) && !suppressed(&suppressions, &ev.content_sha256)
                });
                if !has_unsuppressed_source {
                    continue;
                }

                active_hashes.insert((claim.front.id.clone(), "claim".to_string(), claim.front.version, raw_hash.clone()));

                // Insert into projected_items
                tx.execute(
                    "INSERT OR REPLACE INTO projected_items (
                        object_id, object_kind, version, kind, status, valid_until, occurred_at, updated_at, content_sha256, preview
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        claim.front.id,
                        "claim",
                        claim.front.version as i64,
                        claim.front.kind,
                        claim.front.status,
                        claim.front.valid_until,
                        claim.front.occurred_at,
                        claim.front.updated_at,
                        raw_hash,
                        claim.body,
                    ],
                )
                .map_err(|e| StoreError::Metadata(e.to_string()))?;

                // Insert FTS tokens
                tx.execute(
                    "INSERT INTO fts_items (object_id, object_kind, content) VALUES (?1, ?2, ?3)",
                    params![claim.front.id, "claim", claim.body],
                )
                .map_err(|e| StoreError::Metadata(e.to_string()))?;

                // Insert CJK bigrams
                let grams = cjk_bigrams(&claim.body);
                for g in grams {
                    tx.execute(
                        "INSERT OR IGNORE INTO cjk_grams (object_id, object_kind, gram) VALUES (?1, ?2, ?3)",
                        params![claim.front.id, "claim", g],
                    )
                    .map_err(|e| StoreError::Metadata(e.to_string()))?;
                }

                claims_indexed += 1;
            }
        }

        // 2. Index conversation entries from daily notes
        let memory_dir = scope_dir.join("memory");
        if memory_dir.exists() {
            for entry in fs::read_dir(&memory_dir)? {
                let entry = entry?;
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy();
                if !(name.len() == 13 && name.ends_with(".md") && validate_date(&name[..10]).is_ok()) {
                    continue;
                }
                let bytes = read_bytes(&entry.path())?;
                let Ok(note) = DailyNote::parse(&bytes) else {
                    continue;
                };
                for block in note.blocks {
                    if suppressed(&suppressions, &block.header.id)
                        || suppressed(&suppressions, &block.header.content_sha256)
                    {
                        continue;
                    }

                    active_hashes.insert((
                        block.header.id.clone(),
                        "entry".to_string(),
                        1,
                        block.header.content_sha256.clone(),
                    ));

                    // Insert entry into projected_items
                    tx.execute(
                        "INSERT OR REPLACE INTO projected_items (
                            object_id, object_kind, version, kind, status, valid_until, occurred_at, updated_at, content_sha256, preview
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        params![
                            block.header.id,
                            "entry",
                            1_i64,
                            block.header.source_kind,
                            "active",
                            None::<String>,
                            Some(block.header.occurred_at),
                            Some(block.header.received_at),
                            block.header.content_sha256,
                            block.payload,
                        ],
                    )
                    .map_err(|e| StoreError::Metadata(e.to_string()))?;

                    // Insert FTS tokens
                    tx.execute(
                        "INSERT INTO fts_items (object_id, object_kind, content) VALUES (?1, ?2, ?3)",
                        params![block.header.id, "entry", block.payload],
                    )
                    .map_err(|e| StoreError::Metadata(e.to_string()))?;

                    // Insert CJK bigrams
                    let grams = cjk_bigrams(&block.payload);
                    for g in grams {
                        tx.execute(
                            "INSERT OR IGNORE INTO cjk_grams (object_id, object_kind, gram) VALUES (?1, ?2, ?3)",
                            params![block.header.id, "entry", g],
                        )
                        .map_err(|e| StoreError::Metadata(e.to_string()))?;
                    }

                    entries_indexed += 1;
                }
            }
        }

        tx.commit()
            .map_err(|e| StoreError::Metadata(e.to_string()))?;
        drop(conn);

        // Replace db file
        fs::rename(&temp_db_file, &db_file)?;

        // 3. Reconcile vectors: preserve existing valid vectors whose hash/version matches active source objects
        let mut vectors_retained = 0;
        if vectors_dir.exists() {
            let existing_records = VectorStore::load_records(&vectors_dir)?;
            let manifest = VectorStore::load_manifest(&vectors_dir)?;
            let mut valid_records = Vec::new();
            for r in existing_records {
                if active_hashes.contains(&(r.object_id.clone(), r.object_kind.clone(), r.version, r.content_sha256.clone())) {
                    valid_records.push(r);
                }
            }
            vectors_retained = valid_records.len();
            VectorStore::save_all(&vectors_dir, &manifest.model_id, manifest.dim, &valid_records)?;
        }

        // 4. Compute source fingerprints and write manifest
        let fingerprints = Self::compute_source_fingerprints(scope_dir)?;
        let manifest = IndexManifest {
            format_version: INDEX_FORMAT_VERSION.to_string(),
            builder_version: INDEX_BUILDER_VERSION.to_string(),
            created_at: now(),
            source_fingerprints: fingerprints.clone(),
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| StoreError::Metadata(e.to_string()))?;
        atomic_replace(&Self::manifest_path(scope_dir), &manifest_bytes)?;

        Ok(IndexRebuildReport {
            claims_indexed,
            entries_indexed,
            vectors_retained,
            source_files_count: fingerprints.len(),
        })
    }
}
