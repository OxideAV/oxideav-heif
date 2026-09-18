//! oxideav-heif — HEIF / HEIC / MIAF image container (ISO/IEC 23008-12,
//! ISO/IEC 23000-22) for the oxideav framework.
//!
//! This crate owns the *container*: the ISOBMFF box tree, the `meta`
//! item model (items, locations, references, properties, entity
//! groups), derived images (`grid` / `iovl` / `iden`), auxiliaries
//! (alpha / depth), thumbnails, Exif / XMP / ICC metadata and the
//! `moov` image-sequence tracks. It never decodes an HEVC or AV1
//! bitstream itself — coded items are handed to `oxideav-h265` /
//! `oxideav-av1` through the registry when the default-on `registry`
//! feature is enabled.
//!
//! # Layers
//!
//! * [`boxes`] — bounds-checked ISOBMFF box reader / writer helpers.
//! * [`ftyp`] — brands and the container probe.
//! * [`meta`] — the `meta` box tree model.
//! * [`file`](mod@file) — [`HeifFile`]: whole-file parse + item payload resolution
//!   across all three `iloc` construction methods.
//!
//! The standalone build (`default-features = false`) exposes the parsed
//! structure and item bytes without any framework or codec dependency.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod boxes;
pub mod error;
pub mod file;
pub mod ftyp;
pub mod meta;

pub use error::{HeifError, Result};
pub use file::HeifFile;
pub use ftyp::{BrandClass, FileType};
pub use meta::{
    EntityGroup, Extent, ItemInfo, ItemLocation, ItemReference, Meta, PropertyAssociation,
    RawProperty,
};

/// Parse a HEIF file held in memory. Direct entry point of the
/// standalone container surface; see [`HeifFile`] for what it exposes.
pub fn parse(bytes: &[u8]) -> Result<HeifFile> {
    HeifFile::parse(bytes)
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_input_is_rejected() {
        assert!(super::parse(b"").is_err());
    }
}
