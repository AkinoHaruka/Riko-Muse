use crate::quarantine::{FormatError, ReasonCode};
use crate::{is_sha256, sha256_hex, validate_control_text, SOUL_FORMAT};

pub const SOUL_BODY_MAX_SCALARS: usize = 2000;
pub const SOUL_ACTOR_KINDS: [&str; 3] = ["user_cli", "user_api", "admin_cli"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoulFrontMatter {
    pub agent_id: String,
    pub version: u64,
    pub body_sha256: String,
    pub updated_at: String,
    pub actor_kind: String,
    pub origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoulDocument {
    pub front: SoulFrontMatter,
    /// Original UTF-8 body. Never normalized.
    pub body: String,
}

impl SoulDocument {
    pub fn validate(&self) -> Result<(), FormatError> {
        for (field, value) in [
            ("agent_id", self.front.agent_id.as_str()),
            ("body_sha256", &self.front.body_sha256),
            ("updated_at", &self.front.updated_at),
            ("actor_kind", &self.front.actor_kind),
        ] {
            validate_control_text(value, field)?;
            if value.is_empty() {
                return Err(FormatError::new(ReasonCode::InvalidField, Some(field)));
            }
        }
        if self.front.version == 0 {
            return Err(FormatError::new(ReasonCode::InvalidField, Some("version")));
        }
        if !SOUL_ACTOR_KINDS.contains(&self.front.actor_kind.as_str()) {
            return Err(FormatError::new(
                ReasonCode::InvalidField,
                Some("actor_kind"),
            ));
        }
        if let Some(ref origin) = self.front.origin {
            validate_control_text(origin, "origin")?;
            if origin.is_empty() {
                return Err(FormatError::new(ReasonCode::InvalidField, Some("origin")));
            }
        }
        if self.body.chars().count() > SOUL_BODY_MAX_SCALARS {
            return Err(FormatError::new(ReasonCode::InvalidField, Some("body")));
        }
        if !is_sha256(&self.front.body_sha256)
            || sha256_hex(self.body.as_bytes()) != self.front.body_sha256.to_ascii_lowercase()
        {
            return Err(FormatError::new(
                ReasonCode::HashMismatch,
                Some("body_sha256"),
            ));
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, FormatError> {
        self.validate()?;
        let origin_line = match &self.front.origin {
            Some(o) => format!("origin: {o}\n"),
            None => String::new(),
        };
        let mut out = format!(
            "---\nriko_format: {SOUL_FORMAT}\nagent_id: {}\nversion: {}\nbody_sha256: {}\nupdated_at: {}\nactor_kind: {}\n{origin_line}---\n\n",
            self.front.agent_id,
            self.front.version,
            self.front.body_sha256,
            self.front.updated_at,
            self.front.actor_kind,
        )
        .into_bytes();
        out.extend_from_slice(self.body.as_bytes());
        Ok(out)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, FormatError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| FormatError::new(ReasonCode::InvalidUtf8, None))?;
        let (front, body) = text
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("---\n\n"))
            .ok_or_else(|| FormatError::new(ReasonCode::InvalidSyntax, None))?;
        if front.contains('\r') {
            return Err(FormatError::new(ReasonCode::NonLfContainer, None));
        }
        let mut lines = front.lines();
        let take = |lines: &mut std::str::Lines<'_>, prefix: &'static str| {
            lines
                .next()
                .and_then(|line| line.strip_prefix(prefix))
                .map(str::to_owned)
                .ok_or_else(|| {
                    FormatError::new(
                        ReasonCode::InvalidSyntax,
                        Some(prefix.trim_end_matches(": ")),
                    )
                })
        };
        if take(&mut lines, "riko_format: ")? != SOUL_FORMAT {
            return Err(FormatError::new(
                ReasonCode::UnsupportedFormat,
                Some("riko_format"),
            ));
        }
        let agent_id = take(&mut lines, "agent_id: ")?;
        let version = take(&mut lines, "version: ")?
            .parse()
            .map_err(|_| FormatError::new(ReasonCode::InvalidField, Some("version")))?;
        let body_sha256 = take(&mut lines, "body_sha256: ")?;
        let updated_at = take(&mut lines, "updated_at: ")?;
        let actor_kind = take(&mut lines, "actor_kind: ")?;
        let origin = if let Some(line) = lines.clone().next() {
            if let Some(val) = line.strip_prefix("origin: ") {
                lines.next();
                Some(val.to_owned())
            } else {
                None
            }
        } else {
            None
        };
        if lines.next().is_some() {
            return Err(FormatError::new(ReasonCode::UnexpectedField, None));
        }
        let document = Self {
            front: SoulFrontMatter {
                agent_id,
                version,
                body_sha256,
                updated_at,
                actor_kind,
                origin,
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
