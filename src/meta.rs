//! The file-level `meta` box tree (ISO/IEC 14496-12 §8.11, ISO/IEC
//! 23008-12 §6.2 / §9): handler, primary item, item info, item
//! locations, item references, item properties, item data, entity
//! groups and data references.
//!
//! ```text
//! meta (FullBox v=0)
//!   hdlr            handler_type ('pict' for HEIF)
//!   pitm            primary item id (v0: u16, v1: u32)
//!   dinf/dref       data references (self-contained flag)
//!   iloc            item locations (v0/v1/v2, construction methods 0/1/2)
//!   iinf            item info (v0: u16 count, v1: u32 count)
//!     infe          item info entries (v2: u16 id, v3: u32 id)
//!   iref            item references (v0: u16 ids, v1: u32 ids)
//!   iprp            item properties
//!     ipco          property container (1-based implicit indices)
//!     ipma          item → property associations (v0/v1, 7/15-bit index)
//!   idat            inline item data (construction method 1)
//!   grpl            entity groups
//!   ipro            item protection (surfaced as a count only)
//! ```
//!
//! Every property is kept as its raw box (type + body bytes) so unknown
//! and vendor properties survive round trips; the typed view lives in
//! the `props` module.

use crate::boxes::{
    FourCc, Reader, fourcc_str, iter_boxes, parse_box_header, parse_full_box, payload,
};
use crate::error::{HeifError, Result};

/// Upper bound on the number of items a `meta` box may declare.
pub const MAX_ITEMS: usize = 1 << 20;
/// Upper bound on the number of properties in `ipco`.
pub const MAX_PROPERTIES: usize = 1 << 15;
/// Upper bound on the number of `iref` boxes.
pub const MAX_REFERENCES: usize = 1 << 20;
/// Upper bound on the number of entity groups.
pub const MAX_ENTITY_GROUPS: usize = 1 << 16;

/// Item type of HEVC image items (HEIF Annex B.2.2.1.2).
pub const ITEM_TYPE_HVC1: FourCc = *b"hvc1";
/// Item type of HEVC image items using `hev1` sample-entry semantics
/// (parameter sets may ride in band); treated like `hvc1`.
pub const ITEM_TYPE_HEV1: FourCc = *b"hev1";
/// Item type of HEVC tile items (Annex B.2.5).
pub const ITEM_TYPE_HVT1: FourCc = *b"hvt1";
/// Item type of layered HEVC image items (Annex B.2.2.1.3).
pub const ITEM_TYPE_LHV1: FourCc = *b"lhv1";
/// Item type of AV1 image items (AVIF §2.1).
pub const ITEM_TYPE_AV01: FourCc = *b"av01";
/// Item type of AVC image items (Annex E).
pub const ITEM_TYPE_AVC1: FourCc = *b"avc1";
/// Item type of JPEG image items (Annex H).
pub const ITEM_TYPE_JPEG: FourCc = *b"jpeg";
/// Item type of VVC image items (Annex L.2.2.1.2).
pub const ITEM_TYPE_VVC1: FourCc = *b"vvc1";
/// Item type of VVC subpicture items that are not decodable on their
/// own (Annex L.2.4.1); referenced by a VVC base item through `subp`.
pub const ITEM_TYPE_VVS1: FourCc = *b"vvs1";
/// Grid derived image item (§6.6.2.3).
pub const ITEM_TYPE_GRID: FourCc = *b"grid";
/// Overlay derived image item (§6.6.2.2).
pub const ITEM_TYPE_IOVL: FourCc = *b"iovl";
/// Identity derived image item (§6.6.2.1).
pub const ITEM_TYPE_IDEN: FourCc = *b"iden";
/// Tone-map derived image item (ISO/IEC 21496-1 gain maps).
pub const ITEM_TYPE_TMAP: FourCc = *b"tmap";
/// Tiled image item (HEIF Amd 2:2026 §6.11).
pub const ITEM_TYPE_TILI: FourCc = *b"tili";
/// Colour format enhancement derived image item (HEIF Amd 1:2025
/// §6.6.2.5).
pub const ITEM_TYPE_CFEN: FourCc = *b"cfen";
/// Pixel transform derived image item (HEIF 4th edition WD
/// §6.6.2.6).
pub const ITEM_TYPE_SATO: FourCc = *b"sato";
/// Exif metadata item (Annex A.2).
pub const ITEM_TYPE_EXIF: FourCc = *b"Exif";
/// Deflate-compressed Exif metadata item (HEIF Amd 2:2026 A.2.1).
pub const ITEM_TYPE_DEXF: FourCc = *b"dExf";
/// MIME-typed item (XMP = `application/rdf+xml`, Annex A.3).
pub const ITEM_TYPE_MIME: FourCc = *b"mime";
/// URI-typed item (IPTC, Annex A.5).
pub const ITEM_TYPE_URI: FourCc = *b"uri ";
/// Region item (HEIF §6.10).
pub const ITEM_TYPE_RGAN: FourCc = *b"rgan";

/// Strip the `exif_tiff_header_offset` word of an `Exif` item body or
/// an Exif metadata-track sample (HEIF Annex A.2.1 `ExifDataBlock`) and
/// return the bytes from the TIFF header on. A body that starts with
/// the TIFF header itself (`MM\0*` / `II*\0` where the offset word
/// should be — the conformance file C034 is written that way, its
/// item exactly the staged Exif blob) is taken as a block without the
/// word: that bit pattern, read as an offset, could only point past
/// any item.
pub fn exif_payload(item: &[u8]) -> Result<Vec<u8>> {
    if item.len() < 4 {
        return Err(HeifError::invalid("Exif item shorter than its offset word"));
    }
    if item.starts_with(b"MM\0*") || item.starts_with(b"II*\0") {
        return Ok(item.to_vec());
    }
    let off = u32::from_be_bytes([item[0], item[1], item[2], item[3]]) as usize;
    let start = 4usize
        .checked_add(off)
        .filter(|s| *s <= item.len())
        .ok_or_else(|| {
            HeifError::invalid(format!("Exif tiff header offset {off} past the item"))
        })?;
    Ok(item[start..].to_vec())
}

/// An `ExifDataBlock` (Annex A.2.1) over a payload that starts at its
/// TIFF header: the zero offset word then the payload.
pub fn exif_data_block(tiff: &[u8]) -> Vec<u8> {
    let mut body = 0u32.to_be_bytes().to_vec();
    body.extend_from_slice(tiff);
    body
}

/// Inflate a `deflate`-encoded metadata item (RFC 1951; `dExf` items
/// and `content_encoding = "deflate"` XMP, HEIF Amd 2:2026 A.2.1 /
/// O.4.3), capped at [`crate::file::MAX_ITEM_BYTES`]. `None` when the
/// crate is built without the `deflate` feature.
pub fn inflate_metadata(body: &[u8]) -> Result<Option<Vec<u8>>> {
    #[cfg(feature = "deflate")]
    {
        compcol::vec::decompress_to_vec_capped::<compcol::deflate::Deflate>(
            body,
            crate::file::MAX_ITEM_BYTES,
        )
        .map(Some)
        .map_err(|e| HeifError::invalid(format!("deflate metadata item: {e}")))
    }
    #[cfg(not(feature = "deflate"))]
    {
        let _ = body;
        Ok(None)
    }
}

/// The Exif payload (from the TIFF header on) and the XMP packet among
/// the items of `meta` listed in `ids` — the first `Exif` / `dExf` item
/// and the first XMP `mime` item, deflated ones inflated. `file`
/// resolves the item bodies (a track-level `meta` resolves against the
/// same file bytes). An unreadable item is skipped.
pub fn metadata_items<D: AsRef<[u8]>>(
    file: &crate::file::HeifFile<D>,
    meta: &Meta,
    ids: impl IntoIterator<Item = u32>,
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut exif = None;
    let mut xmp = None;
    for id in ids {
        let Some(m) = meta.item(id) else {
            continue;
        };
        let Ok(body) = file.item_data_in(meta, id) else {
            continue;
        };
        if m.item_type == ITEM_TYPE_EXIF && exif.is_none() {
            exif = exif_payload(&body).ok();
        } else if m.item_type == ITEM_TYPE_DEXF && exif.is_none() {
            if let Ok(Some(block)) = inflate_metadata(&body) {
                exif = exif_payload(&block).ok();
            }
        } else if m.is_xmp() && xmp.is_none() {
            let deflated = m
                .content_encoding
                .as_deref()
                .map(|e| e.eq_ignore_ascii_case("deflate"))
                .unwrap_or(false);
            if deflated {
                if let Ok(Some(packet)) = inflate_metadata(&body) {
                    xmp = Some(packet);
                }
            } else {
                xmp = Some(body.into_owned());
            }
        }
    }
    (exif, xmp)
}

/// Item reference types (HEIF §6 / §7.4.5 of the 2017 edition).
pub mod reference {
    use super::FourCc;
    /// Derived image → input images.
    pub const DIMG: FourCc = *b"dimg";
    /// Thumbnail → master.
    pub const THMB: FourCc = *b"thmb";
    /// Auxiliary → master.
    pub const AUXL: FourCc = *b"auxl";
    /// Metadata item → described image.
    pub const CDSC: FourCc = *b"cdsc";
    /// Pre-derived coded image → source images.
    pub const BASE: FourCc = *b"base";
    /// Master → auxiliary: master is pre-multiplied by alpha.
    pub const PREM: FourCc = *b"prem";
    /// Tile item → base image (with `rloc`).
    pub const TBAS: FourCc = *b"tbas";
    /// Dependent slice tile → independent slice tile.
    pub const DPND: FourCc = *b"dpnd";
    /// Predictively coded item → reference items.
    pub const PRED: FourCc = *b"pred";
    /// Scalable item → external base layer.
    pub const EXBL: FourCc = *b"exbl";
    /// Item whose data is addressed by construction method 2.
    pub const ILOC: FourCc = *b"iloc";
    /// Region item → image item.
    pub const CDSC_REGION: FourCc = *b"cdsc";
    /// Mask item → region.
    pub const MASK: FourCc = *b"mask";
    /// Text item → image.
    pub const TEXT: FourCc = *b"text";
    /// Text item → font item.
    pub const FONT: FourCc = *b"font";
    /// Event item → image.
    pub const EVNT: FourCc = *b"evnt";
}

/// `hdlr` box contents.
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct Handler {
    /// `handler_type` (`pict` for HEIF image collections).
    pub handler_type: FourCc,
    /// Human-readable `name` (may be empty).
    pub name: String,
}
impl Handler {
    /// Every field as a positional argument, in declaration order.
    pub fn new(handler_type: FourCc, name: String) -> Self {
        Self { handler_type, name }
    }
}

/// One `infe` entry (ISO/IEC 14496-12 §8.11.6).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ItemInfo {
    /// `item_ID`.
    pub id: u32,
    /// `infe` box version (2 or 3 are the HEIF-legal ones).
    pub version: u8,
    /// `infe` flags; bit 0 = hidden item (HEIF §6.4.2).
    pub flags: u32,
    /// `item_protection_index` (0 = unprotected).
    pub protection_index: u16,
    /// `item_type`.
    pub item_type: FourCc,
    /// `item_name`.
    pub name: String,
    /// `content_type` for `mime` items.
    pub content_type: Option<String>,
    /// `content_encoding` for `mime` items (empty string collapsed to `None`).
    pub content_encoding: Option<String>,
    /// `item_uri_type` for `uri ` items.
    pub item_uri_type: Option<String>,
}
impl ItemInfo {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u32,
        version: u8,
        flags: u32,
        protection_index: u16,
        item_type: FourCc,
        name: String,
        content_type: Option<String>,
        content_encoding: Option<String>,
        item_uri_type: Option<String>,
    ) -> Self {
        Self {
            id,
            version,
            flags,
            protection_index,
            item_type,
            name,
            content_type,
            content_encoding,
            item_uri_type,
        }
    }
}

impl ItemInfo {
    /// HEIF §6.4.2: `(flags & 1) == 1` marks a hidden image item.
    pub fn is_hidden(&self) -> bool {
        self.flags & 1 == 1
    }

    /// `true` for the derived image item types this crate knows, and
    /// for tiled image items (`tili`, Amd 2:2026 §6.11 — composed from
    /// tiles like a grid, without input items).
    pub fn is_derived_image(&self) -> bool {
        matches!(
            self.item_type,
            ITEM_TYPE_GRID
                | ITEM_TYPE_IOVL
                | ITEM_TYPE_IDEN
                | ITEM_TYPE_TMAP
                | ITEM_TYPE_TILI
                | ITEM_TYPE_CFEN
                | ITEM_TYPE_SATO
        )
    }

    /// `true` for a tiled image item (`tili`).
    pub fn is_tiled_image(&self) -> bool {
        self.item_type == ITEM_TYPE_TILI
    }

    /// `true` for the coded image item types this crate knows.
    pub fn is_coded_image(&self) -> bool {
        matches!(
            self.item_type,
            ITEM_TYPE_HVC1
                | ITEM_TYPE_HEV1
                | ITEM_TYPE_HVT1
                | ITEM_TYPE_LHV1
                | ITEM_TYPE_AV01
                | ITEM_TYPE_AVC1
                | ITEM_TYPE_JPEG
                | ITEM_TYPE_VVC1
        )
    }

    /// `true` for coded or derived image items.
    pub fn is_image(&self) -> bool {
        self.is_coded_image() || self.is_derived_image()
    }

    /// `true` when this is an XMP packet (`mime` + `application/rdf+xml`).
    pub fn is_xmp(&self) -> bool {
        self.item_type == ITEM_TYPE_MIME
            && self
                .content_type
                .as_deref()
                .map(|c| c.eq_ignore_ascii_case("application/rdf+xml"))
                .unwrap_or(false)
    }
}

/// One `iloc` extent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[doc(hidden)]
pub struct Extent {
    /// `extent_index` (only meaningful for construction method 2; 0
    /// when the box carried `index_size == 0`, which implies 1).
    pub index: u64,
    /// `extent_offset` relative to the data origin.
    pub offset: u64,
    /// `extent_length`; 0 means "to the end of the source".
    pub length: u64,
}
impl Extent {
    /// Every field as a positional argument, in declaration order.
    pub fn new(index: u64, offset: u64, length: u64) -> Self {
        Self {
            index,
            offset,
            length,
        }
    }
}

/// One `iloc` item record.
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct ItemLocation {
    /// `item_ID`.
    pub item_id: u32,
    /// `construction_method`: 0 = file offset, 1 = `idat` offset, 2 = item offset.
    pub construction_method: u8,
    /// `data_reference_index` (0 = this file).
    pub data_reference_index: u16,
    /// `base_offset` added to every extent offset.
    pub base_offset: u64,
    /// The extents, concatenated in order to form the item data.
    pub extents: Vec<Extent>,
}
impl ItemLocation {
    /// Every field as a positional argument, in declaration order.
    pub fn new(
        item_id: u32,
        construction_method: u8,
        data_reference_index: u16,
        base_offset: u64,
        extents: Vec<Extent>,
    ) -> Self {
        Self {
            item_id,
            construction_method,
            data_reference_index,
            base_offset,
            extents,
        }
    }
}

impl ItemLocation {
    /// Sum of the explicit extent lengths (0-length extents excluded).
    pub fn declared_size(&self) -> u64 {
        self.extents
            .iter()
            .fold(0u64, |acc, e| acc.saturating_add(e.length))
    }
}

/// One property box from `ipco`, kept raw.
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct RawProperty {
    /// Box type.
    pub box_type: FourCc,
    /// Extended type for `uuid` properties.
    pub user_type: Option<[u8; 16]>,
    /// Box body (header excluded). For FullBox-derived properties the
    /// version/flags bytes are part of the body.
    pub body: Vec<u8>,
    /// Total box size in bytes (header included), as found in the file.
    pub box_size: usize,
}
impl RawProperty {
    /// Every field as a positional argument, in declaration order.
    pub fn new(
        box_type: FourCc,
        user_type: Option<[u8; 16]>,
        body: Vec<u8>,
        box_size: usize,
    ) -> Self {
        Self {
            box_type,
            user_type,
            body,
            box_size,
        }
    }
}

/// One entry of an `ipma` association list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct PropertyAssociation {
    /// 1-based index into `ipco`.
    pub index: u16,
    /// `essential` flag.
    pub essential: bool,
}
impl PropertyAssociation {
    /// Every field as a positional argument, in declaration order.
    pub fn new(index: u16, essential: bool) -> Self {
        Self { index, essential }
    }
}

/// The association list of one item.
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct ItemPropertyAssociations {
    /// `item_ID`.
    pub item_id: u32,
    /// Associations in file order. Index-0 placeholders ("no property")
    /// are dropped at parse time.
    pub entries: Vec<PropertyAssociation>,
}
impl ItemPropertyAssociations {
    /// Every field as a positional argument, in declaration order.
    pub fn new(item_id: u32, entries: Vec<PropertyAssociation>) -> Self {
        Self { item_id, entries }
    }
}

/// One `SingleItemTypeReferenceBox` from `iref`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct ItemReference {
    /// The reference type (box type of the child box).
    pub reference_type: FourCc,
    /// `from_item_ID`.
    pub from_item_id: u32,
    /// `to_item_ID[]` in order.
    pub to_item_ids: Vec<u32>,
}
impl ItemReference {
    /// Every field as a positional argument, in declaration order.
    pub fn new(reference_type: FourCc, from_item_id: u32, to_item_ids: Vec<u32>) -> Self {
        Self {
            reference_type,
            from_item_id,
            to_item_ids,
        }
    }
}

/// One `EntityToGroupBox` from `grpl`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EntityGroup {
    /// `grouping_type` (`altr`, `brst`, `eqiv`, `ster`, …).
    pub grouping_type: FourCc,
    /// `EntityToGroupBox` version (0 in ISO/IEC 14496-12 §8.18.3).
    pub version: u8,
    /// `EntityToGroupBox` flags (24 bits; meaning per grouping type).
    pub flags: u32,
    /// `group_id`.
    pub group_id: u32,
    /// `entity_id[]` (item ids or track ids).
    pub entity_ids: Vec<u32>,
    /// The bytes after `entity_id[]` — the grouping-type-specific
    /// extension (`pymd` tile sizes, `rgpa` area, …); empty for the
    /// plain groups.
    pub payload: Vec<u8>,
}
impl EntityGroup {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(
        grouping_type: FourCc,
        version: u8,
        flags: u32,
        group_id: u32,
        entity_ids: Vec<u32>,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            grouping_type,
            version,
            flags,
            group_id,
            entity_ids,
            payload,
        }
    }
}

/// `pymd` image pyramid tile information (HEIF Amd 1:2025 §6.8.12).
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct PyramidInfo {
    /// `(tile_size_x, tile_size_y)`: one entry for every layer (lowest
    /// resolution first, the base image last) when
    /// `tile_info_constant_flag` is clear, a single entry when set,
    /// empty when `tile_info_present_flag` is clear (derive from
    /// `cexg` / `grid` / `uncC` / `tilC`, Tables 3–5 + 15).
    pub tile_sizes: Vec<(u32, u32)>,
}
impl PyramidInfo {
    /// Every field as a positional argument, in declaration order.
    pub fn new(tile_sizes: Vec<(u32, u32)>) -> Self {
        Self { tile_sizes }
    }
}

impl PyramidInfo {
    /// `tile_info_present_flag`.
    pub const FLAG_TILE_INFO_PRESENT: u32 = 1;
    /// `tile_info_constant_flag`.
    pub const FLAG_TILE_INFO_CONSTANT: u32 = 2;

    /// The tile size of layer `i` (0 = lowest-resolution overview),
    /// when signalled.
    pub fn tile_size(&self, layer: usize) -> Option<(u32, u32)> {
        match self.tile_sizes.len() {
            0 => None,
            1 => Some(self.tile_sizes[0]),
            _ => self.tile_sizes.get(layer).copied(),
        }
    }
}

/// `rgpa` region partition group area (HEIF Amd 1:2025 §6.8.13), when
/// `area_info_present`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct RegionPartitionArea {
    /// `reference_width`.
    pub reference_width: u32,
    /// `reference_height`.
    pub reference_height: u32,
    /// `top`.
    pub top: u32,
    /// `left`.
    pub left: u32,
    /// `width`.
    pub width: u32,
    /// `height`.
    pub height: u32,
}
impl RegionPartitionArea {
    /// Every field as a positional argument, in declaration order.
    pub fn new(
        reference_width: u32,
        reference_height: u32,
        top: u32,
        left: u32,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            reference_width,
            reference_height,
            top,
            left,
            width,
            height,
        }
    }
}

/// How the monoscopic fallback of a `stem` group relates to the pair
/// (HEIF Amd 1:2025 §6.8.11, `flags & 3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StereoFallbackPosition {
    /// 0.
    Unspecified,
    /// 1: co-located with the left view.
    Left,
    /// 2: co-located with the right view.
    Right,
    /// 3: centred between the views.
    Centre,
}

impl EntityGroup {
    /// `grouping_type == 'altr'`.
    pub fn is_alternatives(&self) -> bool {
        &self.grouping_type == b"altr"
    }

    /// `pymd` (HEIF Amd 1:2025 §6.8.12): the entities are the pyramid
    /// layers, lowest-resolution overview first and the base image
    /// last; the tile information per the flags.
    pub fn pyramid(&self) -> Result<Option<PyramidInfo>> {
        if &self.grouping_type != b"pymd" {
            return Ok(None);
        }
        let mut tile_sizes = Vec::new();
        if self.flags & PyramidInfo::FLAG_TILE_INFO_PRESENT != 0 {
            let n = if self.flags & PyramidInfo::FLAG_TILE_INFO_CONSTANT != 0 {
                1
            } else {
                self.entity_ids.len()
            };
            let mut r = Reader::new(&self.payload);
            for _ in 0..n {
                tile_sizes.push((r.u32("pymd tile_size_x")?, r.u32("pymd tile_size_y")?));
            }
        }
        Ok(Some(PyramidInfo { tile_sizes }))
    }

    /// `rgpa` (HEIF Amd 1:2025 §6.8.13): the region items of a
    /// partition; the area when `area_info_present` (else the area of
    /// the image the group is associated with through `rpds`).
    pub fn region_partition_area(&self) -> Result<Option<RegionPartitionArea>> {
        if &self.grouping_type != b"rgpa" || self.flags & 0x2000 == 0 {
            return Ok(None);
        }
        let large = self.flags & 0x1000 != 0;
        let mut r = Reader::new(&self.payload);
        let mut f = |what: &str| -> Result<u32> {
            if large {
                r.u32(what)
            } else {
                r.u16(what).map(u32::from)
            }
        };
        Ok(Some(RegionPartitionArea {
            reference_width: f("rgpa reference_width")?,
            reference_height: f("rgpa reference_height")?,
            top: f("rgpa top")?,
            left: f("rgpa left")?,
            width: f("rgpa width")?,
            height: f("rgpa height")?,
        }))
    }

    /// `unrg` (HEIF Amd 2:2026 §11.3.5.1, union of regions): the
    /// region items whose union the group denotes; `None` for other
    /// groups.
    pub fn region_union(&self) -> Option<&[u32]> {
        (&self.grouping_type == b"unrg").then_some(self.entity_ids.as_slice())
    }

    /// `corg` (HEIF Amd 2:2026 §11.3.5.2, compound region): `(main
    /// region item, the region items it logically includes)`; `None`
    /// for other groups or a `corg` with fewer than two entities (the
    /// clause's minimum — the inclusion is logical, not geometric).
    pub fn compound_region(&self) -> Option<(u32, &[u32])> {
        if &self.grouping_type != b"corg" || self.entity_ids.len() < 2 {
            return None;
        }
        Some((self.entity_ids[0], &self.entity_ids[1..]))
    }

    /// `stem` (HEIF Amd 1:2025 §6.8.11): `(left, right, monoscopic
    /// fallback, fallback position)`; `None` for other groups or a
    /// `stem` without exactly three entities.
    pub fn stereo_with_fallback(&self) -> Option<(u32, u32, u32, StereoFallbackPosition)> {
        if &self.grouping_type != b"stem" || self.entity_ids.len() != 3 {
            return None;
        }
        let pos = match self.flags & 3 {
            1 => StereoFallbackPosition::Left,
            2 => StereoFallbackPosition::Right,
            3 => StereoFallbackPosition::Centre,
            _ => StereoFallbackPosition::Unspecified,
        };
        Some((
            self.entity_ids[0],
            self.entity_ids[1],
            self.entity_ids[2],
            pos,
        ))
    }

    /// `ster` / `stem`: `(left, right)` of a stereo pair (HEIF §6.8.5 /
    /// Amd 1 §6.8.11).
    pub fn stereo_pair(&self) -> Option<(u32, u32)> {
        if (&self.grouping_type == b"ster" || &self.grouping_type == b"stem")
            && self.entity_ids.len() >= 2
        {
            Some((self.entity_ids[0], self.entity_ids[1]))
        } else {
            None
        }
    }
}

/// One `dref` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct DataReference {
    /// Entry box type (`url ` / `urn ` / `deti`).
    pub entry_type: FourCc,
    /// The data is in this file: `(flags & 1) == 1` for `url ` /
    /// `urn `; for a `deti` entry (HEIF Amd 2:2026 §6.11.5, whose
    /// flags are field widths) `external_tiles_urls == 0`.
    pub self_contained: bool,
    /// `location` string for `url ` entries (empty when self-contained).
    pub location: String,
    /// `name` string for `urn ` entries.
    pub name: String,
    /// The entry's FullBox flags.
    pub flags: u32,
    /// The entry body after the FullBox header (parsed by
    /// [`crate::tiled::DataEntryTiledItem::parse`] for `deti`).
    pub payload: Vec<u8>,
}
impl DataReference {
    /// Every field as a positional argument, in declaration order.
    pub fn new(
        entry_type: FourCc,
        self_contained: bool,
        location: String,
        name: String,
        flags: u32,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            entry_type,
            self_contained,
            location,
            name,
            flags,
            payload,
        }
    }
}

/// Below this many records (items + locations + `ipma` rows +
/// reference boxes + entity groups) the lookups walk the vectors.
const LOOKUP_THRESHOLD: usize = 64;

/// Positions of a [`Meta`]'s records by id — what turns the per-item
/// lookups of a large tree (a 1 000-tile grid, a burst of thousands)
/// from a walk into a table read. Built once, on the first lookup;
/// holds positions only, so the records are always read from the tree
/// itself and an entry is used only when the record at its position
/// still carries the id.
#[derive(Debug, Default)]
struct Lookup {
    /// The vector lengths the tables were built from.
    lens: [usize; 5],
    /// `item_ID` → position in `items` (the first of that id).
    items: std::collections::HashMap<u32, u32>,
    /// `item_ID` → position in `locations` (the first of that id).
    locations: std::collections::HashMap<u32, u32>,
    /// `item_ID` → positions of its rows in `associations`.
    associations: Positions,
    /// `from_item_ID` → positions in `references`.
    refs_from: Positions,
    /// Target id → positions of the `references` boxes naming it.
    refs_to: Positions,
    /// Entity id → positions of the `entity_groups` holding it.
    groups: Positions,
}

/// Ascending record positions per id, each with a slot — where in
/// the record the id sits (the index among a reference box's targets
/// or a group's entities; 0 where the id is the record's own key) —
/// so a table entry is checked against the tree in constant time.
/// Most ids have one position (an item's `ipma` row, a tile's `dimg`
/// box), so the first is stored inline and only the others in a list.
#[derive(Debug, Default)]
struct Positions {
    first: std::collections::HashMap<u32, (u32, u32)>,
    more: std::collections::HashMap<u32, Vec<(u32, u32)>>,
}

impl Positions {
    /// Record `id` at slot `slot` of the record at position `p`
    /// (positions arrive in ascending order; a second slot of the same
    /// record is dropped).
    fn push(&mut self, id: u32, p: u32, slot: u32) {
        match self.first.entry(id) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((p, slot));
            }
            std::collections::hash_map::Entry::Occupied(first) => {
                if first.get().0 == p {
                    return;
                }
                let more = self.more.entry(id).or_default();
                if more.last().map(|e| e.0) != Some(p) {
                    more.push((p, slot));
                }
            }
        }
    }

    /// The `(position, slot)` pairs of `id`, positions ascending.
    fn of(&self, id: u32) -> impl Iterator<Item = (usize, usize)> + '_ {
        let first = self.first.get(&id).copied();
        let more = first
            .and_then(|_| self.more.get(&id))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        first
            .into_iter()
            .chain(more.iter().copied())
            .map(|(p, slot)| (p as usize, slot as usize))
    }
}

impl Lookup {
    fn lens(meta: &Meta) -> [usize; 5] {
        [
            meta.items.len(),
            meta.locations.len(),
            meta.associations.len(),
            meta.references.len(),
            meta.entity_groups.len(),
        ]
    }

    fn matches(&self, meta: &Meta) -> bool {
        self.lens == Self::lens(meta)
    }

    fn build(meta: &Meta) -> Self {
        let mut l = Lookup {
            lens: Self::lens(meta),
            ..Lookup::default()
        };
        for (p, it) in meta.items.iter().enumerate() {
            l.items.entry(it.id).or_insert(p as u32);
        }
        for (p, loc) in meta.locations.iter().enumerate() {
            l.locations.entry(loc.item_id).or_insert(p as u32);
        }
        for (p, a) in meta.associations.iter().enumerate() {
            l.associations.push(a.item_id, p as u32, 0);
        }
        // A box naming one target several times is listed once.
        for (p, r) in meta.references.iter().enumerate() {
            l.refs_from.push(r.from_item_id, p as u32, 0);
            for (slot, to) in r.to_item_ids.iter().enumerate() {
                l.refs_to.push(*to, p as u32, slot as u32);
            }
        }
        for (p, g) in meta.entity_groups.iter().enumerate() {
            for (slot, e) in g.entity_ids.iter().enumerate() {
                l.groups.push(*e, p as u32, slot as u32);
            }
        }
        l
    }
}

/// The lazily built [`Lookup`] of a [`Meta`]. It is derived data: a
/// clone starts without one, and it takes no part in comparisons.
#[derive(Default)]
struct LookupCell(std::sync::OnceLock<Lookup>);

impl Clone for LookupCell {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for LookupCell {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for LookupCell {}

impl std::fmt::Debug for LookupCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.get().is_some() {
            "Lookup(built)"
        } else {
            "Lookup(-)"
        })
    }
}

/// Parsed file-level `meta` box.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Meta {
    /// `meta` FullBox version.
    pub version: u8,
    /// `meta` FullBox flags.
    pub flags: u32,
    /// `hdlr`.
    pub handler: Option<Handler>,
    /// `pitm`.
    pub primary_item_id: Option<u32>,
    /// `iinf` entries in file order.
    pub items: Vec<ItemInfo>,
    /// `iloc` records in file order.
    pub locations: Vec<ItemLocation>,
    /// `ipco` properties; `properties[i]` has 1-based index `i + 1`.
    pub properties: Vec<RawProperty>,
    /// All `ipma` rows (across every `ipma` box) in file order.
    pub associations: Vec<ItemPropertyAssociations>,
    /// `iref` children in file order.
    pub references: Vec<ItemReference>,
    /// `grpl` entity groups.
    pub entity_groups: Vec<EntityGroup>,
    /// `dinf/dref` entries (1-based `data_reference_index` maps to
    /// `data_references[index - 1]`).
    pub data_references: Vec<DataReference>,
    /// `idat` payload.
    pub idat: Option<Vec<u8>>,
    /// Number of `sinf` protection schemes declared in `ipro`.
    pub protection_scheme_count: u16,
    /// The `ipro` box's `sinf` entries (an item's 1-based
    /// `item_protection_index` names one).
    pub protection_schemes: Vec<crate::cenc::ProtectionScheme>,
    /// An `ipma` box listed its rows out of increasing `item_ID` order
    /// (ISO/IEC 14496-12 §8.11.14.1 requires the order; the rows are
    /// read all the same and [`crate::miaf::check`] reports it).
    pub ipma_unordered: bool,
    /// Positions by id, built on the first lookup of a large tree (see
    /// [`Meta::reindex`]).
    lookup: LookupCell,
    /// `xml ` box payload (UTF-8), when present.
    pub xml: Option<String>,
    /// Box types encountered directly inside `meta`, in file order.
    pub child_box_types: Vec<FourCc>,
}
impl Meta {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        version: u8,
        flags: u32,
        handler: Option<Handler>,
        primary_item_id: Option<u32>,
        items: Vec<ItemInfo>,
        locations: Vec<ItemLocation>,
        properties: Vec<RawProperty>,
        associations: Vec<ItemPropertyAssociations>,
        references: Vec<ItemReference>,
        entity_groups: Vec<EntityGroup>,
        data_references: Vec<DataReference>,
        idat: Option<Vec<u8>>,
        protection_scheme_count: u16,
        xml: Option<String>,
        child_box_types: Vec<FourCc>,
    ) -> Self {
        Self {
            version,
            flags,
            handler,
            primary_item_id,
            items,
            locations,
            properties,
            associations,
            references,
            entity_groups,
            data_references,
            idat,
            protection_scheme_count,
            protection_schemes: Vec::new(),
            ipma_unordered: false,
            lookup: LookupCell::default(),
            xml,
            child_box_types,
        }
    }

    /// The `sinf` an item's `item_protection_index` names (`None` for an
    /// unprotected item or an index past the `ipro` entries).
    pub fn protection_scheme(&self, item_id: u32) -> Option<&crate::cenc::ProtectionScheme> {
        let i = self.item(item_id)?.protection_index;
        i.checked_sub(1)
            .and_then(|i| self.protection_schemes.get(i as usize))
    }
}

impl Meta {
    /// Parse a `meta` box payload (the FullBox header included).
    pub fn parse(meta_payload: &[u8]) -> Result<Self> {
        let (version, flags, body) = parse_full_box(meta_payload)?;
        if version != 0 {
            return Err(HeifError::invalid(format!(
                "meta box version {version} (only 0 is defined)"
            )));
        }
        let mut me = Meta {
            version,
            flags,
            ..Meta::default()
        };
        let mut seen_ipma_keys: Vec<(u8, u32)> = Vec::new();
        for h in iter_boxes(body) {
            let h = h?;
            let p = payload(body, &h);
            me.child_box_types.push(h.box_type);
            match &h.box_type {
                b"hdlr" => me.handler = Some(parse_hdlr(p)?),
                b"pitm" => me.primary_item_id = Some(parse_pitm(p)?),
                b"iinf" => me.items = parse_iinf(p)?,
                b"iloc" => me.locations = parse_iloc(p)?,
                b"iref" => me.references = parse_iref(p)?,
                b"iprp" => {
                    let (props, assocs, unordered) = parse_iprp(p, &mut seen_ipma_keys)?;
                    me.properties = props;
                    me.associations = assocs;
                    me.ipma_unordered = unordered;
                }
                b"idat" => me.idat = Some(p.to_vec()),
                b"grpl" => me.entity_groups = parse_grpl(p)?,
                b"dinf" => me.data_references = parse_dinf(p)?,
                b"ipro" => {
                    (me.protection_scheme_count, me.protection_schemes) = parse_ipro(p)?;
                }
                b"xml " => {
                    let (_v, _f, x) = parse_full_box(p)?;
                    me.xml = Some(String::from_utf8_lossy(x).into_owned());
                }
                _ => {}
            }
        }
        me.validate()?;
        Ok(me)
    }

    fn validate(&self) -> Result<()> {
        // Unique item ids (§8.11.6: item_ID is unique within the box).
        let mut ids: Vec<u32> = self.items.iter().map(|i| i.id).collect();
        ids.sort_unstable();
        if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
            return Err(HeifError::invalid(format!(
                "duplicate item_ID {} in iinf",
                w[0]
            )));
        }
        // ipma indices must address an existing property.
        for a in &self.associations {
            for e in &a.entries {
                if e.index as usize > self.properties.len() {
                    return Err(HeifError::invalid(format!(
                        "ipma: item {} references property index {} but ipco holds {}",
                        a.item_id,
                        e.index,
                        self.properties.len()
                    )));
                }
            }
        }
        Ok(())
    }

    /// The id → position tables of a large tree (`None` for a small
    /// one, or once a vector's length no longer matches — the linear
    /// walk then answers).
    fn lookup(&self) -> Option<&Lookup> {
        let entries = self.items.len()
            + self.locations.len()
            + self.associations.len()
            + self.references.len()
            + self.entity_groups.len();
        if entries < LOOKUP_THRESHOLD {
            return None;
        }
        let l = self.lookup.0.get_or_init(|| Lookup::build(self));
        l.matches(self).then_some(l)
    }

    /// Drop the id → position tables the lookups of a large tree use
    /// (`item`, `location`, `properties_of`, `references_from` /
    /// `references_to`, the entity-group queries); the next lookup
    /// rebuilds them. The tables follow pushes and removals on their
    /// own and never hand out an entry of another id — call this after
    /// rewriting ids *in place* (an `item_ID`, a reference's
    /// `from_item_ID` / targets, an `ipma` row's item, a group's
    /// entities) in a tree that has already been queried.
    pub fn reindex(&mut self) {
        self.lookup = LookupCell::default();
    }

    /// Look up an item by id.
    pub fn item(&self, id: u32) -> Option<&ItemInfo> {
        if let Some(l) = self.lookup()
            && let Some(it) = l.items.get(&id).and_then(|&p| self.items.get(p as usize))
            && it.id == id
        {
            return Some(it);
        }
        self.items.iter().find(|i| i.id == id)
    }

    /// Look up an item location by id.
    pub fn location(&self, id: u32) -> Option<&ItemLocation> {
        if let Some(l) = self.lookup()
            && let Some(loc) = l
                .locations
                .get(&id)
                .and_then(|&p| self.locations.get(p as usize))
            && loc.item_id == id
        {
            return Some(loc);
        }
        self.locations.iter().find(|l| l.item_id == id)
    }

    /// The `ipma` rows of `item_id`, in file order.
    fn association_rows(&self, item_id: u32) -> Vec<&ItemPropertyAssociations> {
        if let Some(l) = self.lookup() {
            let rows: Option<Vec<&ItemPropertyAssociations>> = l
                .associations
                .of(item_id)
                .map(|(p, _)| self.associations.get(p).filter(|a| a.item_id == item_id))
                .collect();
            if let Some(rows) = rows {
                return rows;
            }
        }
        self.associations
            .iter()
            .filter(|a| a.item_id == item_id)
            .collect()
    }

    /// The reference boxes whose source is `id` (`by_source`) or that
    /// name `id` among their targets, in file order.
    fn reference_boxes(&self, id: u32, by_source: bool) -> Vec<&ItemReference> {
        if let Some(l) = self.lookup() {
            let table = if by_source { &l.refs_from } else { &l.refs_to };
            let boxes: Option<Vec<&ItemReference>> = table
                .of(id)
                .map(|(p, slot)| {
                    self.references.get(p).filter(|r| {
                        if by_source {
                            r.from_item_id == id
                        } else {
                            r.to_item_ids.get(slot) == Some(&id)
                        }
                    })
                })
                .collect();
            if let Some(boxes) = boxes {
                return boxes;
            }
        }
        self.references
            .iter()
            .filter(|r| {
                if by_source {
                    r.from_item_id == id
                } else {
                    r.to_item_ids.contains(&id)
                }
            })
            .collect()
    }

    /// The entity groups holding `entity_id`, in file order.
    fn groups_of(&self, entity_id: u32) -> Vec<&EntityGroup> {
        if let Some(l) = self.lookup() {
            let groups: Option<Vec<&EntityGroup>> = l
                .groups
                .of(entity_id)
                .map(|(p, slot)| {
                    self.entity_groups
                        .get(p)
                        .filter(|g| g.entity_ids.get(slot) == Some(&entity_id))
                })
                .collect();
            if let Some(groups) = groups {
                return groups;
            }
        }
        self.entity_groups
            .iter()
            .filter(|g| g.entity_ids.contains(&entity_id))
            .collect()
    }

    /// The primary item's info, when `pitm` names an existing item.
    pub fn primary_item(&self) -> Option<&ItemInfo> {
        self.primary_item_id.and_then(|id| self.item(id))
    }

    /// Properties associated with `item_id`, in `ipma` order, as
    /// `(1-based index, property, essential)`. Rows from several `ipma`
    /// boxes for the same item are concatenated in file order.
    pub fn properties_of(&self, item_id: u32) -> Vec<(u16, &RawProperty, bool)> {
        let mut out = Vec::new();
        for a in self.association_rows(item_id) {
            for e in &a.entries {
                if let Some(p) = self.properties.get(e.index as usize - 1) {
                    out.push((e.index, p, e.essential));
                }
            }
        }
        out
    }

    /// Raw association entries of an item, in `ipma` order.
    pub fn associations_of(&self, item_id: u32) -> Vec<PropertyAssociation> {
        self.association_rows(item_id)
            .into_iter()
            .flat_map(|a| a.entries.iter().copied())
            .collect()
    }

    /// The first property of `box_type` associated with `item_id`.
    pub fn property_of(&self, item_id: u32, box_type: &FourCc) -> Option<&RawProperty> {
        self.properties_of(item_id)
            .into_iter()
            .find(|(_, p, _)| &p.box_type == box_type)
            .map(|(_, p, _)| p)
    }

    /// Targets of the references of `reference_type` whose source is
    /// `from`, in file order (HEIF §6.6.1 allows at most one `dimg` box
    /// per item; several boxes of other types are concatenated).
    pub fn references_from(&self, from: u32, reference_type: &FourCc) -> Vec<u32> {
        self.reference_boxes(from, true)
            .into_iter()
            .filter(|r| &r.reference_type == reference_type)
            .flat_map(|r| r.to_item_ids.iter().copied())
            .collect()
    }

    /// Sources of the references of `reference_type` that target `to`,
    /// in file order.
    pub fn references_to(&self, to: u32, reference_type: &FourCc) -> Vec<u32> {
        self.reference_boxes(to, false)
            .into_iter()
            .filter(|r| &r.reference_type == reference_type)
            .map(|r| r.from_item_id)
            .collect()
    }

    /// Items with a `thmb` reference to `master`.
    pub fn thumbnails_of(&self, master: u32) -> Vec<u32> {
        self.references_to(master, &reference::THMB)
    }

    /// Items with an `auxl` reference to `master`.
    pub fn auxiliaries_of(&self, master: u32) -> Vec<u32> {
        self.references_to(master, &reference::AUXL)
    }

    /// Metadata items with a `cdsc` reference to `image`.
    pub fn metadata_of(&self, image: u32) -> Vec<u32> {
        self.references_to(image, &reference::CDSC)
    }

    /// Input images of a derived image item (`dimg` targets).
    pub fn derivation_inputs(&self, derived: u32) -> Vec<u32> {
        self.references_from(derived, &reference::DIMG)
    }

    /// `true` when `master` carries a `prem` reference to `aux`
    /// (master samples are pre-multiplied by that alpha plane).
    pub fn is_premultiplied(&self, master: u32, aux: u32) -> bool {
        self.references_from(master, &reference::PREM)
            .contains(&aux)
    }

    /// The image collection as a viewer lists it (MIAF Amd 1:2025
    /// Annex A.3 / A.4, informative): every displayable master image
    /// item (non-hidden, not a thumbnail / auxiliary) in `iinf` order,
    /// with the members of each `altr` group collapsed into one entry
    /// at the position of the group's earliest member. Each entry is
    /// the `altr` group's entities in group order (its first
    /// processable one is the one to show, ISO/IEC 14496-12 §8.18.3)
    /// or a single item.
    pub fn display_order(&self) -> Vec<Vec<u32>> {
        let mut out: Vec<Vec<u32>> = Vec::new();
        let mut seen_groups: Vec<u32> = Vec::new();
        for it in &self.items {
            if !it.is_image()
                || it.is_hidden()
                || !self.references_from(it.id, &reference::THMB).is_empty()
                || !self.references_from(it.id, &reference::AUXL).is_empty()
            {
                continue;
            }
            match self
                .groups_of(it.id)
                .into_iter()
                .find(|g| g.is_alternatives())
            {
                Some(g) => {
                    if !seen_groups.contains(&g.group_id) {
                        seen_groups.push(g.group_id);
                        out.push(g.entity_ids.clone());
                    }
                }
                None => out.push(vec![it.id]),
            }
        }
        out
    }

    /// The region items (`rgan`) describing `image`: those with a
    /// `cdsc` reference to it (HEIF §6.10).
    pub fn region_items_of(&self, image: u32) -> Vec<u32> {
        self.references_to(image, &reference::CDSC)
            .into_iter()
            .filter(|id| {
                self.item(*id)
                    .is_some_and(|i| i.item_type == ITEM_TYPE_RGAN)
            })
            .collect()
    }

    /// The `unrg` / `corg` groups of regions defined inside `image`
    /// (HEIF Amd 2:2026 §11.3.5): every entity of such a group is a
    /// region item with a `cdsc` reference to that image (the group
    /// may additionally carry its own `cdsc` under unique ids).
    pub fn region_groups_of(&self, image: u32) -> Vec<&EntityGroup> {
        let regions = self.region_items_of(image);
        self.entity_groups
            .iter()
            .filter(|g| matches!(&g.grouping_type, b"unrg" | b"corg"))
            .filter(|g| {
                !g.entity_ids.is_empty() && g.entity_ids.iter().all(|e| regions.contains(e))
            })
            .collect()
    }

    /// Entity groups of `grouping_type` containing `entity_id`.
    pub fn groups_containing(&self, entity_id: u32, grouping_type: &FourCc) -> Vec<&EntityGroup> {
        self.groups_of(entity_id)
            .into_iter()
            .filter(|g| &g.grouping_type == grouping_type)
            .collect()
    }

    /// `true` when the item's `data_reference_index` resolves to data in
    /// this file (index 0, or a self-contained `dref` entry).
    pub fn is_self_contained(&self, loc: &ItemLocation) -> bool {
        match loc.data_reference_index {
            0 => true,
            n => self
                .data_references
                .get(n as usize - 1)
                .map(|d| d.self_contained)
                .unwrap_or(false),
        }
    }
}

fn parse_hdlr(p: &[u8]) -> Result<Handler> {
    let (_v, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    r.skip(4, "hdlr pre_defined")?;
    let handler_type = r.fourcc("hdlr handler_type")?;
    r.skip(12, "hdlr reserved")?;
    let name = if r.is_empty() {
        String::new()
    } else {
        r.cstr("hdlr name")?
    };
    Ok(Handler { handler_type, name })
}

fn parse_pitm(p: &[u8]) -> Result<u32> {
    let (version, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    match version {
        0 => r.u16("pitm item_ID").map(u32::from),
        1 => r.u32("pitm item_ID"),
        v => Err(HeifError::invalid(format!("pitm version {v}"))),
    }
}

fn parse_iinf(p: &[u8]) -> Result<Vec<ItemInfo>> {
    let (version, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    let count = match version {
        0 => r.u16("iinf entry_count")? as usize,
        _ => r.u32("iinf entry_count")? as usize,
    };
    if count > MAX_ITEMS {
        return Err(HeifError::exhausted(format!(
            "iinf declares {count} items (limit {MAX_ITEMS})"
        )));
    }
    let entries = r.rest();
    let mut out = Vec::with_capacity(count.min(4096));
    let mut cursor = 0usize;
    while out.len() < count {
        if cursor >= entries.len() {
            return Err(HeifError::invalid(format!(
                "iinf declares {count} entries but only {} infe boxes are present",
                out.len()
            )));
        }
        let h = parse_box_header(entries, cursor)?;
        if &h.box_type == b"infe" {
            out.push(parse_infe(payload(entries, &h))?);
        }
        cursor = h.end();
    }
    Ok(out)
}

fn parse_infe(p: &[u8]) -> Result<ItemInfo> {
    let (version, flags, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    let (id, protection_index, item_type) = match version {
        2 => {
            let id = r.u16("infe item_ID")? as u32;
            let pi = r.u16("infe item_protection_index")?;
            (id, pi, r.fourcc("infe item_type")?)
        }
        3 => {
            let id = r.u32("infe item_ID")?;
            let pi = r.u16("infe item_protection_index")?;
            (id, pi, r.fourcc("infe item_type")?)
        }
        v => {
            return Err(HeifError::unsupported(format!(
                "infe version {v} (HEIF requires 2 or 3)"
            )));
        }
    };
    let name = r.cstr("infe item_name")?;
    let mut content_type = None;
    let mut content_encoding = None;
    let mut item_uri_type = None;
    if item_type == ITEM_TYPE_MIME {
        content_type = Some(r.cstr("infe content_type")?);
        if !r.is_empty() {
            let ce = r.cstr("infe content_encoding")?;
            if !ce.is_empty() {
                content_encoding = Some(ce);
            }
        }
    } else if item_type == ITEM_TYPE_URI {
        item_uri_type = Some(r.cstr("infe item_uri_type")?);
    }
    Ok(ItemInfo {
        id,
        version,
        flags,
        protection_index,
        item_type,
        name,
        content_type,
        content_encoding,
        item_uri_type,
    })
}

fn parse_iloc(p: &[u8]) -> Result<Vec<ItemLocation>> {
    let (version, _f, body) = parse_full_box(p)?;
    if version > 2 {
        return Err(HeifError::invalid(format!("iloc version {version}")));
    }
    let mut r = Reader::new(body);
    let b0 = r.u8("iloc sizes")?;
    let b1 = r.u8("iloc sizes")?;
    let offset_size = (b0 >> 4) as usize;
    let length_size = (b0 & 0x0f) as usize;
    let base_offset_size = (b1 >> 4) as usize;
    let index_size = if version >= 1 {
        (b1 & 0x0f) as usize
    } else {
        0
    };
    for (name, w) in [
        ("offset_size", offset_size),
        ("length_size", length_size),
        ("base_offset_size", base_offset_size),
        ("index_size", index_size),
    ] {
        if !matches!(w, 0 | 4 | 8) {
            return Err(HeifError::invalid(format!(
                "iloc {name} {w} not in {{0, 4, 8}}"
            )));
        }
    }
    let item_count = if version < 2 {
        r.u16("iloc item_count")? as usize
    } else {
        r.u32("iloc item_count")? as usize
    };
    if item_count > MAX_ITEMS {
        return Err(HeifError::exhausted(format!(
            "iloc declares {item_count} items (limit {MAX_ITEMS})"
        )));
    }
    let mut out = Vec::with_capacity(item_count.min(4096));
    for _ in 0..item_count {
        let item_id = if version < 2 {
            r.u16("iloc item_ID")? as u32
        } else {
            r.u32("iloc item_ID")?
        };
        let construction_method = if version >= 1 {
            (r.u16("iloc construction_method")? & 0x0f) as u8
        } else {
            0
        };
        if construction_method > 2 {
            return Err(HeifError::invalid(format!(
                "iloc item {item_id}: construction_method {construction_method}"
            )));
        }
        let data_reference_index = r.u16("iloc data_reference_index")?;
        let base_offset = r.uint(base_offset_size, "iloc base_offset")?;
        let extent_count = r.u16("iloc extent_count")? as usize;
        let mut extents = Vec::with_capacity(extent_count.min(256));
        for _ in 0..extent_count {
            let index = if version >= 1 && index_size > 0 {
                r.uint(index_size, "iloc extent_index")?
            } else {
                0
            };
            let offset = r.uint(offset_size, "iloc extent_offset")?;
            let length = r.uint(length_size, "iloc extent_length")?;
            extents.push(Extent {
                index,
                offset,
                length,
            });
        }
        out.push(ItemLocation {
            item_id,
            construction_method,
            data_reference_index,
            base_offset,
            extents,
        });
    }
    Ok(out)
}

fn parse_iref(p: &[u8]) -> Result<Vec<ItemReference>> {
    let (version, _f, body) = parse_full_box(p)?;
    if version > 1 {
        return Err(HeifError::invalid(format!("iref version {version}")));
    }
    let mut out = Vec::new();
    for h in iter_boxes(body) {
        let h = h?;
        if out.len() >= MAX_REFERENCES {
            return Err(HeifError::exhausted(format!(
                "iref holds more than {MAX_REFERENCES} reference boxes"
            )));
        }
        let mut r = Reader::new(payload(body, &h));
        let from_item_id = if version == 0 {
            r.u16("iref from_item_ID")? as u32
        } else {
            r.u32("iref from_item_ID")?
        };
        let count = r.u16("iref reference_count")? as usize;
        let mut to_item_ids = Vec::with_capacity(count);
        for _ in 0..count {
            to_item_ids.push(if version == 0 {
                r.u16("iref to_item_ID")? as u32
            } else {
                r.u32("iref to_item_ID")?
            });
        }
        out.push(ItemReference {
            reference_type: h.box_type,
            from_item_id,
            to_item_ids,
        });
    }
    Ok(out)
}

fn parse_iprp(
    p: &[u8],
    seen_ipma_keys: &mut Vec<(u8, u32)>,
) -> Result<(Vec<RawProperty>, Vec<ItemPropertyAssociations>, bool)> {
    let mut props = Vec::new();
    let mut assocs = Vec::new();
    let mut unordered = false;
    let mut saw_ipco = false;
    for h in iter_boxes(p) {
        let h = h?;
        let body = payload(p, &h);
        match &h.box_type {
            b"ipco" => {
                if saw_ipco {
                    return Err(HeifError::invalid("iprp carries more than one ipco"));
                }
                saw_ipco = true;
                props = parse_ipco(body)?;
            }
            b"ipma" => {
                let (rows, out_of_order) = parse_ipma(body, seen_ipma_keys)?;
                assocs.extend(rows);
                unordered |= out_of_order;
            }
            _ => {}
        }
    }
    if !saw_ipco {
        return Err(HeifError::invalid("iprp without ipco"));
    }
    Ok((props, assocs, unordered))
}

fn parse_ipco(p: &[u8]) -> Result<Vec<RawProperty>> {
    let mut out = Vec::new();
    for h in iter_boxes(p) {
        let h = h?;
        if out.len() >= MAX_PROPERTIES {
            return Err(HeifError::exhausted(format!(
                "ipco holds more than {MAX_PROPERTIES} properties"
            )));
        }
        out.push(RawProperty {
            box_type: h.box_type,
            user_type: h.user_type,
            body: payload(p, &h).to_vec(),
            box_size: h.total_len(),
        });
    }
    Ok(out)
}

fn parse_ipma(
    p: &[u8],
    seen: &mut Vec<(u8, u32)>,
) -> Result<(Vec<ItemPropertyAssociations>, bool)> {
    let (version, flags, body) = parse_full_box(p)?;
    if version > 1 {
        return Err(HeifError::invalid(format!("ipma version {version}")));
    }
    // HEIF §9.3.1: at most one ipma box per (version, flags) pair.
    if seen.contains(&(version, flags)) {
        return Err(HeifError::invalid(format!(
            "duplicate ipma box with version {version} flags {flags}"
        )));
    }
    seen.push((version, flags));
    let large = flags & 1 == 1;
    let mut r = Reader::new(body);
    let entry_count = r.u32("ipma entry_count")? as usize;
    if entry_count > MAX_ITEMS {
        return Err(HeifError::exhausted(format!(
            "ipma declares {entry_count} entries (limit {MAX_ITEMS})"
        )));
    }
    let mut out: Vec<ItemPropertyAssociations> = Vec::with_capacity(entry_count.min(4096));
    let mut last_id: Option<u32> = None;
    let mut unordered_ids: Option<std::collections::HashSet<u32>> = None;
    for _ in 0..entry_count {
        let item_id = if version == 0 {
            r.u16("ipma item_ID")? as u32
        } else {
            r.u32("ipma item_ID")?
        };
        // ISO/IEC 14496-12 §8.11.14.1: "ordered by increasing item_ID"
        // with at most one row per item. A repeated item is ambiguous
        // and refused; rows merely out of order name the same
        // associations, so they are read and the departure is recorded
        // for the conformance check (`Meta::ipma_unordered`).
        if let Some(prev) = last_id
            && item_id <= prev
            && unordered_ids.is_none()
        {
            // First departure: remember every id read so far.
            unordered_ids = Some(out.iter().map(|a| a.item_id).collect());
        }
        if let Some(ids) = unordered_ids.as_mut()
            && !ids.insert(item_id)
        {
            return Err(HeifError::invalid(format!(
                "ipma names item {item_id} twice in one box"
            )));
        }
        last_id = Some(item_id);
        let n = r.u8("ipma association_count")? as usize;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let (essential, index) = if large {
                let w = r.u16("ipma association")?;
                (w & 0x8000 != 0, w & 0x7fff)
            } else {
                let w = r.u8("ipma association")?;
                (w & 0x80 != 0, (w & 0x7f) as u16)
            };
            if index == 0 {
                if essential {
                    return Err(HeifError::invalid(format!(
                        "ipma: item {item_id} has an essential association with property index 0"
                    )));
                }
                continue; // "no property is associated"
            }
            entries.push(PropertyAssociation { index, essential });
        }
        out.push(ItemPropertyAssociations { item_id, entries });
    }
    Ok((out, unordered_ids.is_some()))
}

fn parse_grpl(p: &[u8]) -> Result<Vec<EntityGroup>> {
    let mut out = Vec::new();
    for h in iter_boxes(p) {
        let h = h?;
        if out.len() >= MAX_ENTITY_GROUPS {
            return Err(HeifError::exhausted(format!(
                "grpl holds more than {MAX_ENTITY_GROUPS} groups"
            )));
        }
        let (version, flags, body) = parse_full_box(payload(p, &h))?;
        let mut r = Reader::new(body);
        let group_id = r.u32("EntityToGroupBox group_id")?;
        let n = r.u32("EntityToGroupBox num_entities_in_group")? as usize;
        if n > r.remaining() / 4 {
            return Err(HeifError::invalid(format!(
                "EntityToGroupBox '{}' declares {n} entities but only {} bytes follow",
                fourcc_str(&h.box_type),
                r.remaining()
            )));
        }
        let mut entity_ids = Vec::with_capacity(n);
        for _ in 0..n {
            entity_ids.push(r.u32("EntityToGroupBox entity_id")?);
        }
        out.push(EntityGroup {
            grouping_type: h.box_type,
            version,
            flags,
            group_id,
            entity_ids,
            payload: r.rest().to_vec(),
        });
    }
    Ok(out)
}

fn parse_dinf(p: &[u8]) -> Result<Vec<DataReference>> {
    let Some((_h, dref)) = crate::boxes::find_box(p, b"dref")? else {
        return Ok(Vec::new());
    };
    let (_v, _f, body) = parse_full_box(dref)?;
    let mut r = Reader::new(body);
    let n = r.u32("dref entry_count")? as usize;
    let entries = r.rest();
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while out.len() < n && cursor < entries.len() {
        let h = parse_box_header(entries, cursor)?;
        let (_ev, eflags, ebody) = parse_full_box(payload(entries, &h))?;
        let mut er = Reader::new(ebody);
        let (location, name) = match &h.box_type {
            b"url " => (
                if er.is_empty() {
                    String::new()
                } else {
                    er.cstr("url location")?
                },
                String::new(),
            ),
            b"urn " => {
                let name = er.cstr("urn name")?;
                let location = if er.is_empty() {
                    String::new()
                } else {
                    er.cstr("urn location")?
                };
                (location, name)
            }
            _ => (String::new(), String::new()),
        };
        let self_contained = if &h.box_type == b"deti" {
            (eflags >> 7) & 1 == 0
        } else {
            eflags & 1 == 1
        };
        out.push(DataReference {
            entry_type: h.box_type,
            self_contained,
            location,
            name,
            flags: eflags,
            payload: ebody.to_vec(),
        });
        cursor = h.end();
    }
    Ok(out)
}

fn parse_ipro(p: &[u8]) -> Result<(u16, Vec<crate::cenc::ProtectionScheme>)> {
    let (_v, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    let count = r.u16("ipro protection_count")?;
    let mut schemes = Vec::new();
    for h in crate::boxes::iter_boxes(r.rest()) {
        let h = h?;
        if &h.box_type == b"sinf" && schemes.len() < count as usize {
            schemes.push(crate::cenc::parse_sinf(crate::boxes::payload(
                r.rest(),
                &h,
            ))?);
        }
    }
    Ok((count, schemes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_payload_resolves_the_offset_word_and_tolerates_a_bare_tiff_header() {
        assert_eq!(
            exif_payload(&[0, 0, 0, 0, b'M', b'M', 0, 42]).unwrap(),
            b"MM\0*"
        );
        assert_eq!(
            exif_payload(&[0, 0, 0, 2, 9, 9, b'I', b'I', 42, 0]).unwrap(),
            b"II*\0"
        );
        // No offset word at all (C034's shape): the body is the payload.
        assert_eq!(
            exif_payload(b"MM\0*\0\0\0\x08").unwrap(),
            b"MM\0*\0\0\0\x08"
        );
        assert_eq!(exif_data_block(b"II*\0"), b"\0\0\0\0II*\0");
        assert!(exif_payload(&[0, 0, 0, 9, 1, 2]).is_err());
        assert!(exif_payload(&[1, 2]).is_err());
    }
    use crate::boxes::write::{boxed, full_boxed};

    fn infe_v2(id: u16, ty: &[u8; 4], name: &str) -> Vec<u8> {
        let mut b = id.to_be_bytes().to_vec();
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(ty);
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        full_boxed(b"infe", 2, 0, &b)
    }

    fn iinf_v0(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut b = (entries.len() as u16).to_be_bytes().to_vec();
        for e in entries {
            b.extend_from_slice(e);
        }
        full_boxed(b"iinf", 0, 0, &b)
    }

    fn hdlr_pict() -> Vec<u8> {
        let mut b = vec![0u8; 4];
        b.extend_from_slice(b"pict");
        b.extend_from_slice(&[0u8; 12]);
        b.push(0);
        full_boxed(b"hdlr", 0, 0, &b)
    }

    fn pitm_v0(id: u16) -> Vec<u8> {
        full_boxed(b"pitm", 0, 0, &id.to_be_bytes())
    }

    type IlocItem = (u16, u8, Vec<(u32, u32)>);

    fn iloc_v1(items: &[IlocItem]) -> Vec<u8> {
        // offset_size 4, length_size 4, base_offset_size 0, index_size 0
        let mut b = vec![0x44u8, 0x00];
        b.extend_from_slice(&(items.len() as u16).to_be_bytes());
        for (id, cm, extents) in items {
            b.extend_from_slice(&id.to_be_bytes());
            b.extend_from_slice(&(*cm as u16).to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes()); // dref idx
            b.extend_from_slice(&(extents.len() as u16).to_be_bytes());
            for (o, l) in extents {
                b.extend_from_slice(&o.to_be_bytes());
                b.extend_from_slice(&l.to_be_bytes());
            }
        }
        full_boxed(b"iloc", 1, 0, &b)
    }

    fn iprp(props: &[Vec<u8>], rows: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut ipco = Vec::new();
        for p in props {
            ipco.extend_from_slice(p);
        }
        let mut ipma = (rows.len() as u32).to_be_bytes().to_vec();
        for (id, assoc) in rows {
            ipma.extend_from_slice(&id.to_be_bytes());
            ipma.push(assoc.len() as u8);
            ipma.extend_from_slice(assoc);
        }
        let mut body = boxed(b"ipco", &ipco);
        body.extend(full_boxed(b"ipma", 0, 0, &ipma));
        boxed(b"iprp", &body)
    }

    fn iref_v0(refs: &[(&[u8; 4], u16, Vec<u16>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (ty, from, to) in refs {
            let mut b = from.to_be_bytes().to_vec();
            b.extend_from_slice(&(to.len() as u16).to_be_bytes());
            for t in to {
                b.extend_from_slice(&t.to_be_bytes());
            }
            body.extend(boxed(ty, &b));
        }
        full_boxed(b"iref", 0, 0, &body)
    }

    fn meta_box(children: &[Vec<u8>]) -> Vec<u8> {
        let mut body = vec![0u8; 4];
        for c in children {
            body.extend_from_slice(c);
        }
        boxed(b"meta", &body)
    }

    #[test]
    fn parses_a_minimal_collection() {
        let ispe = full_boxed(b"ispe", 0, 0, &[0, 0, 0, 8, 0, 0, 0, 8]);
        let hvcc = boxed(b"hvcC", &[1, 2, 3]);
        let m = meta_box(&[
            hdlr_pict(),
            pitm_v0(1),
            iinf_v0(&[infe_v2(1, b"hvc1", ""), infe_v2(2, b"Exif", "exif")]),
            iloc_v1(&[(1, 0, vec![(100, 50)]), (2, 1, vec![(0, 10)])]),
            iprp(&[hvcc, ispe], &[(1, vec![0x81, 0x02]), (2, vec![0x00])]),
            iref_v0(&[(b"cdsc", 2, vec![1])]),
            boxed(b"idat", &[9; 10]),
        ]);
        let h = parse_box_header(&m, 0).unwrap();
        let meta = Meta::parse(payload(&m, &h)).unwrap();
        assert_eq!(meta.handler.as_ref().unwrap().handler_type, *b"pict");
        assert_eq!(meta.primary_item_id, Some(1));
        assert_eq!(meta.items.len(), 2);
        assert_eq!(meta.items[1].name, "exif");
        assert_eq!(meta.locations[1].construction_method, 1);
        assert_eq!(meta.properties.len(), 2);
        let props = meta.properties_of(1);
        assert_eq!(props.len(), 2);
        assert_eq!(props[0].0, 1);
        assert!(props[0].2, "hvcC essential");
        assert_eq!(&props[1].1.box_type, b"ispe");
        assert!(!props[1].2);
        // Index-0 placeholder dropped.
        assert!(meta.properties_of(2).is_empty());
        assert_eq!(meta.metadata_of(1), vec![2]);
        assert_eq!(meta.references_from(2, &reference::CDSC), vec![1]);
        assert_eq!(meta.idat.as_deref(), Some(&[9u8; 10][..]));
        assert!(meta.primary_item().unwrap().is_coded_image());
        assert_eq!(meta.property_of(1, b"ispe").unwrap().box_size, 20);
    }

    #[test]
    fn rejects_duplicate_ids_and_bad_indices() {
        let m = meta_box(&[
            hdlr_pict(),
            iinf_v0(&[infe_v2(1, b"hvc1", ""), infe_v2(1, b"hvc1", "")]),
        ]);
        let h = parse_box_header(&m, 0).unwrap();
        assert!(Meta::parse(payload(&m, &h)).is_err());

        let m = meta_box(&[
            hdlr_pict(),
            iinf_v0(&[infe_v2(1, b"hvc1", "")]),
            iprp(&[boxed(b"ispe", &[0; 12])], &[(1, vec![0x05])]),
        ]);
        let h = parse_box_header(&m, 0).unwrap();
        let err = Meta::parse(payload(&m, &h)).unwrap_err();
        assert!(err.to_string().contains("property index 5"), "{err}");
    }

    #[test]
    fn ipma_rows_out_of_order_are_read_and_flagged() {
        // ISO/IEC 14496-12 §8.11.14.1 orders the rows by item_ID; a
        // box that does not is read all the same and flagged.
        let m = meta_box(&[
            hdlr_pict(),
            iprp(
                &[boxed(b"ispe", &[0; 12])],
                &[(2, vec![0x01]), (5, vec![]), (1, vec![0x81])],
            ),
        ]);
        let h = parse_box_header(&m, 0).unwrap();
        let meta = Meta::parse(payload(&m, &h)).unwrap();
        assert!(meta.ipma_unordered);
        assert_eq!(meta.properties_of(1).len(), 1);
        assert!(meta.properties_of(1)[0].2);
        assert_eq!(meta.properties_of(2).len(), 1);
        // In order: not flagged.
        let m = meta_box(&[
            hdlr_pict(),
            iprp(
                &[boxed(b"ispe", &[0; 12])],
                &[(1, vec![0x01]), (2, vec![0x01])],
            ),
        ]);
        let h = parse_box_header(&m, 0).unwrap();
        assert!(!Meta::parse(payload(&m, &h)).unwrap().ipma_unordered);
    }

    #[test]
    fn ipma_repeated_item_and_essential_zero_are_rejected() {
        // The same item twice in one box, adjacent or not.
        for rows in [
            vec![(1u16, vec![0x01u8]), (1, vec![0x01])],
            vec![(2, vec![0x01]), (1, vec![0x01]), (2, vec![0x01])],
            vec![(3, vec![0x01]), (1, vec![0x01]), (4, vec![]), (1, vec![])],
        ] {
            let m = meta_box(&[hdlr_pict(), iprp(&[boxed(b"ispe", &[0; 12])], &rows)]);
            let h = parse_box_header(&m, 0).unwrap();
            let err = Meta::parse(payload(&m, &h)).unwrap_err();
            assert!(err.to_string().contains("twice"), "{err}");
        }
        let m = meta_box(&[
            hdlr_pict(),
            iprp(&[boxed(b"ispe", &[0; 12])], &[(1, vec![0x80])]),
        ]);
        let h = parse_box_header(&m, 0).unwrap();
        assert!(Meta::parse(payload(&m, &h)).is_err());
    }

    /// A tree past the lookup threshold, built field by field: items
    /// 1..=n, a row per odd item (item 7 in two rows), references of
    /// several types (one box naming a target twice), two groups.
    fn large_tree(n: u32) -> Meta {
        let mut m = Meta::default();
        for id in 1..=n {
            m.items.push(ItemInfo {
                id,
                version: 2,
                flags: 0,
                protection_index: 0,
                item_type: *b"hvc1",
                name: String::new(),
                content_type: None,
                content_encoding: None,
                item_uri_type: None,
            });
            if id % 2 == 1 {
                m.associations.push(ItemPropertyAssociations::new(
                    id,
                    vec![PropertyAssociation {
                        index: 1,
                        essential: id % 4 == 1,
                    }],
                ));
            }
            if id % 3 == 0 {
                m.locations.push(ItemLocation {
                    item_id: id,
                    construction_method: 0,
                    data_reference_index: 0,
                    base_offset: id as u64,
                    extents: Vec::new(),
                });
            }
        }
        m.associations.push(ItemPropertyAssociations::new(
            7,
            vec![PropertyAssociation {
                index: 1,
                essential: false,
            }],
        ));
        m.properties
            .push(RawProperty::new(*b"ispe", None, vec![0; 12], 20));
        m.references
            .push(ItemReference::new(*b"dimg", n, (1..n).collect()));
        m.references.push(ItemReference::new(*b"thmb", 2, vec![n]));
        m.references
            .push(ItemReference::new(*b"cdsc", 3, vec![5, 5, n]));
        m.references
            .push(ItemReference::new(*b"auxl", 4, vec![n, 9]));
        m.references
            .push(ItemReference::new(*b"dimg", n, vec![n + 50]));
        let group = |ty: &[u8; 4], id: u32, ents: Vec<u32>| EntityGroup {
            grouping_type: *ty,
            version: 0,
            flags: 0,
            group_id: id,
            entity_ids: ents,
            payload: Vec::new(),
        };
        m.entity_groups.push(group(b"altr", 900, vec![6, 8]));
        m.entity_groups.push(group(b"ster", 901, vec![8, 10, 8]));
        m
    }

    /// Every lookup answered by walking the vectors.
    #[allow(clippy::type_complexity)]
    fn walked(
        m: &Meta,
        id: u32,
    ) -> (
        Option<u32>,
        Option<u64>,
        Vec<bool>,
        Vec<Vec<u32>>,
        Vec<Vec<u32>>,
        Vec<u32>,
    ) {
        let types: [&FourCc; 4] = [b"dimg", b"thmb", b"cdsc", b"auxl"];
        (
            m.items.iter().find(|i| i.id == id).map(|i| i.id),
            m.locations
                .iter()
                .find(|l| l.item_id == id)
                .map(|l| l.base_offset),
            m.associations
                .iter()
                .filter(|a| a.item_id == id)
                .flat_map(|a| a.entries.iter().map(|e| e.essential))
                .collect(),
            types
                .iter()
                .map(|t| {
                    m.references
                        .iter()
                        .filter(|r| r.from_item_id == id && &&r.reference_type == t)
                        .flat_map(|r| r.to_item_ids.iter().copied())
                        .collect()
                })
                .collect(),
            types
                .iter()
                .map(|t| {
                    m.references
                        .iter()
                        .filter(|r| &&r.reference_type == t && r.to_item_ids.contains(&id))
                        .map(|r| r.from_item_id)
                        .collect()
                })
                .collect(),
            m.entity_groups
                .iter()
                .filter(|g| g.entity_ids.contains(&id))
                .map(|g| g.group_id)
                .collect(),
        )
    }

    /// The same through the public lookups (the tables, on a large tree).
    #[allow(clippy::type_complexity)]
    fn looked_up(
        m: &Meta,
        id: u32,
    ) -> (
        Option<u32>,
        Option<u64>,
        Vec<bool>,
        Vec<Vec<u32>>,
        Vec<Vec<u32>>,
        Vec<u32>,
    ) {
        let types: [&FourCc; 4] = [b"dimg", b"thmb", b"cdsc", b"auxl"];
        let mut groups: Vec<u32> = [b"altr", b"ster"]
            .iter()
            .flat_map(|t| m.groups_containing(id, t))
            .map(|g| g.group_id)
            .collect();
        groups.sort_unstable();
        (
            m.item(id).map(|i| i.id),
            m.location(id).map(|l| l.base_offset),
            m.properties_of(id).iter().map(|p| p.2).collect(),
            types.iter().map(|t| m.references_from(id, t)).collect(),
            types.iter().map(|t| m.references_to(id, t)).collect(),
            groups,
        )
    }

    #[test]
    fn lookups_of_a_large_tree_equal_the_walk() {
        let n = 300;
        let mut m = large_tree(n);
        assert!(m.lookup().is_some(), "a tree this size is indexed");
        for id in 0..=n + 60 {
            assert_eq!(looked_up(&m, id), walked(&m, id), "item {id}");
        }
        assert_eq!(m.properties_of(7).len(), 2, "both rows of item 7");
        assert_eq!(m.associations_of(7).len(), 2);
        assert_eq!(m.display_order().len(), n as usize - 3);
        // A small tree is walked.
        assert!(large_tree(4).lookup().is_none());

        // Records pushed after the tables were built: the lengths no
        // longer match, the walk answers.
        m.references
            .push(ItemReference::new(*b"thmb", 11, vec![12]));
        assert!(m.lookup().is_none());
        assert_eq!(m.references_to(12, b"thmb"), [11]);
        for id in [1, 11, 12, n] {
            assert_eq!(looked_up(&m, id), walked(&m, id), "item {id} after a push");
        }
        m.reindex();
        assert!(m.lookup().is_some());
        assert_eq!(m.references_to(12, b"thmb"), [11]);

        // Ids rewritten in place: an entry is never used for another
        // id (the record at its position is checked), an item or a
        // location is still found by the walk, and `reindex` brings
        // the tables back in line for the rest.
        m.items[4].id = 5000;
        m.locations[0].item_id = 5001;
        m.references[1].from_item_id = 5002;
        m.entity_groups[0].entity_ids[0] = 5003;
        m.associations[0].item_id = 5004;
        assert_eq!(m.item(5000).map(|i| i.id), Some(5000));
        assert!(m.item(5).is_none());
        assert_eq!(m.location(5001).map(|l| l.item_id), Some(5001));
        assert!(m.location(3).is_none());
        assert_eq!(m.references_from(2, b"thmb"), Vec::<u32>::new());
        assert!(m.groups_containing(6, b"altr").is_empty());
        assert!(m.properties_of(1).is_empty());
        m.reindex();
        for id in [1, 2, 3, 5, 6, 8, n, 5000, 5001, 5002, 5003, 5004] {
            assert_eq!(looked_up(&m, id), walked(&m, id), "item {id} after reindex");
        }
        // A clone starts without tables and compares equal.
        let c = m.clone();
        assert_eq!(c, m);
        assert_eq!(looked_up(&c, n), walked(&m, n));
    }

    #[test]
    fn mime_and_uri_infe_tails() {
        let mut b = 5u16.to_be_bytes().to_vec();
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(b"mime");
        b.extend_from_slice(b"\0application/rdf+xml\0\0");
        let infe = full_boxed(b"infe", 2, 0, &b);
        let m = meta_box(&[hdlr_pict(), iinf_v0(&[infe])]);
        let h = parse_box_header(&m, 0).unwrap();
        let meta = Meta::parse(payload(&m, &h)).unwrap();
        assert!(meta.items[0].is_xmp());
        assert_eq!(meta.items[0].content_encoding, None);

        let mut b = 7u32.to_be_bytes().to_vec();
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(b"uri ");
        b.extend_from_slice(b"n\0urn:iptc:std:IIM:3.0:spec\0");
        let infe = full_boxed(b"infe", 3, 1, &b);
        let m = meta_box(&[hdlr_pict(), iinf_v0(&[infe])]);
        let h = parse_box_header(&m, 0).unwrap();
        let meta = Meta::parse(payload(&m, &h)).unwrap();
        assert_eq!(meta.items[0].id, 7);
        assert!(meta.items[0].is_hidden());
        assert_eq!(
            meta.items[0].item_uri_type.as_deref(),
            Some("urn:iptc:std:IIM:3.0:spec")
        );
    }

    #[test]
    fn iloc_versions_and_widths() {
        // v0: no construction method, offset_size 8, length_size 4, base_offset_size 4.
        let mut b = vec![0x84u8, 0x40];
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&3u16.to_be_bytes()); // item id
        b.extend_from_slice(&0u16.to_be_bytes()); // dref
        b.extend_from_slice(&16u32.to_be_bytes()); // base offset
        b.extend_from_slice(&1u16.to_be_bytes()); // extents
        b.extend_from_slice(&32u64.to_be_bytes());
        b.extend_from_slice(&7u32.to_be_bytes());
        let iloc = full_boxed(b"iloc", 0, 0, &b);
        let locs = parse_iloc(payload(&iloc, &parse_box_header(&iloc, 0).unwrap())).unwrap();
        assert_eq!(locs[0].item_id, 3);
        assert_eq!(locs[0].base_offset, 16);
        assert_eq!(locs[0].extents[0].offset, 32);
        assert_eq!(locs[0].extents[0].length, 7);
        assert_eq!(locs[0].declared_size(), 7);

        // v2: 32-bit ids, index_size 4.
        let mut b = vec![0x44u8, 0x04];
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&70000u32.to_be_bytes());
        b.extend_from_slice(&2u16.to_be_bytes()); // cm=2
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u32.to_be_bytes()); // extent_index
        b.extend_from_slice(&5u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        let iloc = full_boxed(b"iloc", 2, 0, &b);
        let locs = parse_iloc(payload(&iloc, &parse_box_header(&iloc, 0).unwrap())).unwrap();
        assert_eq!(locs[0].item_id, 70000);
        assert_eq!(locs[0].construction_method, 2);
        assert_eq!(locs[0].extents[0].index, 1);

        // Bad width nibble.
        let mut b = vec![0x34u8, 0x00];
        b.extend_from_slice(&0u16.to_be_bytes());
        let iloc = full_boxed(b"iloc", 1, 0, &b);
        assert!(parse_iloc(payload(&iloc, &parse_box_header(&iloc, 0).unwrap())).is_err());
    }

    #[test]
    fn iref_v1_and_grpl_and_dref() {
        let mut b = 10u32.to_be_bytes().to_vec();
        b.extend_from_slice(&2u16.to_be_bytes());
        b.extend_from_slice(&11u32.to_be_bytes());
        b.extend_from_slice(&12u32.to_be_bytes());
        let iref = full_boxed(b"iref", 1, 0, &boxed(b"dimg", &b));
        let refs = parse_iref(payload(&iref, &parse_box_header(&iref, 0).unwrap())).unwrap();
        assert_eq!(refs[0].from_item_id, 10);
        assert_eq!(refs[0].to_item_ids, vec![11, 12]);

        let mut g = 1u32.to_be_bytes().to_vec();
        g.extend_from_slice(&2u32.to_be_bytes());
        g.extend_from_slice(&5u32.to_be_bytes());
        g.extend_from_slice(&6u32.to_be_bytes());
        let grpl = boxed(b"grpl", &full_boxed(b"altr", 0, 0, &g));
        let groups = parse_grpl(payload(&grpl, &parse_box_header(&grpl, 0).unwrap())).unwrap();
        assert_eq!(groups[0].grouping_type, *b"altr");
        assert_eq!(groups[0].entity_ids, vec![5, 6]);

        let url = full_boxed(b"url ", 0, 1, &[]);
        let mut d = 1u32.to_be_bytes().to_vec();
        d.extend(url);
        let dinf = boxed(b"dinf", &full_boxed(b"dref", 0, 0, &d));
        let drefs = parse_dinf(payload(&dinf, &parse_box_header(&dinf, 0).unwrap())).unwrap();
        assert!(drefs[0].self_contained);
    }

    #[test]
    fn multiple_ipma_boxes_concatenate_but_same_key_is_rejected() {
        let props = [boxed(b"ispe", &[0; 12]), boxed(b"pixi", &[0; 5])];
        let mut ipco = Vec::new();
        for p in &props {
            ipco.extend_from_slice(p);
        }
        let mut row_a = 1u32.to_be_bytes().to_vec();
        row_a.extend_from_slice(&1u16.to_be_bytes());
        row_a.push(1);
        row_a.push(0x01);
        let mut row_b = 1u32.to_be_bytes().to_vec();
        row_b.extend_from_slice(&1u32.to_be_bytes());
        row_b.push(1);
        row_b.extend_from_slice(&0x8002u16.to_be_bytes());
        let mut body = boxed(b"ipco", &ipco);
        body.extend(full_boxed(b"ipma", 0, 0, &row_a));
        body.extend(full_boxed(b"ipma", 1, 1, &row_b));
        let m = meta_box(&[hdlr_pict(), boxed(b"iprp", &body)]);
        let h = parse_box_header(&m, 0).unwrap();
        let meta = Meta::parse(payload(&m, &h)).unwrap();
        let p = meta.properties_of(1);
        assert_eq!(p.len(), 2);
        assert_eq!(&p[1].1.box_type, b"pixi");
        assert!(p[1].2);

        let mut body = boxed(b"ipco", &ipco);
        body.extend(full_boxed(b"ipma", 0, 0, &row_a));
        body.extend(full_boxed(b"ipma", 0, 0, &row_a));
        let m = meta_box(&[hdlr_pict(), boxed(b"iprp", &body)]);
        let h = parse_box_header(&m, 0).unwrap();
        assert!(Meta::parse(payload(&m, &h)).is_err());
    }
}
