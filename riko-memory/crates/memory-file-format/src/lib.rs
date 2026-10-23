//! Canonical syntax for `file_store_v1` (doc8/03 §2-§4).
//!
//! This crate deliberately has no database or HTTP dependency. It parses and
//! serializes canonical files, verifies hashes and control metadata, and emits
//! machine-readable quarantine reasons. Generation transactions belong to F2.

pub mod claim;
pub mod evidence;
pub mod format;
pub mod json_files;
pub mod overview;
pub mod quarantine;
pub mod soul;

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

pub const FILE_STORE_FORMAT: &str = "file_store_v1";
pub const FILE_STORE_FORMAT_VERSION: u32 = 1;
pub const EVIDENCE_FORMAT: &str = "evidence_v1";
pub const CLAIM_FORMAT: &str = "memory_v1";
pub const SOUL_FORMAT: &str = "soul_v1";
pub const OVERVIEW_FORMAT: &str = "memory_overview_v1";
pub const OPERATION_FORMAT: &str = "riko-operation-v1";
pub const RECEIPT_FORMAT: &str = "riko-receipt-v1";
pub const FENCE_FORMAT: &str = "riko-fence-v1";

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(crate) fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn is_nfc(value: &str) -> bool {
    value.nfc().eq(value.chars())
}

pub(crate) fn validate_control_text(
    value: &str,
    field: &'static str,
) -> Result<(), quarantine::FormatError> {
    if value.contains('\r') {
        return Err(quarantine::FormatError::new(
            quarantine::ReasonCode::NonLfContainer,
            Some(field),
        ));
    }
    if !is_nfc(value) {
        return Err(quarantine::FormatError::new(
            quarantine::ReasonCode::NonNfcMetadata,
            Some(field),
        ));
    }
    Ok(())
}

pub(crate) fn validate_container(bytes: &[u8]) -> Result<&str, quarantine::FormatError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| quarantine::FormatError::new(quarantine::ReasonCode::InvalidUtf8, None))?;
    if text.contains('\r') {
        return Err(quarantine::FormatError::new(
            quarantine::ReasonCode::NonLfContainer,
            None,
        ));
    }
    Ok(text)
}
