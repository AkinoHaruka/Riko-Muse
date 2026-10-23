//! Vector shards, manifests, and similarity scanning for projection indices.
//!
//! Vectors are stored as partitioned records and manifests under `index/vectors/`.
//! Each entry binds `(object_id, object_kind, version, content_sha256, model_id, dim)`.

use crate::{atomic_replace, read_bytes, StoreError};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VectorEntryMeta {
    pub object_id: String,
    pub object_kind: String, // "claim" | "entry"
    pub version: u64,
    pub content_sha256: String,
    pub model_id: String,
    pub dim: usize,
    pub shard_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VectorManifest {
    pub format_version: String,
    pub model_id: String,
    pub dim: usize,
    pub entries: Vec<VectorEntryMeta>,
}

impl Default for VectorManifest {
    fn default() -> Self {
        Self {
            format_version: "vector_manifest_v1".to_string(),
            model_id: String::new(),
            dim: 0,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorRecord {
    pub object_id: String,
    pub object_kind: String,
    pub version: u64,
    pub content_sha256: String,
    pub model_id: String,
    pub vector: Vec<f32>,
}

pub struct VectorStore;

impl VectorStore {
    const SHARD_FILENAME: &'static str = "shards.json";
    const MANIFEST_FILENAME: &'static str = "manifest.json";

    pub fn manifest_path(vectors_dir: &Path) -> std::path::PathBuf {
        vectors_dir.join(Self::MANIFEST_FILENAME)
    }

    pub fn shards_path(vectors_dir: &Path) -> std::path::PathBuf {
        vectors_dir.join(Self::SHARD_FILENAME)
    }

    pub fn load_manifest(vectors_dir: &Path) -> Result<VectorManifest, StoreError> {
        let path = Self::manifest_path(vectors_dir);
        if !path.exists() {
            return Ok(VectorManifest::default());
        }
        let bytes = read_bytes(&path)?;
        let manifest: VectorManifest = serde_json::from_slice(&bytes)
            .map_err(|e| StoreError::Metadata(format!("invalid vector manifest: {e}")))?;
        Ok(manifest)
    }

    pub fn load_records(vectors_dir: &Path) -> Result<Vec<VectorRecord>, StoreError> {
        let path = Self::shards_path(vectors_dir);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = read_bytes(&path)?;
        let records: Vec<VectorRecord> = serde_json::from_slice(&bytes)
            .map_err(|e| StoreError::Metadata(format!("invalid vector shard file: {e}")))?;
        Ok(records)
    }

    pub fn save_all(
        vectors_dir: &Path,
        model_id: &str,
        dim: usize,
        records: &[VectorRecord],
    ) -> Result<(), StoreError> {
        fs::create_dir_all(vectors_dir)?;
        let mut entries = Vec::with_capacity(records.len());
        for rec in records {
            entries.push(VectorEntryMeta {
                object_id: rec.object_id.clone(),
                object_kind: rec.object_kind.clone(),
                version: rec.version,
                content_sha256: rec.content_sha256.clone(),
                model_id: rec.model_id.clone(),
                dim: rec.vector.len(),
                shard_file: Self::SHARD_FILENAME.to_string(),
            });
        }
        let manifest = VectorManifest {
            format_version: "vector_manifest_v1".to_string(),
            model_id: model_id.to_string(),
            dim,
            entries,
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| StoreError::Metadata(e.to_string()))?;
        let records_bytes = serde_json::to_vec_pretty(&records)
            .map_err(|e| StoreError::Metadata(e.to_string()))?;

        atomic_replace(&Self::shards_path(vectors_dir), &records_bytes)?;
        atomic_replace(&Self::manifest_path(vectors_dir), &manifest_bytes)?;
        Ok(())
    }

    pub fn upsert_vector(
        vectors_dir: &Path,
        object_id: &str,
        object_kind: &str,
        version: u64,
        content_sha256: &str,
        model_id: &str,
        vector: &[f32],
    ) -> Result<(), StoreError> {
        let mut records = Self::load_records(vectors_dir)?;
        records.retain(|r| !(r.object_id == object_id && r.object_kind == object_kind));
        records.push(VectorRecord {
            object_id: object_id.to_string(),
            object_kind: object_kind.to_string(),
            version,
            content_sha256: content_sha256.to_string(),
            model_id: model_id.to_string(),
            vector: vector.to_vec(),
        });
        Self::save_all(vectors_dir, model_id, vector.len(), &records)
    }

    pub fn scan_vectors(
        vectors_dir: &Path,
        query_vector: &[f32],
        min_similarity: f32,
    ) -> Result<Vec<(String, String, f32)>, StoreError> {
        let records = Self::load_records(vectors_dir)?;
        let mut hits = Vec::new();
        for r in records {
            if r.vector.len() != query_vector.len() {
                continue;
            }
            let sim = cosine_similarity(&r.vector, query_vector);
            if sim >= min_similarity {
                hits.push((r.object_id, r.object_kind, sim));
            }
        }
        hits.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        Ok(hits)
    }
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for (&x, &y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom <= f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}
