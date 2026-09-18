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
//! * [`file`](mod@file) — [`HeifFile`]: whole-file parse + item payload
//!   resolution across all three `iloc` construction methods.
//! * [`props`] — the typed item-property surface (`ispe`, `pixi`,
//!   `colr`, `pasp`, `clap`, `irot`, `imir`, `iscl`, `auxC`, `hvcC`,
//!   `av1C`, `clli`, `mdcv`, `cclv`, `amve`, `rloc`, `lsel`, `a1op`,
//!   `a1lx`, `rref`, `crtt`, `mdft`, `udes`, `altt`) with the §6.5.1
//!   descriptive / transformative / essential semantics.
//! * [`hvcc`] / [`av1c`] — the decoder configuration records.
//! * [`derived`] — `grid` / `iovl` / `iden` descriptors and the bounded
//!   derivation graph ([`derived::build_graph`]).
//! * [`miaf`] — MIAF constraints as typed checks ([`miaf::check`]).
//!
//! The standalone build (`default-features = false`) exposes all of the
//! above — parsed structure and item bytes — without any framework or
//! codec dependency.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod av1c;
pub mod boxes;
pub mod derived;
pub mod error;
pub mod file;
pub mod ftyp;
pub mod hvcc;
pub mod meta;
pub mod miaf;
pub mod props;

pub use av1c::Av1Config;
pub use derived::{
    build_graph, build_primary_graph, GridDescriptor, ImageKind, ImageNode, OverlayDescriptor,
};
pub use error::{HeifError, Result};
pub use file::HeifFile;
pub use ftyp::{BrandClass, FileType};
pub use hvcc::HevcConfig;
pub use meta::{
    EntityGroup, Extent, ItemInfo, ItemLocation, ItemReference, Meta, PropertyAssociation,
    RawProperty,
};
pub use miaf::{MiafProfile, MiafReport, MiafViolation};
pub use props::{
    AuxC, AuxKind, Clap, Colr, CropRect, Imir, Irot, Ispe, ItemProperties, Pasp, Pixi, Property,
    PropertyEntry,
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
