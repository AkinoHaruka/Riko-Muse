use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

/// Stable reason codes consumed by quarantine/doctor tooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    InvalidUtf8,
    NonLfContainer,
    NonNfcMetadata,
    InvalidSyntax,
    UnsupportedFormat,
    MissingField,
    UnexpectedField,
    InvalidField,
    HashMismatch,
    DuplicateId,
    VersionRegression,
    DanglingReference,
    IdMismatch,
    ScopeMismatch,
    DomainMismatch,
    ActorMismatch,
}

impl ReasonCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidUtf8 => "invalid_utf8",
            Self::NonLfContainer => "non_lf_container",
            Self::NonNfcMetadata => "non_nfc_metadata",
            Self::InvalidSyntax => "invalid_syntax",
            Self::UnsupportedFormat => "unsupported_format",
            Self::MissingField => "missing_field",
            Self::UnexpectedField => "unexpected_field",
            Self::InvalidField => "invalid_field",
            Self::HashMismatch => "hash_mismatch",
            Self::DuplicateId => "duplicate_id",
            Self::VersionRegression => "version_regression",
            Self::DanglingReference => "dangling_reference",
            Self::IdMismatch => "id_mismatch",
            Self::ScopeMismatch => "scope_mismatch",
            Self::DomainMismatch => "domain_mismatch",
            Self::ActorMismatch => "actor_mismatch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("file format validation failed: {code:?} ({field:?})")]
pub struct FormatError {
    pub code: ReasonCode,
    pub field: Option<&'static str>,
}

impl FormatError {
    pub const fn new(code: ReasonCode, field: Option<&'static str>) -> Self {
        Self { code, field }
    }
}

/// Privacy-safe report: never contains claim, evidence, Soul, or response bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineReport {
    pub path: String,
    pub reason_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_id: Option<String>,
}

impl QuarantineReport {
    pub fn from_error(
        path: impl Into<String>,
        object_id: Option<&str>,
        error: &FormatError,
    ) -> Self {
        Self {
            path: path.into(),
            reason_code: error.code.as_str().to_owned(),
            field: error.field.map(str::to_owned),
            object_id: object_id.map(str::to_owned),
        }
    }
}

/// Cross-file checks that cannot be decided by a single parser.
#[derive(Debug, Default)]
pub struct CorpusValidator {
    ids: HashSet<String>,
    versions: HashMap<String, u64>,
    references: Vec<(String, String)>,
}

impl CorpusValidator {
    pub fn add_object(&mut self, id: &str, version: u64) -> Result<(), FormatError> {
        if !self.ids.insert(id.to_owned()) {
            return Err(FormatError::new(ReasonCode::DuplicateId, Some("id")));
        }
        self.versions.insert(id.to_owned(), version);
        Ok(())
    }

    pub fn require_newer_version(
        &self,
        id: &str,
        previous_version: u64,
        next_version: u64,
    ) -> Result<(), FormatError> {
        if next_version <= previous_version
            || self.versions.get(id).is_some_and(|v| next_version <= *v)
        {
            return Err(FormatError::new(
                ReasonCode::VersionRegression,
                Some("version"),
            ));
        }
        Ok(())
    }

    pub fn add_reference(&mut self, owner_id: &str, target_id: &str) {
        self.references
            .push((owner_id.to_owned(), target_id.to_owned()));
    }

    pub fn validate_references(&self) -> Result<(), FormatError> {
        if self
            .references
            .iter()
            .any(|(_, target)| !self.ids.contains(target))
        {
            return Err(FormatError::new(
                ReasonCode::DanglingReference,
                Some("reference"),
            ));
        }
        Ok(())
    }
}

pub fn validate_identity(
    field: &'static str,
    actual: &str,
    expected: &str,
) -> Result<(), FormatError> {
    if actual == expected {
        return Ok(());
    }
    let code = match field {
        "id" => ReasonCode::IdMismatch,
        "scope_id" => ReasonCode::ScopeMismatch,
        "domain_id" => ReasonCode::DomainMismatch,
        "actor_kind" | "actor_id" => ReasonCode::ActorMismatch,
        _ => ReasonCode::InvalidField,
    };
    Err(FormatError::new(code, Some(field)))
}
