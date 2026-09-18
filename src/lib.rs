//! oxideav-heif — HEIF / HEIC / MIAF image container (ISO/IEC 23008-12,
//! ISO/IEC 23000-22) for the oxideav framework.
//!
//! Bootstrap scaffold: the item model, derivations, property surface and
//! registry integration land in the implementer rounds (see CHANGELOG).
#![forbid(unsafe_code)]

/// Crate error type (placeholder until the parser lands).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeifError {
    /// The feature is not implemented yet.
    NotImplemented(&'static str),
}

impl core::fmt::Display for HeifError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HeifError::NotImplemented(what) => write!(f, "not implemented: {what}"),
        }
    }
}

impl std::error::Error for HeifError {}

/// Parse a HEIF file. Placeholder until the container parser lands.
pub fn parse(_bytes: &[u8]) -> Result<(), HeifError> {
    Err(HeifError::NotImplemented("HEIF container parser"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn scaffold_reports_not_implemented() {
        assert!(super::parse(b"").is_err());
    }
}
