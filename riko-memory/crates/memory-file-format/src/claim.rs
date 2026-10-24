use crate::quarantine::{FormatError, ReasonCode};
use crate::{is_sha256, validate_control_text, CLAIM_FORMAT};
use memory_domain::{claim_sha256, normalize_v1, MemoryKind};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimEvidenceRef {
    pub id: String,
    pub start_byte: Option<u64>,
    pub end_byte: Option<u64>,
    pub content_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimFrontMatter {
    pub id: String,
    pub domain_id: String,
    pub kind: String,
    pub status: String,
    pub version: u64,
    pub source_class: String,
    pub claim_sha256: String,
    pub normalized_claim: String,
    pub occurred_at: Option<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub origin_host_id: String,
    pub origin_agent_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub reason_code: String,
    pub reason_text: Option<String>,
    pub supersedes: Option<String>,
    pub slot: Option<String>,
    pub precedence: Option<String>,
    pub evidence: Vec<ClaimEvidenceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimDocument {
    pub front: ClaimFrontMatter,
    /// Original UTF-8 claim bytes represented as a Rust string. Never normalized.
    pub body: String,
}

fn kind(value: &str) -> Option<MemoryKind> {
    match value {
        "fact" => Some(MemoryKind::Fact),
        "preference" => Some(MemoryKind::Preference),
        "instruction" => Some(MemoryKind::Instruction),
        "episode" => Some(MemoryKind::Episode),
        _ => None,
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn optional(value: &Option<String>) -> String {
    value
        .as_deref()
        .map(json_string)
        .unwrap_or_else(|| "null".to_owned())
}

fn take<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    prefix: &'static str,
) -> Result<&'a str, FormatError> {
    lines
        .next()
        .and_then(|line| line.strip_prefix(prefix))
        .ok_or_else(|| {
            FormatError::new(
                ReasonCode::InvalidSyntax,
                Some(prefix.trim_end_matches(": ")),
            )
        })
}

fn parse_json_string(value: &str, field: &'static str) -> Result<String, FormatError> {
    let parsed: String = serde_json::from_str(value)
        .map_err(|_| FormatError::new(ReasonCode::InvalidField, Some(field)))?;
    validate_control_text(&parsed, field)?;
    Ok(parsed)
}

fn parse_optional(value: &str, field: &'static str) -> Result<Option<String>, FormatError> {
    if value == "null" {
        Ok(None)
    } else {
        parse_json_string(value, field).map(Some)
    }
}

impl ClaimDocument {
    pub fn validate(&self) -> Result<(), FormatError> {
        let f = &self.front;
        for (field, value) in [
            ("id", f.id.as_str()),
            ("domain_id", &f.domain_id),
            ("kind", &f.kind),
            ("status", &f.status),
            ("source_class", &f.source_class),
            ("claim_sha256", &f.claim_sha256),
            ("normalized_claim", &f.normalized_claim),
            ("origin_host_id", &f.origin_host_id),
            ("origin_agent_id", &f.origin_agent_id),
            ("created_at", &f.created_at),
            ("updated_at", &f.updated_at),
            ("reason_code", &f.reason_code),
        ] {
            validate_control_text(value, field)?;
            if value.is_empty() {
                return Err(FormatError::new(ReasonCode::InvalidField, Some(field)));
            }
        }
        for (field, value) in [
            ("occurred_at", &f.occurred_at),
            ("valid_from", &f.valid_from),
            ("valid_until", &f.valid_until),
            ("reason_text", &f.reason_text),
            ("supersedes", &f.supersedes),
            ("slot", &f.slot),
            ("precedence", &f.precedence),
        ] {
            if let Some(value) = value {
                validate_control_text(value, field)?;
            }
        }
        if let Some(ref prec) = f.precedence {
            if !matches!(prec.as_str(), "session" | "project" | "global") {
                return Err(FormatError::new(ReasonCode::InvalidField, Some("precedence")));
            }
        }
        let Some(kind) = kind(&f.kind) else {
            return Err(FormatError::new(ReasonCode::InvalidField, Some("kind")));
        };
        if !matches!(
            f.status.as_str(),
            "active" | "superseded" | "expired" | "forgotten"
        ) {
            return Err(FormatError::new(ReasonCode::InvalidField, Some("status")));
        }
        if !matches!(f.source_class.as_str(), "user_explicit" | "manual_edit") {
            return Err(FormatError::new(
                ReasonCode::InvalidField,
                Some("source_class"),
            ));
        }
        if f.version == 0 {
            return Err(FormatError::new(ReasonCode::InvalidField, Some("version")));
        }
        if self.body.is_empty() {
            return Err(FormatError::new(ReasonCode::InvalidField, Some("body")));
        }
        if !is_sha256(&f.claim_sha256)
            || claim_sha256(kind, &self.body) != f.claim_sha256.to_ascii_lowercase()
        {
            return Err(FormatError::new(
                ReasonCode::HashMismatch,
                Some("claim_sha256"),
            ));
        }
        if normalize_v1(&self.body) != f.normalized_claim {
            return Err(FormatError::new(
                ReasonCode::HashMismatch,
                Some("normalized_claim"),
            ));
        }
        if f.evidence.is_empty() {
            return Err(FormatError::new(ReasonCode::MissingField, Some("evidence")));
        }
        let mut ids = HashSet::new();
        for evidence in &f.evidence {
            validate_control_text(&evidence.id, "evidence.id")?;
            if !ids.insert(evidence.id.as_str()) {
                return Err(FormatError::new(
                    ReasonCode::DuplicateId,
                    Some("evidence.id"),
                ));
            }
            match (evidence.start_byte, evidence.end_byte) {
                (Some(start), Some(end)) if end > start => {}
                (None, None) => {}
                _ => {
                    return Err(FormatError::new(ReasonCode::InvalidField, Some("end_byte")));
                }
            }
            if !is_sha256(&evidence.content_sha256) {
                return Err(FormatError::new(
                    ReasonCode::InvalidField,
                    Some("content_sha256"),
                ));
            }
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, FormatError> {
        self.validate()?;
        let f = &self.front;
        let mut header_str = format!(
            "---\nriko_format: {CLAIM_FORMAT}\nid: {}\ndomain_id: {}\nkind: {}\nstatus: {}\nversion: {}\nsource_class: {}\nclaim_sha256: {}\nnormalized_claim: {}\noccurred_at: {}\nvalid_from: {}\nvalid_until: {}\norigin_host_id: {}\norigin_agent_id: {}\ncreated_at: {}\nupdated_at: {}\nreason_code: {}\nreason_text: {}\nsupersedes: {}\n",
            f.id,
            f.domain_id,
            f.kind,
            f.status,
            f.version,
            f.source_class,
            f.claim_sha256,
            json_string(&f.normalized_claim),
            optional(&f.occurred_at),
            optional(&f.valid_from),
            optional(&f.valid_until),
            f.origin_host_id,
            f.origin_agent_id,
            f.created_at,
            f.updated_at,
            f.reason_code,
            optional(&f.reason_text),
            optional(&f.supersedes),
        );
        if let Some(ref slot) = f.slot {
            header_str.push_str(&format!("slot: {}\n", json_string(slot)));
        }
        if let Some(ref prec) = f.precedence {
            header_str.push_str(&format!("precedence: {}\n", prec));
        }
        header_str.push_str("evidence:\n");
        let mut out = header_str.into_bytes();
        for evidence in &f.evidence {
            out.extend_from_slice(
                format!(
                    "  - id: {}\n    start_byte: {}\n    end_byte: {}\n    content_sha256: {}\n",
                    evidence.id,
                    evidence
                        .start_byte
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "null".to_owned()),
                    evidence
                        .end_byte
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "null".to_owned()),
                    evidence.content_sha256
                )
                .as_bytes(),
            );
        }
        out.extend_from_slice(b"---\n\n");
        out.extend_from_slice(self.body.as_bytes());
        Ok(out)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, FormatError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| FormatError::new(ReasonCode::InvalidUtf8, None))?;
        let split = text
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("---\n\n"))
            .ok_or_else(|| FormatError::new(ReasonCode::InvalidSyntax, None))?;
        let (front, body) = split;
        if front.contains('\r') {
            return Err(FormatError::new(ReasonCode::NonLfContainer, None));
        }
        let mut lines = front.lines();
        if take(&mut lines, "riko_format: ")? != CLAIM_FORMAT {
            return Err(FormatError::new(
                ReasonCode::UnsupportedFormat,
                Some("riko_format"),
            ));
        }
        let id = take(&mut lines, "id: ")?.to_owned();
        let domain_id = take(&mut lines, "domain_id: ")?.to_owned();
        let kind = take(&mut lines, "kind: ")?.to_owned();
        let status = take(&mut lines, "status: ")?.to_owned();
        let version = take(&mut lines, "version: ")?
            .parse()
            .map_err(|_| FormatError::new(ReasonCode::InvalidField, Some("version")))?;
        let source_class = take(&mut lines, "source_class: ")?.to_owned();
        let claim_sha256 = take(&mut lines, "claim_sha256: ")?.to_owned();
        let normalized_claim =
            parse_json_string(take(&mut lines, "normalized_claim: ")?, "normalized_claim")?;
        let occurred_at = parse_optional(take(&mut lines, "occurred_at: ")?, "occurred_at")?;
        let valid_from = parse_optional(take(&mut lines, "valid_from: ")?, "valid_from")?;
        let valid_until = parse_optional(take(&mut lines, "valid_until: ")?, "valid_until")?;
        let origin_host_id = take(&mut lines, "origin_host_id: ")?.to_owned();
        let origin_agent_id = take(&mut lines, "origin_agent_id: ")?.to_owned();
        let created_at = take(&mut lines, "created_at: ")?.to_owned();
        let updated_at = take(&mut lines, "updated_at: ")?.to_owned();
        let reason_code = take(&mut lines, "reason_code: ")?.to_owned();
        let reason_text = parse_optional(take(&mut lines, "reason_text: ")?, "reason_text")?;
        let supersedes = parse_optional(take(&mut lines, "supersedes: ")?, "supersedes")?;
        let mut slot = None;
        let mut precedence = None;
        loop {
            let Some(line) = lines.next() else {
                return Err(FormatError::new(
                    ReasonCode::InvalidSyntax,
                    Some("evidence"),
                ));
            };
            if line == "evidence:" {
                break;
            }
            if let Some(s) = line.strip_prefix("slot: ") {
                slot = Some(parse_json_string(s, "slot")?);
            } else if let Some(p) = line.strip_prefix("precedence: ") {
                validate_control_text(p, "precedence")?;
                precedence = Some(p.to_owned());
            } else {
                return Err(FormatError::new(ReasonCode::InvalidSyntax, None));
            }
        }
        let mut evidence = Vec::new();
        while let Some(line) = lines.next() {
            let evidence_id = line
                .strip_prefix("  - id: ")
                .ok_or_else(|| FormatError::new(ReasonCode::InvalidSyntax, Some("evidence.id")))?;
            let start = take(&mut lines, "    start_byte: ")?;
            let start_byte =
                if start == "null" {
                    None
                } else {
                    Some(start.parse().map_err(|_| {
                        FormatError::new(ReasonCode::InvalidField, Some("start_byte"))
                    })?)
                };
            let end = take(&mut lines, "    end_byte: ")?;
            let end_byte =
                if end == "null" {
                    None
                } else {
                    Some(end.parse().map_err(|_| {
                        FormatError::new(ReasonCode::InvalidField, Some("end_byte"))
                    })?)
                };
            let content_sha256 = take(&mut lines, "    content_sha256: ")?.to_owned();
            evidence.push(ClaimEvidenceRef {
                id: evidence_id.to_owned(),
                start_byte,
                end_byte,
                content_sha256,
            });
        }
        let document = Self {
            front: ClaimFrontMatter {
                id,
                domain_id,
                kind,
                status,
                version,
                source_class,
                claim_sha256,
                normalized_claim,
                occurred_at,
                valid_from,
                valid_until,
                origin_host_id,
                origin_agent_id,
                created_at,
                updated_at,
                reason_code,
                reason_text,
                supersedes,
                slot,
                precedence,
                evidence,
            },
            body: body.to_owned(),
        };
        document.validate()?;
        if document.to_bytes()? != bytes {
            return Err(FormatError::new(ReasonCode::InvalidSyntax, None));
        }
        Ok(document)
    }
}
