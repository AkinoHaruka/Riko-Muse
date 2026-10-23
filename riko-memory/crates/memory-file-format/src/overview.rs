use crate::quarantine::{FormatError, ReasonCode};
use crate::{validate_control_text, OVERVIEW_FORMAT};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverviewFrontMatter {
    pub domain_id: String,
    pub source_generation: String,
    pub generated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratedRef {
    pub memory_id: String,
    pub version: u64,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedBlock {
    pub generator: String,
    pub sources: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryOverview {
    pub front: OverviewFrontMatter,
    pub curated: Vec<CuratedRef>,
    pub derived: Vec<DerivedBlock>,
}

impl MemoryOverview {
    /// Parse annotations only. Rendering is intentionally outside F1.
    pub fn parse(bytes: &[u8]) -> Result<Self, FormatError> {
        let text = crate::validate_container(bytes)?;
        let (front, body) = text
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("---\n\n"))
            .ok_or_else(|| FormatError::new(ReasonCode::InvalidSyntax, None))?;
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
        if take(&mut lines, "riko_format: ")? != OVERVIEW_FORMAT {
            return Err(FormatError::new(
                ReasonCode::UnsupportedFormat,
                Some("riko_format"),
            ));
        }
        let domain_id = take(&mut lines, "domain_id: ")?;
        let source_generation = take(&mut lines, "source_generation: ")?;
        let generated_at = take(&mut lines, "generated_at: ")?;
        if lines.next().is_some() {
            return Err(FormatError::new(ReasonCode::UnexpectedField, None));
        }
        for (field, value) in [
            ("domain_id", domain_id.as_str()),
            ("source_generation", source_generation.as_str()),
            ("generated_at", generated_at.as_str()),
        ] {
            validate_control_text(value, field)?;
            if value.is_empty() {
                return Err(FormatError::new(ReasonCode::InvalidField, Some(field)));
            }
        }

        let mut curated = Vec::new();
        let mut derived = Vec::new();
        let mut ids = HashSet::new();
        for (index, line) in body.lines().enumerate() {
            let line_number = index + front.lines().count() + 4;
            if let Some(inner) = line
                .strip_prefix("<!-- riko:ref ")
                .and_then(|v| v.strip_suffix(" -->"))
            {
                let mut parts = inner.split(' ');
                let memory_id = parts
                    .next()
                    .and_then(|v| v.strip_prefix("memory="))
                    .ok_or_else(|| FormatError::new(ReasonCode::InvalidSyntax, Some("memory")))?;
                let version: u64 = parts
                    .next()
                    .and_then(|v| v.strip_prefix("version="))
                    .ok_or_else(|| FormatError::new(ReasonCode::InvalidSyntax, Some("version")))?
                    .parse()
                    .map_err(|_| FormatError::new(ReasonCode::InvalidField, Some("version")))?;
                if parts.next().is_some() || memory_id.is_empty() || version == 0 {
                    return Err(FormatError::new(
                        ReasonCode::InvalidSyntax,
                        Some("riko:ref"),
                    ));
                }
                validate_control_text(memory_id, "memory")?;
                if !ids.insert(memory_id.to_owned()) {
                    return Err(FormatError::new(ReasonCode::DuplicateId, Some("memory")));
                }
                curated.push(CuratedRef {
                    memory_id: memory_id.to_owned(),
                    version,
                    line: line_number,
                });
            } else if let Some(inner) = line
                .strip_prefix("<!-- riko:derived ")
                .and_then(|v| v.strip_suffix(" -->"))
            {
                let (generator, sources_json) = inner
                    .strip_prefix("generator=")
                    .and_then(|v| v.split_once(" sources="))
                    .ok_or_else(|| {
                        FormatError::new(ReasonCode::InvalidSyntax, Some("riko:derived"))
                    })?;
                let sources: String = serde_json::from_str(sources_json)
                    .map_err(|_| FormatError::new(ReasonCode::InvalidField, Some("sources")))?;
                validate_control_text(generator, "generator")?;
                validate_control_text(&sources, "sources")?;
                if generator.is_empty() || sources.is_empty() {
                    return Err(FormatError::new(
                        ReasonCode::InvalidField,
                        Some("riko:derived"),
                    ));
                }
                derived.push(DerivedBlock {
                    generator: generator.to_owned(),
                    sources,
                    line: line_number,
                });
            }
        }
        Ok(Self {
            front: OverviewFrontMatter {
                domain_id,
                source_generation,
                generated_at,
            },
            curated,
            derived,
        })
    }
}
