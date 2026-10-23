use crate::quarantine::{validate_identity, FormatError, ReasonCode};
use crate::{is_sha256, validate_control_text, FENCE_FORMAT, OPERATION_FORMAT, RECEIPT_FORMAT};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

/// JSON Schema identifiers for tooling and manifests. Validation is performed
/// by the typed parsers below; F1 does not add a JSON-Schema runtime dependency.
pub const OPERATION_SCHEMA_ID: &str = "riko://schema/operation-v1";
pub const RECEIPT_SCHEMA_ID: &str = "riko://schema/receipt-v1";
pub const FENCE_SCHEMA_ID: &str = "riko://schema/fence-v1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationFile {
    pub format: String,
    pub id: String,
    pub scope_id: String,
    pub domain_id: String,
    pub operation: String,
    pub actor_kind: String,
    pub actor_id: String,
    pub target_id: Option<String>,
    pub expected_version: Option<u64>,
    pub occurred_at: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptFile {
    pub format: String,
    pub scope_id: String,
    pub operation: String,
    pub idempotency_key: String,
    pub request_sha256: String,
    pub result_status: String,
    /// Exact JSON response used by idempotent replay, encoded as a JSON string.
    pub response_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FenceKind {
    Forget,
    Purge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FenceFile {
    pub format: String,
    pub id: String,
    pub scope_id: String,
    pub domain_id: String,
    pub fence_kind: FenceKind,
    pub source_kind: String,
    pub source_id: String,
    pub claim_sha256: Option<String>,
    pub forgotten_memory_id: Option<String>,
    pub actor_kind: String,
    pub actor_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ExpectedIdentity<'a> {
    pub id: Option<&'a str>,
    pub scope_id: &'a str,
    pub domain_id: Option<&'a str>,
    pub actor_kind: Option<&'a str>,
    pub actor_id: Option<&'a str>,
}

fn validate_value(value: &Value) -> Result<(), FormatError> {
    match value {
        Value::String(s) => validate_control_text(s, "json"),
        Value::Array(values) => values.iter().try_for_each(validate_value),
        Value::Object(values) => {
            for (key, value) in values {
                validate_control_text(key, "json")?;
                validate_value(value)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn parse_canonical<T>(bytes: &[u8]) -> Result<T, FormatError>
where
    T: DeserializeOwned + Serialize,
{
    let text = crate::validate_container(bytes)?;
    let value: T = serde_json::from_str(text)
        .map_err(|_| FormatError::new(ReasonCode::InvalidSyntax, None))?;
    let canonical = serde_json::to_vec(&value)
        .map_err(|_| FormatError::new(ReasonCode::InvalidSyntax, None))?;
    if bytes != [canonical.as_slice(), b"\n"].concat() {
        return Err(FormatError::new(ReasonCode::InvalidSyntax, None));
    }
    Ok(value)
}

fn to_canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, FormatError> {
    let mut bytes =
        serde_json::to_vec(value).map_err(|_| FormatError::new(ReasonCode::InvalidSyntax, None))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn validate_required(field: &'static str, value: &str) -> Result<(), FormatError> {
    validate_control_text(value, field)?;
    if value.is_empty() {
        Err(FormatError::new(ReasonCode::InvalidField, Some(field)))
    } else {
        Ok(())
    }
}

impl OperationFile {
    pub fn validate(&self) -> Result<(), FormatError> {
        if self.format != OPERATION_FORMAT {
            return Err(FormatError::new(
                ReasonCode::UnsupportedFormat,
                Some("format"),
            ));
        }
        for (field, value) in [
            ("id", self.id.as_str()),
            ("scope_id", &self.scope_id),
            ("domain_id", &self.domain_id),
            ("operation", &self.operation),
            ("actor_kind", &self.actor_kind),
            ("actor_id", &self.actor_id),
            ("occurred_at", &self.occurred_at),
        ] {
            validate_required(field, value)?;
        }
        if let Some(target) = &self.target_id {
            validate_required("target_id", target)?;
        }
        validate_value(&self.payload)
    }

    pub fn validate_identity(&self, expected: ExpectedIdentity<'_>) -> Result<(), FormatError> {
        if let Some(id) = expected.id {
            validate_identity("id", &self.id, id)?;
        }
        validate_identity("scope_id", &self.scope_id, expected.scope_id)?;
        if let Some(domain) = expected.domain_id {
            validate_identity("domain_id", &self.domain_id, domain)?;
        }
        if let Some(actor) = expected.actor_kind {
            validate_identity("actor_kind", &self.actor_kind, actor)?;
        }
        if let Some(actor) = expected.actor_id {
            validate_identity("actor_id", &self.actor_id, actor)?;
        }
        Ok(())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, FormatError> {
        let v: Self = parse_canonical(bytes)?;
        v.validate()?;
        Ok(v)
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>, FormatError> {
        self.validate()?;
        to_canonical(self)
    }
}

impl ReceiptFile {
    pub fn validate(&self) -> Result<(), FormatError> {
        if self.format != RECEIPT_FORMAT {
            return Err(FormatError::new(
                ReasonCode::UnsupportedFormat,
                Some("format"),
            ));
        }
        for (field, value) in [
            ("scope_id", self.scope_id.as_str()),
            ("operation", &self.operation),
            ("idempotency_key", &self.idempotency_key),
            ("request_sha256", &self.request_sha256),
            ("result_status", &self.result_status),
            ("created_at", &self.created_at),
        ] {
            validate_required(field, value)?;
        }
        if self.idempotency_key.len() > 128
            || !self
                .idempotency_key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(FormatError::new(
                ReasonCode::InvalidField,
                Some("idempotency_key"),
            ));
        }
        if !is_sha256(&self.request_sha256) {
            return Err(FormatError::new(
                ReasonCode::InvalidField,
                Some("request_sha256"),
            ));
        }
        validate_control_text(&self.response_json, "response_json")?;
        let _: Value = serde_json::from_str(&self.response_json)
            .map_err(|_| FormatError::new(ReasonCode::InvalidField, Some("response_json")))?;
        Ok(())
    }

    pub fn validate_scope(&self, scope_id: &str) -> Result<(), FormatError> {
        validate_identity("scope_id", &self.scope_id, scope_id)
    }
    pub fn parse(bytes: &[u8]) -> Result<Self, FormatError> {
        let v: Self = parse_canonical(bytes)?;
        v.validate()?;
        Ok(v)
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>, FormatError> {
        self.validate()?;
        to_canonical(self)
    }
}

impl FenceFile {
    pub fn validate(&self) -> Result<(), FormatError> {
        if self.format != FENCE_FORMAT {
            return Err(FormatError::new(
                ReasonCode::UnsupportedFormat,
                Some("format"),
            ));
        }
        for (field, value) in [
            ("id", self.id.as_str()),
            ("scope_id", &self.scope_id),
            ("domain_id", &self.domain_id),
            ("source_kind", &self.source_kind),
            ("source_id", &self.source_id),
            ("actor_kind", &self.actor_kind),
            ("actor_id", &self.actor_id),
            ("created_at", &self.created_at),
        ] {
            validate_required(field, value)?;
        }
        if !matches!(self.source_kind.as_str(), "evidence" | "memory") {
            return Err(FormatError::new(
                ReasonCode::InvalidField,
                Some("source_kind"),
            ));
        }
        match self.fence_kind {
            FenceKind::Forget => {
                if self.source_kind != "evidence" {
                    return Err(FormatError::new(
                        ReasonCode::InvalidField,
                        Some("source_kind"),
                    ));
                }
                if !self.claim_sha256.as_deref().is_some_and(is_sha256) {
                    return Err(FormatError::new(
                        ReasonCode::MissingField,
                        Some("claim_sha256"),
                    ));
                }
                if !self
                    .forgotten_memory_id
                    .as_deref()
                    .is_some_and(|id| !id.is_empty())
                {
                    return Err(FormatError::new(
                        ReasonCode::MissingField,
                        Some("forgotten_memory_id"),
                    ));
                }
            }
            FenceKind::Purge => {
                if self.claim_sha256.is_some() || self.forgotten_memory_id.is_some() {
                    return Err(FormatError::new(
                        ReasonCode::UnexpectedField,
                        Some("claim_sha256/forgotten_memory_id"),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn validate_identity(&self, expected: ExpectedIdentity<'_>) -> Result<(), FormatError> {
        if let Some(id) = expected.id {
            validate_identity("id", &self.id, id)?;
        }
        validate_identity("scope_id", &self.scope_id, expected.scope_id)?;
        if let Some(domain) = expected.domain_id {
            validate_identity("domain_id", &self.domain_id, domain)?;
        }
        if let Some(actor) = expected.actor_kind {
            validate_identity("actor_kind", &self.actor_kind, actor)?;
        }
        if let Some(actor) = expected.actor_id {
            validate_identity("actor_id", &self.actor_id, actor)?;
        }
        Ok(())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, FormatError> {
        let v: Self = parse_canonical(bytes)?;
        v.validate()?;
        Ok(v)
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>, FormatError> {
        self.validate()?;
        to_canonical(self)
    }
}
