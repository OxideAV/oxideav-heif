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
    fourcc_str, iter_boxes, parse_box_header, parse_full_box, payload, FourCc, Reader,
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
/// Grid derived image item (§6.6.2.3).
pub const ITEM_TYPE_GRID: FourCc = *b"grid";
/// Overlay derived image item (§6.6.2.2).
pub const ITEM_TYPE_IOVL: FourCc = *b"iovl";
/// Identity derived image item (§6.6.2.1).
pub const ITEM_TYPE_IDEN: FourCc = *b"iden";
/// Tone-map derived image item (ISO/IEC 21496-1 gain maps).
pub const ITEM_TYPE_TMAP: FourCc = *b"tmap";
/// Exif metadata item (Annex A.2).
pub const ITEM_TYPE_EXIF: FourCc = *b"Exif";
/// MIME-typed item (XMP = `application/rdf+xml`, Annex A.3).
pub const ITEM_TYPE_MIME: FourCc = *b"mime";
/// URI-typed item (IPTC, Annex A.5).
pub const ITEM_TYPE_URI: FourCc = *b"uri ";

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
pub struct Handler {
    /// `handler_type` (`pict` for HEIF image collections).
    pub handler_type: FourCc,
    /// Human-readable `name` (may be empty).
    pub name: String,
}

/// One `infe` entry (ISO/IEC 14496-12 §8.11.6).
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// HEIF §6.4.2: `(flags & 1) == 1` marks a hidden image item.
    pub fn is_hidden(&self) -> bool {
        self.flags & 1 == 1
    }

    /// `true` for the derived image item types this crate knows.
    pub fn is_derived_image(&self) -> bool {
        matches!(
            self.item_type,
            ITEM_TYPE_GRID | ITEM_TYPE_IOVL | ITEM_TYPE_IDEN | ITEM_TYPE_TMAP
        )
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
pub struct Extent {
    /// `extent_index` (only meaningful for construction method 2; 0
    /// when the box carried `index_size == 0`, which implies 1).
    pub index: u64,
    /// `extent_offset` relative to the data origin.
    pub offset: u64,
    /// `extent_length`; 0 means "to the end of the source".
    pub length: u64,
}

/// One `iloc` item record.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Sum of the explicit extent lengths (0-length extents excluded).
    pub fn declared_size(&self) -> u64 {
        self.extents
            .iter()
            .fold(0u64, |acc, e| acc.saturating_add(e.length))
    }
}

/// One property box from `ipco`, kept raw.
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// One entry of an `ipma` association list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyAssociation {
    /// 1-based index into `ipco`.
    pub index: u16,
    /// `essential` flag.
    pub essential: bool,
}

/// The association list of one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemPropertyAssociations {
    /// `item_ID`.
    pub item_id: u32,
    /// Associations in file order. Index-0 placeholders ("no property")
    /// are dropped at parse time.
    pub entries: Vec<PropertyAssociation>,
}

/// One `SingleItemTypeReferenceBox` from `iref`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemReference {
    /// The reference type (box type of the child box).
    pub reference_type: FourCc,
    /// `from_item_ID`.
    pub from_item_id: u32,
    /// `to_item_ID[]` in order.
    pub to_item_ids: Vec<u32>,
}

/// One `EntityToGroupBox` from `grpl`.
#[derive(Clone, Debug, PartialEq, Eq)]
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
}

/// One `dref` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataReference {
    /// Entry box type (`url ` / `urn `).
    pub entry_type: FourCc,
    /// `(flags & 1) == 1`: the data is in this file.
    pub self_contained: bool,
    /// `location` string for `url ` entries (empty when self-contained).
    pub location: String,
    /// `name` string for `urn ` entries.
    pub name: String,
}

/// Parsed file-level `meta` box.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
    /// `xml ` box payload (UTF-8), when present.
    pub xml: Option<String>,
    /// Box types encountered directly inside `meta`, in file order.
    pub child_box_types: Vec<FourCc>,
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
                    let (props, assocs) = parse_iprp(p, &mut seen_ipma_keys)?;
                    me.properties = props;
                    me.associations = assocs;
                }
                b"idat" => me.idat = Some(p.to_vec()),
                b"grpl" => me.entity_groups = parse_grpl(p)?,
                b"dinf" => me.data_references = parse_dinf(p)?,
                b"ipro" => me.protection_scheme_count = parse_ipro(p)?,
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

    /// Look up an item by id.
    pub fn item(&self, id: u32) -> Option<&ItemInfo> {
        self.items.iter().find(|i| i.id == id)
    }

    /// Look up an item location by id.
    pub fn location(&self, id: u32) -> Option<&ItemLocation> {
        self.locations.iter().find(|l| l.item_id == id)
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
        for a in self.associations.iter().filter(|a| a.item_id == item_id) {
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
        self.associations
            .iter()
            .filter(|a| a.item_id == item_id)
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
        self.references
            .iter()
            .filter(|r| r.from_item_id == from && &r.reference_type == reference_type)
            .flat_map(|r| r.to_item_ids.iter().copied())
            .collect()
    }

    /// Sources of the references of `reference_type` that target `to`,
    /// in file order.
    pub fn references_to(&self, to: u32, reference_type: &FourCc) -> Vec<u32> {
        self.references
            .iter()
            .filter(|r| &r.reference_type == reference_type && r.to_item_ids.contains(&to))
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

    /// Entity groups of `grouping_type` containing `entity_id`.
    pub fn groups_containing(&self, entity_id: u32, grouping_type: &FourCc) -> Vec<&EntityGroup> {
        self.entity_groups
            .iter()
            .filter(|g| &g.grouping_type == grouping_type && g.entity_ids.contains(&entity_id))
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
            )))
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
) -> Result<(Vec<RawProperty>, Vec<ItemPropertyAssociations>)> {
    let mut props = Vec::new();
    let mut assocs = Vec::new();
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
            b"ipma" => assocs.extend(parse_ipma(body, seen_ipma_keys)?),
            _ => {}
        }
    }
    if !saw_ipco {
        return Err(HeifError::invalid("iprp without ipco"));
    }
    Ok((props, assocs))
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

fn parse_ipma(p: &[u8], seen: &mut Vec<(u8, u32)>) -> Result<Vec<ItemPropertyAssociations>> {
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
    let mut out = Vec::with_capacity(entry_count.min(4096));
    let mut last_id: Option<u32> = None;
    for _ in 0..entry_count {
        let item_id = if version == 0 {
            r.u16("ipma item_ID")? as u32
        } else {
            r.u32("ipma item_ID")?
        };
        // §9.3.1: ordered by increasing item_ID, at most one row per item.
        if let Some(prev) = last_id {
            if item_id <= prev {
                return Err(HeifError::invalid(format!(
                    "ipma rows not strictly increasing (item {item_id} after {prev})"
                )));
            }
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
    Ok(out)
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
        out.push(DataReference {
            entry_type: h.box_type,
            self_contained: eflags & 1 == 1,
            location,
            name,
        });
        cursor = h.end();
    }
    Ok(out)
}

fn parse_ipro(p: &[u8]) -> Result<u16> {
    let (_v, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    r.u16("ipro protection_count")
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn ipma_rows_must_increase_and_essential_zero_is_rejected() {
        let m = meta_box(&[
            hdlr_pict(),
            iprp(
                &[boxed(b"ispe", &[0; 12])],
                &[(2, vec![0x01]), (1, vec![0x01])],
            ),
        ]);
        let h = parse_box_header(&m, 0).unwrap();
        assert!(Meta::parse(payload(&m, &h)).is_err());
        let m = meta_box(&[
            hdlr_pict(),
            iprp(&[boxed(b"ispe", &[0; 12])], &[(1, vec![0x80])]),
        ]);
        let h = parse_box_header(&m, 0).unwrap();
        assert!(Meta::parse(payload(&m, &h)).is_err());
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
