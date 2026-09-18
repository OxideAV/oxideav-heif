//! Crate error type.
//!
//! The standalone build (no `registry` feature) exposes this type as the
//! only error surface; with `registry` on it also converts into
//! [`oxideav_core::Error`] so framework callers see the familiar
//! `InvalidData` / `Unsupported` / `ResourceExhausted` variants.

use std::fmt;

/// Errors raised by the HEIF container parser, the derivation /
/// composition layer and the writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeifError {
    /// The byte stream violates ISO/IEC 14496-12 / 23008-12 syntax or
    /// semantics (truncated box, bad version, dangling reference, …).
    InvalidData(String),
    /// The file is syntactically fine but uses a feature this crate does
    /// not implement (unknown item type, unsupported construction, …).
    Unsupported(String),
    /// A structural limit was exceeded (derivation depth, canvas size,
    /// item count, …). Raised before any large allocation happens.
    ResourceExhausted(String),
}

impl HeifError {
    /// Build an [`HeifError::InvalidData`].
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidData(msg.into())
    }

    /// Build an [`HeifError::Unsupported`].
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }

    /// Build an [`HeifError::ResourceExhausted`].
    pub fn exhausted(msg: impl Into<String>) -> Self {
        Self::ResourceExhausted(msg.into())
    }
}

impl fmt::Display for HeifError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeifError::InvalidData(m) => write!(f, "heif: invalid data: {m}"),
            HeifError::Unsupported(m) => write!(f, "heif: unsupported: {m}"),
            HeifError::ResourceExhausted(m) => write!(f, "heif: resource exhausted: {m}"),
        }
    }
}

impl std::error::Error for HeifError {}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, HeifError>;

#[cfg(feature = "registry")]
impl From<HeifError> for oxideav_core::Error {
    fn from(e: HeifError) -> Self {
        match e {
            HeifError::InvalidData(m) => oxideav_core::Error::InvalidData(m),
            HeifError::Unsupported(m) => oxideav_core::Error::Unsupported(m),
            HeifError::ResourceExhausted(m) => oxideav_core::Error::ResourceExhausted(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_carries_variant_and_message() {
        assert_eq!(HeifError::invalid("x").to_string(), "heif: invalid data: x");
        assert_eq!(
            HeifError::unsupported("y").to_string(),
            "heif: unsupported: y"
        );
        assert_eq!(
            HeifError::exhausted("z").to_string(),
            "heif: resource exhausted: z"
        );
    }
}
