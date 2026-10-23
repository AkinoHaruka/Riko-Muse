use crate::quarantine::{FormatError, ReasonCode};
use crate::{
    CLAIM_FORMAT, EVIDENCE_FORMAT, FENCE_FORMAT, FILE_STORE_FORMAT, FILE_STORE_FORMAT_VERSION,
    OPERATION_FORMAT, OVERVIEW_FORMAT, RECEIPT_FORMAT, SOUL_FORMAT,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatDefinition {
    pub format: String,
    pub version: u32,
    pub evidence: String,
    pub claim: String,
    pub soul: String,
    pub overview: String,
    pub operation: String,
    pub receipt: String,
    pub fence: String,
}

impl Default for FormatDefinition {
    fn default() -> Self {
        Self {
            format: FILE_STORE_FORMAT.to_owned(),
            version: FILE_STORE_FORMAT_VERSION,
            evidence: EVIDENCE_FORMAT.to_owned(),
            claim: CLAIM_FORMAT.to_owned(),
            soul: SOUL_FORMAT.to_owned(),
            overview: OVERVIEW_FORMAT.to_owned(),
            operation: OPERATION_FORMAT.to_owned(),
            receipt: RECEIPT_FORMAT.to_owned(),
            fence: FENCE_FORMAT.to_owned(),
        }
    }
}

impl FormatDefinition {
    pub fn validate(&self) -> Result<(), FormatError> {
        let expected = Self::default();
        if self != &expected {
            return Err(FormatError::new(
                ReasonCode::UnsupportedFormat,
                Some("format"),
            ));
        }
        Ok(())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, FormatError> {
        let text = crate::validate_container(bytes)?;
        if !crate::is_nfc(text) {
            return Err(FormatError::new(ReasonCode::NonNfcMetadata, None));
        }
        let value: Self =
            toml::from_str(text).map_err(|_| FormatError::new(ReasonCode::InvalidSyntax, None))?;
        value.validate()?;
        if value.to_bytes()? != bytes {
            return Err(FormatError::new(ReasonCode::InvalidSyntax, None));
        }
        Ok(value)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, FormatError> {
        self.validate()?;
        Ok(format!(
            "format = \"{}\"\nversion = {}\nevidence = \"{}\"\nclaim = \"{}\"\nsoul = \"{}\"\noverview = \"{}\"\noperation = \"{}\"\nreceipt = \"{}\"\nfence = \"{}\"\n",
            self.format,
            self.version,
            self.evidence,
            self.claim,
            self.soul,
            self.overview,
            self.operation,
            self.receipt,
            self.fence
        )
        .into_bytes())
    }

    pub fn read(path: impl AsRef<Path>) -> Result<Self, FormatIoError> {
        let bytes = std::fs::read(path).map_err(FormatIoError::Io)?;
        Self::parse(&bytes).map_err(FormatIoError::Format)
    }

    pub fn write(&self, path: impl AsRef<Path>) -> Result<(), FormatIoError> {
        let bytes = self.to_bytes().map_err(FormatIoError::Format)?;
        std::fs::write(path, bytes).map_err(FormatIoError::Io)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FormatIoError {
    #[error("FORMAT.toml I/O failed: {0}")]
    Io(std::io::Error),
    #[error(transparent)]
    Format(FormatError),
}
