use crate::quarantine::{FormatError, ReasonCode};
use crate::{is_sha256, sha256_hex, validate_control_text};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const EVIDENCE_START: &str = "<!-- riko:evidence-v1 ";
pub const EVIDENCE_START_SUFFIX: &str = " -->\n";
pub const EVIDENCE_END: &str = "<!-- riko:end-evidence -->\n";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceHeader {
    pub id: String,
    pub host_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub event_seq: u64,
    pub role: String,
    pub source_kind: String,
    pub domain_id: String,
    pub occurred_at: String,
    pub content_bytes: usize,
    pub content_sha256: String,
    pub received_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceBlock {
    pub header: EvidenceHeader,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyNote {
    pub date: String,
    pub blocks: Vec<EvidenceBlock>,
}

impl EvidenceHeader {
    fn validate(&self, payload: &[u8]) -> Result<(), FormatError> {
        for (field, value) in [
            ("id", self.id.as_str()),
            ("host_id", &self.host_id),
            ("agent_id", &self.agent_id),
            ("session_id", &self.session_id),
            ("role", &self.role),
            ("source_kind", &self.source_kind),
            ("domain_id", &self.domain_id),
            ("occurred_at", &self.occurred_at),
            ("content_sha256", &self.content_sha256),
            ("received_at", &self.received_at),
        ] {
            validate_control_text(value, field)?;
            if value.is_empty() {
                return Err(FormatError::new(ReasonCode::InvalidField, Some(field)));
            }
        }
        if !matches!(self.role.as_str(), "user" | "assistant" | "tool" | "system") {
            return Err(FormatError::new(ReasonCode::InvalidField, Some("role")));
        }
        if self.content_bytes != payload.len() {
            return Err(FormatError::new(
                ReasonCode::HashMismatch,
                Some("content_bytes"),
            ));
        }
        if !is_sha256(&self.content_sha256)
            || sha256_hex(payload) != self.content_sha256.to_ascii_lowercase()
        {
            return Err(FormatError::new(
                ReasonCode::HashMismatch,
                Some("content_sha256"),
            ));
        }
        Ok(())
    }
}

impl EvidenceBlock {
    pub fn new(mut header: EvidenceHeader, payload: String) -> Self {
        header.content_bytes = payload.len();
        header.content_sha256 = sha256_hex(payload.as_bytes());
        Self { header, payload }
    }

    fn to_bytes(&self) -> Result<Vec<u8>, FormatError> {
        self.header.validate(self.payload.as_bytes())?;
        let json = serde_json::to_string(&self.header)
            .map_err(|_| FormatError::new(ReasonCode::InvalidSyntax, None))?;
        let mut out = Vec::with_capacity(json.len() + self.payload.len() + 64);
        out.extend_from_slice(EVIDENCE_START.as_bytes());
        out.extend_from_slice(json.as_bytes());
        out.extend_from_slice(EVIDENCE_START_SUFFIX.as_bytes());
        out.extend_from_slice(self.payload.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(EVIDENCE_END.as_bytes());
        Ok(out)
    }
}

impl DailyNote {
    pub fn parse(bytes: &[u8]) -> Result<Self, FormatError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| FormatError::new(ReasonCode::InvalidUtf8, None))?;
        let (title, mut offset) = text
            .find("\n\n")
            .map(|i| (&text[..i], i + 2))
            .ok_or_else(|| FormatError::new(ReasonCode::InvalidSyntax, None))?;
        let date = title
            .strip_prefix("# ")
            .ok_or_else(|| FormatError::new(ReasonCode::InvalidSyntax, Some("date")))?;
        validate_control_text(date, "date")?;
        let mut blocks = Vec::new();
        let mut ids = HashSet::new();
        while offset < bytes.len() {
            if !bytes[offset..].starts_with(EVIDENCE_START.as_bytes()) {
                return Err(FormatError::new(ReasonCode::InvalidSyntax, None));
            }
            let json_start = offset + EVIDENCE_START.len();
            let suffix_at = bytes[json_start..]
                .windows(EVIDENCE_START_SUFFIX.len())
                .position(|w| w == EVIDENCE_START_SUFFIX.as_bytes())
                .map(|i| json_start + i)
                .ok_or_else(|| FormatError::new(ReasonCode::InvalidSyntax, None))?;
            let json = std::str::from_utf8(&bytes[json_start..suffix_at])
                .map_err(|_| FormatError::new(ReasonCode::InvalidUtf8, None))?;
            validate_control_text(json, "evidence_header")?;
            let header: EvidenceHeader = serde_json::from_str(json).map_err(|_| {
                FormatError::new(ReasonCode::InvalidSyntax, Some("evidence_header"))
            })?;
            let payload_start = suffix_at + EVIDENCE_START_SUFFIX.len();
            let payload_end = payload_start
                .checked_add(header.content_bytes)
                .ok_or_else(|| FormatError::new(ReasonCode::InvalidField, Some("content_bytes")))?;
            if payload_end > bytes.len() {
                return Err(FormatError::new(
                    ReasonCode::HashMismatch,
                    Some("content_bytes"),
                ));
            }
            let payload_bytes = &bytes[payload_start..payload_end];
            let payload = std::str::from_utf8(payload_bytes)
                .map_err(|_| FormatError::new(ReasonCode::InvalidUtf8, Some("payload")))?;
            header.validate(payload_bytes)?;
            let marker_end = payload_end + 1 + EVIDENCE_END.len();
            if bytes.get(payload_end) != Some(&b'\n')
                || bytes.get(payload_end + 1..marker_end) != Some(EVIDENCE_END.as_bytes())
            {
                return Err(FormatError::new(ReasonCode::InvalidSyntax, None));
            }
            if !ids.insert(header.id.clone()) {
                return Err(FormatError::new(ReasonCode::DuplicateId, Some("id")));
            }
            blocks.push(EvidenceBlock {
                header,
                payload: payload.to_owned(),
            });
            offset = marker_end;
            if offset < bytes.len() {
                if bytes.get(offset..offset + 1) != Some(b"\n") {
                    return Err(FormatError::new(ReasonCode::InvalidSyntax, None));
                }
                offset += 1;
            }
        }
        let note = Self {
            date: date.to_owned(),
            blocks,
        };
        if note.to_bytes()? != bytes {
            return Err(FormatError::new(ReasonCode::InvalidSyntax, None));
        }
        Ok(note)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, FormatError> {
        validate_control_text(&self.date, "date")?;
        let mut seen = HashSet::new();
        let mut out = format!("# {}\n\n", self.date).into_bytes();
        for (index, block) in self.blocks.iter().enumerate() {
            if !seen.insert(block.header.id.as_str()) {
                return Err(FormatError::new(ReasonCode::DuplicateId, Some("id")));
            }
            if index > 0 {
                out.push(b'\n');
            }
            out.extend_from_slice(&block.to_bytes()?);
        }
        Ok(out)
    }
}
