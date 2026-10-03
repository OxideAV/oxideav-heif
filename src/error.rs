//! Crate error type.
//!
//! The standalone build (no `registry` feature) exposes this type as the
//! only error surface; with `registry` on it also converts into
//! [`oxideav_core::Error`] so framework callers see the familiar
//! `InvalidData` / `Unsupported` / `ResourceExhausted` variants.
//!
//! The variant set follows the workspace image-crate contract:
//! `InvalidData`, `Unsupported`, `LimitExceeded`, `Io` (plus the typed
//! helpers below). [`Error`] is the contract's alias of [`HeifError`].

use std::fmt;

/// Errors raised by the HEIF container parser, the derivation /
/// composition layer and the writer.
#[derive(Debug)]
#[non_exhaustive]
pub enum HeifError {
    /// The byte stream violates ISO/IEC 14496-12 / 23008-12 syntax or
    /// semantics (truncated box, bad version, dangling reference, …).
    InvalidData(String),
    /// The file is syntactically fine but uses a feature this crate does
    /// not implement (unknown item type, unsupported construction, …).
    Unsupported(String),
    /// A structural limit was exceeded (derivation depth, canvas size,
    /// item count, a [`DecodeOptions`](crate::DecodeOptions) bound, …).
    /// Raised before any large allocation happens.
    LimitExceeded(String),
    /// An I/O error from a [`Read`](std::io::Read) / [`Write`](std::io::Write)
    /// handed to [`decode_from`](crate::decode_from) /
    /// [`encode_to`](crate::encode_to).
    Io(std::io::Error),
    /// The pre-0.0.9 name of [`HeifError::LimitExceeded`]. No longer
    /// produced; kept one release so existing `match` arms compile.
    #[deprecated(
        since = "0.0.9",
        note = "limit refusals are `HeifError::LimitExceeded`"
    )]
    ResourceExhausted(String),
}

/// The contract's alias of [`HeifError`].
pub type Error = HeifError;

impl HeifError {
    /// Build an [`HeifError::InvalidData`].
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidData(msg.into())
    }

    /// Build an [`HeifError::Unsupported`].
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }

    /// Build an [`HeifError::LimitExceeded`].
    pub fn exhausted(msg: impl Into<String>) -> Self {
        Self::LimitExceeded(msg.into())
    }

    /// Build an [`HeifError::LimitExceeded`] (the contract's name of
    /// [`HeifError::exhausted`]).
    pub fn limit(msg: impl Into<String>) -> Self {
        Self::LimitExceeded(msg.into())
    }

    /// Build an [`HeifError::Io`] from a message (`ErrorKind::Other`).
    pub fn io(msg: impl Into<String>) -> Self {
        Self::Io(std::io::Error::other(msg.into()))
    }

    /// The refusal for an L-HEVC (`lhv1`) item whose `tols` asks for
    /// an output layer set beyond the base layer (this crate decodes
    /// the base layer only; opt in with
    /// `ItemDecoder::base_layer_fallback`). An [`HeifError::Unsupported`]
    /// whose message [`HeifError::layered_hevc_info`] reads back.
    pub fn layered_hevc(item_id: u32, target_ols_idx: u16) -> Self {
        Self::Unsupported(format!(
            "{LAYERED_HEVC_PREFIX}item {item_id} output layer set {target_ols_idx} needs enhancement layers (base layer only)"
        ))
    }

    /// `(item_id, target_ols_idx)` when this is the
    /// [`HeifError::layered_hevc`] refusal.
    pub fn layered_hevc_info(&self) -> Option<(u32, u16)> {
        let HeifError::Unsupported(m) = self else {
            return None;
        };
        let rest = m.strip_prefix(LAYERED_HEVC_PREFIX)?;
        let mut words = rest.split_whitespace();
        let item_id = words.nth(1)?.parse().ok()?;
        let tols = words.nth(3)?.parse().ok()?;
        Some((item_id, tols))
    }
}

/// Message prefix of [`HeifError::layered_hevc`].
const LAYERED_HEVC_PREFIX: &str = "L-HEVC: ";

impl fmt::Display for HeifError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeifError::InvalidData(m) => write!(f, "heif: invalid data: {m}"),
            HeifError::Unsupported(m) => write!(f, "heif: unsupported: {m}"),
            HeifError::LimitExceeded(m) => write!(f, "heif: limit exceeded: {m}"),
            HeifError::Io(m) => write!(f, "heif: i/o: {m}"),
            #[allow(deprecated)]
            HeifError::ResourceExhausted(m) => write!(f, "heif: resource exhausted: {m}"),
        }
    }
}

impl std::error::Error for HeifError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HeifError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for HeifError {
    fn from(e: std::io::Error) -> Self {
        HeifError::Io(e)
    }
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, HeifError>;

#[cfg(feature = "registry")]
impl From<HeifError> for oxideav_core::Error {
    fn from(e: HeifError) -> Self {
        match e {
            HeifError::InvalidData(m) => oxideav_core::Error::InvalidData(m),
            HeifError::Unsupported(m) => oxideav_core::Error::Unsupported(m),
            HeifError::LimitExceeded(m) => oxideav_core::Error::ResourceExhausted(m),
            HeifError::Io(e) => oxideav_core::Error::Io(e),
            #[allow(deprecated)]
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
            "heif: limit exceeded: z"
        );
        assert!(matches!(HeifError::limit("z"), HeifError::LimitExceeded(_)));
        let io: HeifError = std::io::Error::other("disk").into();
        assert_eq!(io.to_string(), "heif: i/o: disk");
        assert!(std::error::Error::source(&io).is_some());
        assert!(matches!(HeifError::io("x"), HeifError::Io(_)));
        let e = HeifError::layered_hevc(7, 2);
        assert!(matches!(e, HeifError::Unsupported(_)));
        assert_eq!(e.layered_hevc_info(), Some((7, 2)));
        assert_eq!(HeifError::unsupported("x").layered_hevc_info(), None);
    }
}
