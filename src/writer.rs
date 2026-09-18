//! HEIF / MIAF file writer (standalone).
//!
//! [`HeifWriter`] assembles a MIAF-conformant still-image file from
//! already-coded item payloads: coded images (`hvc1` / `av01` / …
//! with their decoder-configuration property), derived images
//! (`grid` / `iovl` / `iden`), auxiliaries (alpha / depth via `auxl`
//! and `auxC`), thumbnails (`thmb`), Exif / XMP metadata items
//! (`cdsc`), ICC / `nclx` colour, and transformative properties. The
//! emitted layout follows MIAF §7.2.1:
//!
//! ```text
//! ftyp                 major brand + compatible brands (auto-selected)
//! meta (v0)
//!   hdlr 'pict'
//!   pitm               primary item
//!   iinf / infe v2|v3  (v3 when an id exceeds 16 bits)
//!   iref v0|v1         dimg / thmb / auxl / cdsc / prem / base
//!   iprp
//!     ipco             de-duplicated property boxes
//!     ipma             one row per item, essential flags kept
//!   iloc v1            cm 0 (mdat) for coded + metadata items,
//!                      cm 1 (idat) for derived items
//!   idat               derived-item bodies
//! mdat                 thumbnails + metadata first, then the primary
//!                      item's data, then everything else (§7.2.1.13)
//! ```
//!
//! [`SequenceWriter`] emits an image-sequence file (`msf1` / `hevc`):
//! a `pict` track with one `hvc1` / `av01` sample entry (`hvcC` /
//! `av1C` + `ccst`) and a flat sample table, plus a `meta` still for
//! the cover image (§7.1 recommends one).
//!
//! Coded payloads come from the caller; with the `registry` feature
//! the `encode` module produces them through the oxideav encoders.

use crate::boxes::write::{boxed, full_boxed, push_box};
use crate::boxes::FourCc;
use crate::derived::{GridDescriptor, OverlayDescriptor};
use crate::error::{HeifError, Result};
use crate::ftyp::{
    BRAND_AVIF, BRAND_AVIS, BRAND_HEIC, BRAND_HEIX, BRAND_HEVC, BRAND_HEVX, BRAND_ISO8, BRAND_MIAF,
    BRAND_MIF1, BRAND_MSF1,
};
use crate::meta::{
    reference, ITEM_TYPE_AV01, ITEM_TYPE_EXIF, ITEM_TYPE_GRID, ITEM_TYPE_HVC1, ITEM_TYPE_IDEN,
    ITEM_TYPE_IOVL, ITEM_TYPE_MIME,
};
use crate::props::{write::property_box, AuxC, Property, AUX_URN_ALPHA, AUX_URN_DEPTH};

#[doc(hidden)]
/// How an item's body is stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemBody {
    /// Coded image data (`mdat`, construction method 0).
    Coded(Vec<u8>),
    /// A `grid` descriptor (`idat`, construction method 1).
    Grid(GridDescriptor),
    /// An `iovl` descriptor (`idat`, construction method 1).
    Overlay(OverlayDescriptor),
    /// An `iden` item (no body).
    Identity,
    /// A metadata item body (Exif block / XMP packet, `mdat`).
    Metadata(Vec<u8>),
}

#[doc(hidden)]
/// One item queued for writing.
#[derive(Clone, Debug)]
pub struct WriterItem {
    /// `item_ID`.
    pub id: u32,
    /// `item_type`.
    pub item_type: FourCc,
    /// `item_name`.
    pub name: String,
    /// `content_type` (for `mime` items).
    pub content_type: Option<String>,
    /// Hidden flag (`infe` flags bit 0).
    pub hidden: bool,
    /// Body.
    pub body: ItemBody,
    /// Properties in association order with their essential flags.
    pub properties: Vec<(Property, bool)>,
}

/// Builder for a still-image / image-collection file.
#[derive(Clone, Debug, Default)]
pub struct HeifWriter {
    items: Vec<WriterItem>,
    references: Vec<(FourCc, u32, Vec<u32>)>,
    entity_groups: Vec<(FourCc, u32, Vec<u32>)>,
    primary: Option<u32>,
    next_id: u32,
    major_brand: Option<FourCc>,
    compatible_brands: Option<Vec<FourCc>>,
}

impl HeifWriter {
    /// Empty writer; item ids are assigned from 1.
    pub fn new() -> Self {
        Self {
            next_id: 1,
            ..Self::default()
        }
    }

    /// Override the brands (otherwise selected from the item types:
    /// `heic` / `heix` for HEVC, `avif` for AV1, `mif1` + `miaf`
    /// always).
    pub fn with_brands(mut self, major: FourCc, compatible: Vec<FourCc>) -> Self {
        self.major_brand = Some(major);
        self.compatible_brands = Some(compatible);
        self
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Add a coded image item; returns its id.
    pub fn add_coded_item(
        &mut self,
        item_type: FourCc,
        data: Vec<u8>,
        properties: Vec<(Property, bool)>,
    ) -> u32 {
        let id = self.alloc_id();
        self.items.push(WriterItem {
            id,
            item_type,
            name: String::new(),
            content_type: None,
            hidden: false,
            body: ItemBody::Coded(data),
            properties,
        });
        id
    }

    /// Add a `grid` derived item over `tiles` (row-major); an `ispe`
    /// of the output size is added automatically when absent.
    pub fn add_grid(
        &mut self,
        desc: GridDescriptor,
        tiles: &[u32],
        mut properties: Vec<(Property, bool)>,
    ) -> Result<u32> {
        if tiles.len() != desc.tile_count() {
            return Err(HeifError::invalid(format!(
                "grid: {} tiles for a {}x{} layout",
                tiles.len(),
                desc.rows,
                desc.columns
            )));
        }
        ensure_ispe(&mut properties, desc.output_width, desc.output_height);
        let id = self.alloc_id();
        self.items.push(WriterItem {
            id,
            item_type: ITEM_TYPE_GRID,
            name: String::new(),
            content_type: None,
            hidden: false,
            body: ItemBody::Grid(desc),
            properties,
        });
        self.references.push((reference::DIMG, id, tiles.to_vec()));
        Ok(id)
    }

    /// Add an `iovl` derived item over `inputs` (bottom-most first).
    pub fn add_overlay(
        &mut self,
        desc: OverlayDescriptor,
        inputs: &[u32],
        mut properties: Vec<(Property, bool)>,
    ) -> Result<u32> {
        if inputs.len() != desc.offsets.len() {
            return Err(HeifError::invalid(format!(
                "iovl: {} inputs for {} offsets",
                inputs.len(),
                desc.offsets.len()
            )));
        }
        ensure_ispe(&mut properties, desc.output_width, desc.output_height);
        let id = self.alloc_id();
        self.items.push(WriterItem {
            id,
            item_type: ITEM_TYPE_IOVL,
            name: String::new(),
            content_type: None,
            hidden: false,
            body: ItemBody::Overlay(desc),
            properties,
        });
        self.references.push((reference::DIMG, id, inputs.to_vec()));
        Ok(id)
    }

    /// Add an `iden` derived item over `input` carrying `properties`
    /// (typically `ispe` + transformative properties).
    pub fn add_identity(&mut self, input: u32, properties: Vec<(Property, bool)>) -> u32 {
        let id = self.alloc_id();
        self.items.push(WriterItem {
            id,
            item_type: ITEM_TYPE_IDEN,
            name: String::new(),
            content_type: None,
            hidden: false,
            body: ItemBody::Identity,
            properties,
        });
        self.references.push((reference::DIMG, id, vec![input]));
        id
    }

    /// Add a thumbnail (a coded item) of `master`.
    pub fn add_thumbnail(
        &mut self,
        master: u32,
        item_type: FourCc,
        data: Vec<u8>,
        properties: Vec<(Property, bool)>,
    ) -> u32 {
        let id = self.add_coded_item(item_type, data, properties);
        self.references.push((reference::THMB, id, vec![master]));
        id
    }

    /// Add an alpha auxiliary (a coded item) of `master`. An `auxC`
    /// with the codec-independent alpha URN is added when `properties`
    /// carries none; `premultiplied` adds the `prem` reference.
    pub fn add_alpha(
        &mut self,
        master: u32,
        item_type: FourCc,
        data: Vec<u8>,
        mut properties: Vec<(Property, bool)>,
        premultiplied: bool,
    ) -> u32 {
        if !properties
            .iter()
            .any(|(p, _)| matches!(p, Property::AuxC(_)))
        {
            properties.push((
                Property::AuxC(AuxC {
                    aux_type: AUX_URN_ALPHA.into(),
                    aux_subtype: Vec::new(),
                }),
                false,
            ));
        }
        let id = self.add_coded_item(item_type, data, properties);
        self.items.last_mut().expect("just pushed").hidden = true;
        self.references.push((reference::AUXL, id, vec![master]));
        if premultiplied {
            self.references.push((reference::PREM, master, vec![id]));
        }
        id
    }

    /// Add a depth-map auxiliary (a coded item) of `master`.
    pub fn add_depth(
        &mut self,
        master: u32,
        item_type: FourCc,
        data: Vec<u8>,
        mut properties: Vec<(Property, bool)>,
    ) -> u32 {
        if !properties
            .iter()
            .any(|(p, _)| matches!(p, Property::AuxC(_)))
        {
            properties.push((
                Property::AuxC(AuxC {
                    aux_type: AUX_URN_DEPTH.into(),
                    aux_subtype: Vec::new(),
                }),
                false,
            ));
        }
        let id = self.add_coded_item(item_type, data, properties);
        self.items.last_mut().expect("just pushed").hidden = true;
        self.references.push((reference::AUXL, id, vec![master]));
        id
    }

    /// Add an Exif metadata item describing `image`. `tiff` is the
    /// Exif payload starting at the TIFF header; the HEIF offset word
    /// (Annex A.2.1) is prepended.
    pub fn add_exif(&mut self, image: u32, tiff: &[u8]) -> u32 {
        let mut body = 0u32.to_be_bytes().to_vec();
        body.extend_from_slice(tiff);
        let id = self.alloc_id();
        self.items.push(WriterItem {
            id,
            item_type: ITEM_TYPE_EXIF,
            name: String::new(),
            content_type: None,
            hidden: false,
            body: ItemBody::Metadata(body),
            properties: Vec::new(),
        });
        self.references.push((reference::CDSC, id, vec![image]));
        id
    }

    /// Add an XMP metadata item (`mime` / `application/rdf+xml`).
    pub fn add_xmp(&mut self, image: u32, xmp: &str) -> u32 {
        let id = self.alloc_id();
        self.items.push(WriterItem {
            id,
            item_type: ITEM_TYPE_MIME,
            name: String::new(),
            content_type: Some("application/rdf+xml".into()),
            hidden: false,
            body: ItemBody::Metadata(xmp.as_bytes().to_vec()),
            properties: Vec::new(),
        });
        self.references.push((reference::CDSC, id, vec![image]));
        id
    }

    /// Add an arbitrary item reference.
    pub fn add_reference(&mut self, reference_type: FourCc, from: u32, to: Vec<u32>) {
        self.references.push((reference_type, from, to));
    }

    /// Add an entity group (`altr`, `brst`, …).
    pub fn add_entity_group(&mut self, grouping_type: FourCc, group_id: u32, entities: Vec<u32>) {
        self.entity_groups.push((grouping_type, group_id, entities));
    }

    /// Mark an item hidden (`infe` flags bit 0).
    pub fn set_hidden(&mut self, id: u32, hidden: bool) {
        if let Some(it) = self.items.iter_mut().find(|i| i.id == id) {
            it.hidden = hidden;
        }
    }

    /// Set the primary item.
    pub fn set_primary(&mut self, id: u32) {
        self.primary = Some(id);
    }

    /// The items queued so far.
    pub fn items(&self) -> &[WriterItem] {
        &self.items
    }

    fn selected_brands(&self) -> (FourCc, Vec<FourCc>) {
        if let (Some(m), Some(c)) = (self.major_brand, &self.compatible_brands) {
            return (m, c.clone());
        }
        let mut compat = vec![BRAND_MIF1];
        let mut major = BRAND_MIF1;
        let has = |t: &FourCc| self.items.iter().any(|i| &i.item_type == t);
        if has(&ITEM_TYPE_HVC1) {
            // heix when any HEVC item is above 8 bits or not 4:2:0.
            let heix = self.items.iter().any(|i| {
                i.properties.iter().any(|(p, _)| match p {
                    Property::HvcC(h) => h.bit_depth_luma() > 8 || h.chroma_format_idc != 1,
                    _ => false,
                })
            });
            major = if heix { BRAND_HEIX } else { BRAND_HEIC };
            compat.push(major);
        }
        if has(&ITEM_TYPE_AV01) {
            if major == BRAND_MIF1 {
                major = BRAND_AVIF;
            }
            compat.push(BRAND_AVIF);
        }
        compat.push(BRAND_MIAF);
        (major, compat)
    }

    /// Serialize the file.
    pub fn write_to_vec(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let primary = self.primary.expect("validated");
        let large_ids = self.items.iter().any(|i| i.id > 0xffff)
            || self
                .references
                .iter()
                .any(|(_, f, t)| *f > 0xffff || t.iter().any(|x| *x > 0xffff));

        // ── ipco / ipma
        let mut ipco_boxes: Vec<Vec<u8>> = Vec::new();
        let mut ipma_rows: Vec<(u32, Vec<(u16, bool)>)> = Vec::new();
        for it in &self.items {
            let mut row = Vec::new();
            for (p, essential) in &it.properties {
                let bytes = property_box(p);
                let idx = match ipco_boxes.iter().position(|b| *b == bytes) {
                    Some(i) => i,
                    None => {
                        ipco_boxes.push(bytes);
                        ipco_boxes.len() - 1
                    }
                };
                row.push((idx as u16 + 1, *essential));
            }
            if !row.is_empty() {
                ipma_rows.push((it.id, row));
            }
        }
        if ipco_boxes.len() > 0x7fff {
            return Err(HeifError::exhausted("more than 32767 properties"));
        }
        ipma_rows.sort_by_key(|(id, _)| *id);
        let large_index = ipco_boxes.len() > 127;
        let mut ipco = Vec::new();
        for b in &ipco_boxes {
            ipco.extend_from_slice(b);
        }
        let mut ipma = (ipma_rows.len() as u32).to_be_bytes().to_vec();
        for (id, row) in &ipma_rows {
            if large_ids {
                ipma.extend_from_slice(&id.to_be_bytes());
            } else {
                ipma.extend_from_slice(&(*id as u16).to_be_bytes());
            }
            ipma.push(row.len() as u8);
            for (idx, essential) in row {
                if large_index {
                    ipma.extend_from_slice(&(((*essential as u16) << 15) | idx).to_be_bytes());
                } else {
                    ipma.push(((*essential as u8) << 7) | (*idx as u8));
                }
            }
        }
        let mut iprp_body = boxed(b"ipco", &ipco);
        iprp_body.extend(full_boxed(
            b"ipma",
            large_ids as u8,
            large_index as u32,
            &ipma,
        ));
        let iprp = boxed(b"iprp", &iprp_body);

        // ── hdlr / pitm / iinf / iref / grpl
        let mut hdlr_body = vec![0u8; 4];
        hdlr_body.extend_from_slice(b"pict");
        hdlr_body.extend_from_slice(&[0u8; 12]);
        hdlr_body.push(0);
        let hdlr = full_boxed(b"hdlr", 0, 0, &hdlr_body);
        let pitm = if large_ids {
            full_boxed(b"pitm", 1, 0, &primary.to_be_bytes())
        } else {
            full_boxed(b"pitm", 0, 0, &(primary as u16).to_be_bytes())
        };
        let mut infes = Vec::new();
        for it in &self.items {
            let mut b = Vec::new();
            let version = if large_ids { 3 } else { 2 };
            if large_ids {
                b.extend_from_slice(&it.id.to_be_bytes());
            } else {
                b.extend_from_slice(&(it.id as u16).to_be_bytes());
            }
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&it.item_type);
            b.extend_from_slice(it.name.as_bytes());
            b.push(0);
            if it.item_type == ITEM_TYPE_MIME {
                b.extend_from_slice(it.content_type.as_deref().unwrap_or("").as_bytes());
                b.push(0);
            }
            infes.push(full_boxed(b"infe", version, it.hidden as u32, &b));
        }
        let mut iinf_body = if large_ids {
            (self.items.len() as u32).to_be_bytes().to_vec()
        } else {
            (self.items.len() as u16).to_be_bytes().to_vec()
        };
        for i in &infes {
            iinf_body.extend_from_slice(i);
        }
        let iinf = full_boxed(b"iinf", large_ids as u8, 0, &iinf_body);
        let iref = if self.references.is_empty() {
            Vec::new()
        } else {
            let mut body = Vec::new();
            for (t, from, to) in &self.references {
                let mut b = Vec::new();
                if large_ids {
                    b.extend_from_slice(&from.to_be_bytes());
                } else {
                    b.extend_from_slice(&(*from as u16).to_be_bytes());
                }
                b.extend_from_slice(&(to.len() as u16).to_be_bytes());
                for x in to {
                    if large_ids {
                        b.extend_from_slice(&x.to_be_bytes());
                    } else {
                        b.extend_from_slice(&(*x as u16).to_be_bytes());
                    }
                }
                body.extend(boxed(t, &b));
            }
            full_boxed(b"iref", large_ids as u8, 0, &body)
        };
        let grpl = if self.entity_groups.is_empty() {
            Vec::new()
        } else {
            let mut body = Vec::new();
            for (t, gid, ents) in &self.entity_groups {
                let mut b = gid.to_be_bytes().to_vec();
                b.extend_from_slice(&(ents.len() as u32).to_be_bytes());
                for e in ents {
                    b.extend_from_slice(&e.to_be_bytes());
                }
                body.extend(full_boxed(t, 0, 0, &b));
            }
            boxed(b"grpl", &body)
        };

        // ── idat + mdat bodies, in the MIAF §7.2.1.13 order.
        let mut idat = Vec::new();
        let mut idat_spans: Vec<(u32, u64, u64)> = Vec::new();
        let mut mdat = Vec::new();
        let mut mdat_spans: Vec<(u32, u64, u64)> = Vec::new();
        let is_thumb_or_meta = |it: &WriterItem| {
            matches!(it.body, ItemBody::Metadata(_))
                || self
                    .references
                    .iter()
                    .any(|(t, f, _)| t == &reference::THMB && *f == it.id)
        };
        let primary_chain = self.chain_of(primary);
        let mut order: Vec<&WriterItem> = Vec::with_capacity(self.items.len());
        order.extend(self.items.iter().filter(|i| is_thumb_or_meta(i)));
        order.extend(
            self.items
                .iter()
                .filter(|i| !is_thumb_or_meta(i) && primary_chain.contains(&i.id)),
        );
        order.extend(
            self.items
                .iter()
                .filter(|i| !is_thumb_or_meta(i) && !primary_chain.contains(&i.id)),
        );
        for it in order {
            match &it.body {
                ItemBody::Coded(d) | ItemBody::Metadata(d) => {
                    mdat_spans.push((it.id, mdat.len() as u64, d.len() as u64));
                    mdat.extend_from_slice(d);
                }
                ItemBody::Grid(g) => {
                    let b = g.to_bytes();
                    idat_spans.push((it.id, idat.len() as u64, b.len() as u64));
                    idat.extend_from_slice(&b);
                }
                ItemBody::Overlay(o) => {
                    let b = o.to_bytes();
                    idat_spans.push((it.id, idat.len() as u64, b.len() as u64));
                    idat.extend_from_slice(&b);
                }
                ItemBody::Identity => {}
            }
        }
        let idat_box = if idat.is_empty() {
            Vec::new()
        } else {
            boxed(b"idat", &idat)
        };

        // ── iloc (fixed widths ⇒ size independent of the mdat offset).
        let iloc_for = |mdat_base: u64| -> Vec<u8> {
            let offset_size = 8u8;
            let length_size = 8u8;
            let mut b = vec![(offset_size << 4) | length_size, 0u8];
            let count = idat_spans.len() + mdat_spans.len();
            if large_ids {
                b.extend_from_slice(&(count as u32).to_be_bytes());
            } else {
                b.extend_from_slice(&(count as u16).to_be_bytes());
            }
            let mut entries: Vec<(u32, u8, u64, u64)> = Vec::new();
            entries.extend(
                mdat_spans
                    .iter()
                    .map(|(id, o, l)| (*id, 0u8, mdat_base + o, *l)),
            );
            entries.extend(idat_spans.iter().map(|(id, o, l)| (*id, 1u8, *o, *l)));
            entries.sort_by_key(|e| e.0);
            for (id, cm, off, len) in entries {
                if large_ids {
                    b.extend_from_slice(&id.to_be_bytes());
                } else {
                    b.extend_from_slice(&(id as u16).to_be_bytes());
                }
                b.extend_from_slice(&(cm as u16).to_be_bytes());
                b.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
                b.extend_from_slice(&1u16.to_be_bytes()); // extent_count
                b.extend_from_slice(&off.to_be_bytes());
                b.extend_from_slice(&len.to_be_bytes());
            }
            full_boxed(b"iloc", if large_ids { 2 } else { 1 }, 0, &b)
        };
        let meta_with = |iloc: &[u8]| -> Vec<u8> {
            let mut body = vec![0u8; 4];
            body.extend_from_slice(&hdlr);
            body.extend_from_slice(&pitm);
            body.extend_from_slice(&iloc_placeholder_or(iloc));
            body.extend_from_slice(&iinf);
            body.extend_from_slice(&iref);
            body.extend_from_slice(&iprp);
            body.extend_from_slice(&grpl);
            body.extend_from_slice(&idat_box);
            boxed(b"meta", &body)
        };
        let (major, compat) = self.selected_brands();
        let ftyp = crate::ftyp::FileType {
            box_type: *b"ftyp",
            major_brand: major,
            minor_version: 0,
            compatible_brands: compat,
        }
        .to_box();
        let meta_len = meta_with(&iloc_for(0)).len() as u64;
        let mdat_header = if mdat.len() as u64 + 8 > u32::MAX as u64 {
            16
        } else {
            8
        };
        let mdat_base = ftyp.len() as u64 + meta_len + mdat_header;
        let meta = meta_with(&iloc_for(mdat_base));
        debug_assert_eq!(meta.len() as u64, meta_len);
        let mut out = ftyp;
        out.extend_from_slice(&meta);
        push_box(&mut out, b"mdat", &mdat);
        Ok(out)
    }

    /// Items reachable from `id` through `dimg` / `auxl` (as targets).
    fn chain_of(&self, id: u32) -> Vec<u32> {
        let mut out = vec![id];
        let mut i = 0;
        while i < out.len() {
            let cur = out[i];
            for (t, f, to) in &self.references {
                if t == &reference::DIMG && *f == cur {
                    for x in to {
                        if !out.contains(x) {
                            out.push(*x);
                        }
                    }
                }
                if t == &reference::AUXL && to.contains(&cur) && !out.contains(f) {
                    out.push(*f);
                }
            }
            i += 1;
        }
        out
    }

    fn validate(&self) -> Result<()> {
        let primary = self
            .primary
            .ok_or_else(|| HeifError::invalid("writer: no primary item set"))?;
        if !self.items.iter().any(|i| i.id == primary) {
            return Err(HeifError::invalid(format!(
                "writer: primary item {primary} was not added"
            )));
        }
        let mut ids: Vec<u32> = self.items.iter().map(|i| i.id).collect();
        ids.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(HeifError::invalid("writer: duplicate item id"));
        }
        for (t, f, to) in &self.references {
            for x in std::iter::once(f).chain(to) {
                if !ids.contains(x) {
                    return Err(HeifError::invalid(format!(
                        "writer: reference '{}' names item {x} which was not added",
                        crate::boxes::fourcc_str(t)
                    )));
                }
            }
        }
        for it in &self.items {
            let is_image = it.item_type != ITEM_TYPE_EXIF && it.item_type != ITEM_TYPE_MIME;
            if is_image
                && !it
                    .properties
                    .iter()
                    .any(|(p, _)| matches!(p, Property::Ispe(_)))
            {
                return Err(HeifError::invalid(format!(
                    "writer: image item {} has no ispe (HEIF §6.5.3 requires one)",
                    it.id
                )));
            }
            if matches!(it.body, ItemBody::Coded(_))
                && !it.properties.iter().any(|(p, _)| p.is_decoder_config())
            {
                return Err(HeifError::invalid(format!(
                    "writer: coded item {} has no decoder configuration property",
                    it.id
                )));
            }
        }
        Ok(())
    }
}

fn iloc_placeholder_or(iloc: &[u8]) -> Vec<u8> {
    iloc.to_vec()
}

fn ensure_ispe(props: &mut Vec<(Property, bool)>, w: u32, h: u32) {
    if !props.iter().any(|(p, _)| matches!(p, Property::Ispe(_))) {
        props.insert(
            0,
            (
                Property::Ispe(crate::props::Ispe {
                    width: w,
                    height: h,
                }),
                false,
            ),
        );
    }
}

#[doc(hidden)]
/// One sample queued for an image-sequence track.
#[derive(Clone, Debug)]
pub struct SequenceSample {
    /// Coded sample bytes (length-prefixed NAL units / an AV1 TU).
    pub data: Vec<u8>,
    /// Duration in the media timescale.
    pub duration: u32,
    /// Sync sample.
    pub sync: bool,
}

/// Builder for an image-sequence file (`msf1`).
#[derive(Clone, Debug)]
pub struct SequenceWriter {
    /// Media timescale.
    pub timescale: u32,
    /// Sample entry type (`hvc1` / `av01`).
    pub entry_type: FourCc,
    /// Decoder configuration property (`HvcC` / `Av1C`) for the entry.
    pub config: Property,
    /// Coded picture size.
    pub width: u16,
    /// Coded picture size.
    pub height: u16,
    /// Extra sample-entry children (`colr`, `clap`, `pasp`, …).
    pub entry_properties: Vec<Property>,
    /// The samples.
    pub samples: Vec<SequenceSample>,
    /// Optional still image for the file-level `meta` (cover image).
    pub still: Option<HeifWriter>,
    /// `ccst` fields: `(all_ref_pics_intra, intra_pred_used, max_ref_per_pic)`.
    pub coding_constraints: (bool, bool, u8),
}

impl SequenceWriter {
    /// New sequence writer; `config` must be an `HvcC` or `Av1C` property.
    pub fn new(
        entry_type: FourCc,
        config: Property,
        width: u16,
        height: u16,
        timescale: u32,
    ) -> Self {
        Self {
            timescale,
            entry_type,
            config,
            width,
            height,
            entry_properties: Vec::new(),
            samples: Vec::new(),
            still: None,
            coding_constraints: (true, true, 15),
        }
    }

    /// Queue a sample.
    pub fn push_sample(&mut self, data: Vec<u8>, duration: u32, sync: bool) {
        self.samples.push(SequenceSample {
            data,
            duration,
            sync,
        });
    }

    /// Serialize the file.
    pub fn write_to_vec(&self) -> Result<Vec<u8>> {
        if self.samples.is_empty() {
            return Err(HeifError::invalid("sequence writer: no samples"));
        }
        if !self.config.is_decoder_config() {
            return Err(HeifError::invalid(
                "sequence writer: config is not a decoder configuration",
            ));
        }
        let ts = self.timescale.max(1);
        let total: u64 = self.samples.iter().map(|s| s.duration as u64).sum();
        // ftyp
        let codec_brand = match &self.entry_type {
            b"hvc1" | b"hev1" => match &self.config {
                Property::HvcC(h) if h.bit_depth_luma() > 8 || h.chroma_format_idc != 1 => {
                    BRAND_HEVX
                }
                _ => BRAND_HEVC,
            },
            b"av01" => BRAND_AVIS,
            _ => BRAND_MSF1,
        };
        let mut compat = vec![BRAND_MSF1, codec_brand, BRAND_ISO8, BRAND_MIAF];
        if self.still.is_some() {
            compat.push(BRAND_MIF1);
        }
        let ftyp = crate::ftyp::FileType {
            box_type: *b"ftyp",
            major_brand: codec_brand,
            minor_version: 0,
            compatible_brands: compat,
        }
        .to_box();
        // Optional still meta (a HeifWriter file minus its ftyp/mdat).
        let (still_meta, still_mdat) = match &self.still {
            Some(w) => {
                let bytes = w.write_to_vec()?;
                let f = crate::file::HeifFile::parse(&bytes)?;
                let meta_h = f
                    .top_level
                    .iter()
                    .find(|h| &h.box_type == b"meta")
                    .cloned()
                    .expect("writer emits meta");
                let mdat_h = f
                    .top_level
                    .iter()
                    .find(|h| &h.box_type == b"mdat")
                    .cloned()
                    .expect("writer emits mdat");
                (
                    bytes[meta_h.start..meta_h.end()].to_vec(),
                    (
                        bytes[mdat_h.payload_start..mdat_h.end()].to_vec(),
                        mdat_h.start as u64,
                    ),
                )
            }
            None => (Vec::new(), (Vec::new(), 0)),
        };
        // moov
        let mvhd = {
            let mut b = vec![0u8; 8];
            b.extend_from_slice(&ts.to_be_bytes());
            b.extend_from_slice(&(total.min(u32::MAX as u64) as u32).to_be_bytes());
            b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate
            b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
            b.extend_from_slice(&[0u8; 10]);
            for m in [0x10000u32, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000] {
                b.extend_from_slice(&m.to_be_bytes());
            }
            b.extend_from_slice(&[0u8; 24]);
            b.extend_from_slice(&2u32.to_be_bytes()); // next_track_ID
            full_boxed(b"mvhd", 0, 0, &b)
        };
        let tkhd = {
            let mut b = vec![0u8; 8];
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&[0u8; 4]);
            b.extend_from_slice(&(total.min(u32::MAX as u64) as u32).to_be_bytes());
            b.extend_from_slice(&[0u8; 8]);
            b.extend_from_slice(&[0u8; 8]); // layer, alternate_group, volume, reserved
            for m in [0x10000u32, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000] {
                b.extend_from_slice(&m.to_be_bytes());
            }
            b.extend_from_slice(&((self.width as u32) << 16).to_be_bytes());
            b.extend_from_slice(&((self.height as u32) << 16).to_be_bytes());
            full_boxed(b"tkhd", 0, 3, &b)
        };
        let mdhd = {
            let mut b = vec![0u8; 8];
            b.extend_from_slice(&ts.to_be_bytes());
            b.extend_from_slice(&(total.min(u32::MAX as u64) as u32).to_be_bytes());
            b.extend_from_slice(&0x55c4u16.to_be_bytes()); // 'und'
            b.extend_from_slice(&0u16.to_be_bytes());
            full_boxed(b"mdhd", 0, 0, &b)
        };
        let hdlr = {
            let mut b = vec![0u8; 4];
            b.extend_from_slice(b"pict");
            b.extend_from_slice(&[0u8; 12]);
            b.push(0);
            full_boxed(b"hdlr", 0, 0, &b)
        };
        let vmhd = full_boxed(b"vmhd", 0, 1, &[0u8; 8]);
        let dinf = {
            let url = full_boxed(b"url ", 0, 1, &[]);
            let mut d = 1u32.to_be_bytes().to_vec();
            d.extend(url);
            boxed(b"dinf", &full_boxed(b"dref", 0, 0, &d))
        };
        let stsd = {
            let mut e = vec![0u8; 6];
            e.extend_from_slice(&1u16.to_be_bytes());
            e.extend_from_slice(&[0u8; 16]);
            e.extend_from_slice(&self.width.to_be_bytes());
            e.extend_from_slice(&self.height.to_be_bytes());
            e.extend_from_slice(&0x0048_0000u32.to_be_bytes());
            e.extend_from_slice(&0x0048_0000u32.to_be_bytes());
            e.extend_from_slice(&[0u8; 4]);
            e.extend_from_slice(&1u16.to_be_bytes());
            e.extend_from_slice(&[0u8; 32]);
            e.extend_from_slice(&0x0018u16.to_be_bytes());
            e.extend_from_slice(&0xffffu16.to_be_bytes());
            e.extend(property_box(&self.config));
            let (a, i, m) = self.coding_constraints;
            let w = ((a as u32) << 31) | ((i as u32) << 30) | (((m & 0x0f) as u32) << 26);
            e.extend(full_boxed(b"ccst", 0, 0, &w.to_be_bytes()));
            for p in &self.entry_properties {
                e.extend(property_box(p));
            }
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend(boxed(&self.entry_type, &e));
            full_boxed(b"stsd", 0, 0, &b)
        };
        let stts = {
            let mut runs: Vec<(u32, u32)> = Vec::new();
            for s in &self.samples {
                match runs.last_mut() {
                    Some((n, d)) if *d == s.duration => *n += 1,
                    _ => runs.push((1, s.duration)),
                }
            }
            let mut b = (runs.len() as u32).to_be_bytes().to_vec();
            for (n, d) in runs {
                b.extend_from_slice(&n.to_be_bytes());
                b.extend_from_slice(&d.to_be_bytes());
            }
            full_boxed(b"stts", 0, 0, &b)
        };
        let stsc = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(&(self.samples.len() as u32).to_be_bytes());
            b.extend_from_slice(&1u32.to_be_bytes());
            full_boxed(b"stsc", 0, 0, &b)
        };
        let stsz = {
            let mut b = 0u32.to_be_bytes().to_vec();
            b.extend_from_slice(&(self.samples.len() as u32).to_be_bytes());
            for s in &self.samples {
                b.extend_from_slice(&(s.data.len() as u32).to_be_bytes());
            }
            full_boxed(b"stsz", 0, 0, &b)
        };
        let stss = if self.samples.iter().all(|s| s.sync) {
            Vec::new()
        } else {
            let syncs: Vec<u32> = self
                .samples
                .iter()
                .enumerate()
                .filter(|(_, s)| s.sync)
                .map(|(i, _)| i as u32 + 1)
                .collect();
            let mut b = (syncs.len() as u32).to_be_bytes().to_vec();
            for s in syncs {
                b.extend_from_slice(&s.to_be_bytes());
            }
            full_boxed(b"stss", 0, 0, &b)
        };
        let co64_for = |base: u64| -> Vec<u8> {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&base.to_be_bytes());
            full_boxed(b"co64", 0, 0, &b)
        };
        let moov_for = |base: u64| -> Vec<u8> {
            let mut stbl = Vec::new();
            stbl.extend_from_slice(&stsd);
            stbl.extend_from_slice(&stts);
            stbl.extend_from_slice(&stsc);
            stbl.extend_from_slice(&stsz);
            stbl.extend_from_slice(&co64_for(base));
            stbl.extend_from_slice(&stss);
            let mut minf = Vec::new();
            minf.extend_from_slice(&vmhd);
            minf.extend_from_slice(&dinf);
            minf.extend(boxed(b"stbl", &stbl));
            let mut mdia = Vec::new();
            mdia.extend_from_slice(&mdhd);
            mdia.extend_from_slice(&hdlr);
            mdia.extend(boxed(b"minf", &minf));
            let mut trak = Vec::new();
            trak.extend_from_slice(&tkhd);
            trak.extend(boxed(b"mdia", &mdia));
            let mut moov = Vec::new();
            moov.extend_from_slice(&mvhd);
            moov.extend(boxed(b"trak", &trak));
            boxed(b"moov", &moov)
        };
        // Layout: ftyp, moov, [meta], mdat(still data + samples).
        let moov_len = moov_for(0).len() as u64;
        let mut still_meta = still_meta;
        let (still_mdat_bytes, still_mdat_old_base) = still_mdat;
        let mdat_start = ftyp.len() as u64 + moov_len + still_meta.len() as u64;
        let mdat_payload = mdat_start + 8;
        if !still_meta.is_empty() {
            // Relocate the still's iloc offsets: they were written for
            // an mdat at `still_mdat_old_base + 8`; the same payload now
            // starts at `mdat_payload`.
            let delta = mdat_payload as i128 - (still_mdat_old_base + 8) as i128;
            relocate_iloc(&mut still_meta, delta)?;
        }
        let samples_base = mdat_payload + still_mdat_bytes.len() as u64;
        let mut out = ftyp;
        out.extend(moov_for(samples_base));
        out.extend_from_slice(&still_meta);
        let mut mdat = still_mdat_bytes;
        for s in &self.samples {
            mdat.extend_from_slice(&s.data);
        }
        push_box(&mut out, b"mdat", &mdat);
        Ok(out)
    }
}

/// Shift every construction-method-0 extent offset of the `iloc` inside
/// a serialized `meta` box by `delta`. The writer always emits `iloc`
/// v1 with 8-byte offsets, so the patch is in place.
fn relocate_iloc(meta: &mut [u8], delta: i128) -> Result<()> {
    let body_start = 8 + 4; // box header + FullBox
    let mut cursor = body_start;
    while cursor + 8 <= meta.len() {
        let size = u32::from_be_bytes([
            meta[cursor],
            meta[cursor + 1],
            meta[cursor + 2],
            meta[cursor + 3],
        ]) as usize;
        if size < 8 || cursor + size > meta.len() {
            return Err(HeifError::invalid("relocate: malformed meta child"));
        }
        if &meta[cursor + 4..cursor + 8] == b"iloc" {
            let b = &mut meta[cursor + 8..cursor + size];
            let version = b[0];
            let mut p = 4usize;
            let offset_size = (b[p] >> 4) as usize;
            let length_size = (b[p] & 0x0f) as usize;
            let base_offset_size = (b[p + 1] >> 4) as usize;
            let index_size = if version >= 1 {
                (b[p + 1] & 0x0f) as usize
            } else {
                0
            };
            p += 2;
            let count = if version < 2 {
                let c = u16::from_be_bytes([b[p], b[p + 1]]) as usize;
                p += 2;
                c
            } else {
                let c = u32::from_be_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]) as usize;
                p += 4;
                c
            };
            for _ in 0..count {
                p += if version < 2 { 2 } else { 4 };
                let cm = if version >= 1 {
                    let w = u16::from_be_bytes([b[p], b[p + 1]]);
                    p += 2;
                    (w & 0x0f) as u8
                } else {
                    0
                };
                p += 2; // data_reference_index
                p += base_offset_size;
                let extents = u16::from_be_bytes([b[p], b[p + 1]]) as usize;
                p += 2;
                for _ in 0..extents {
                    p += index_size;
                    if cm == 0 && offset_size == 8 {
                        let mut o = [0u8; 8];
                        o.copy_from_slice(&b[p..p + 8]);
                        let v = (u64::from_be_bytes(o) as i128 + delta) as u64;
                        b[p..p + 8].copy_from_slice(&v.to_be_bytes());
                    } else if cm == 0 {
                        return Err(HeifError::unsupported(
                            "relocate: iloc offsets are not 8 bytes",
                        ));
                    }
                    p += offset_size + length_size;
                }
            }
            return Ok(());
        }
        cursor += size;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::HeifFile;
    use crate::hvcc::HevcConfig;
    use crate::props::{Colr, Ispe, Pixi};

    fn hvcc() -> HevcConfig {
        let raw = crate::hvcc::tests::sample_record();
        HevcConfig::parse(&raw).unwrap()
    }

    fn std_props(w: u32, h: u32) -> Vec<(Property, bool)> {
        vec![
            (Property::HvcC(hvcc()), true),
            (
                Property::Ispe(Ispe {
                    width: w,
                    height: h,
                }),
                false,
            ),
            (
                Property::Pixi(Pixi {
                    bits_per_channel: vec![8, 8, 8],
                }),
                false,
            ),
            (Property::Colr(Colr::MIAF_DEFAULT), false),
        ]
    }

    #[test]
    fn writes_a_collection_that_parses_and_is_miaf_conformant() {
        let mut w = HeifWriter::new();
        let tiles: Vec<u32> = (0..4)
            .map(|i| w.add_coded_item(ITEM_TYPE_HVC1, vec![i as u8; 16], std_props(64, 64)))
            .collect();
        let grid = w
            .add_grid(
                GridDescriptor {
                    rows: 2,
                    columns: 2,
                    output_width: 128,
                    output_height: 128,
                },
                &tiles,
                vec![(Property::Colr(Colr::MIAF_DEFAULT), false)],
            )
            .unwrap();
        let thumb = w.add_thumbnail(grid, ITEM_TYPE_HVC1, vec![9; 8], std_props(64, 64));
        let alpha = w.add_alpha(grid, ITEM_TYPE_HVC1, vec![7; 8], std_props(128, 128), false);
        let exif = w.add_exif(grid, b"II*\0\x08\0\0\0");
        let xmp = w.add_xmp(grid, "<x:xmpmeta/>");
        w.set_primary(grid);
        let bytes = w.write_to_vec().unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        assert!(f.file_type.has_brand(&BRAND_HEIC));
        assert!(f.file_type.has_brand(&BRAND_MIF1));
        assert!(f.file_type.has_brand(&BRAND_MIAF));
        let meta = f.meta().unwrap();
        assert_eq!(meta.primary_item_id, Some(grid));
        assert_eq!(meta.items.len(), 9);
        assert_eq!(meta.derivation_inputs(grid), tiles);
        assert_eq!(meta.thumbnails_of(grid), vec![thumb]);
        assert_eq!(meta.auxiliaries_of(grid), vec![alpha]);
        assert_eq!(meta.metadata_of(grid), vec![exif, xmp]);
        assert!(meta.item(alpha).unwrap().is_hidden());
        // Shared properties were de-duplicated: 4 tiles share hvcC/ispe/pixi/colr.
        assert!(meta.properties.len() <= 8, "{}", meta.properties.len());
        assert_eq!(f.item_data(tiles[2]).unwrap().as_ref(), &[2u8; 16]);
        assert_eq!(f.item_data(grid).unwrap().len(), 8);
        assert_eq!(
            f.item_data(exif).unwrap().as_ref(),
            b"\0\0\0\0II*\0\x08\0\0\0"
        );
        assert!(meta.item(xmp).unwrap().is_xmp());
        let graph = crate::derived::build_primary_graph(&f).unwrap();
        assert_eq!(graph.inputs.len(), 4);
        assert!(graph.alpha.is_some());
        let rep = crate::miaf::check(&f, crate::miaf::MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{:#?}", rep.violations);
        // MIAF §7.2.1.13: thumbnail and metadata bodies precede the primary chain.
        let thumb_off = f.item_file_spans(thumb).unwrap()[0].0;
        let tile_off = f.item_file_spans(tiles[0]).unwrap()[0].0;
        assert!(thumb_off < tile_off);
    }

    #[test]
    fn validation_rejects_incomplete_items() {
        let mut w = HeifWriter::new();
        let id = w.add_coded_item(ITEM_TYPE_HVC1, vec![1], vec![]);
        w.set_primary(id);
        assert!(w.write_to_vec().is_err(), "no ispe / hvcC");
        let mut w = HeifWriter::new();
        w.add_coded_item(ITEM_TYPE_HVC1, vec![1], std_props(1, 1));
        assert!(w.write_to_vec().is_err(), "no primary");
        let mut w = HeifWriter::new();
        let id = w.add_coded_item(ITEM_TYPE_HVC1, vec![1], std_props(1, 1));
        w.set_primary(id);
        w.add_reference(reference::THMB, 99, vec![id]);
        assert!(w.write_to_vec().is_err(), "dangling reference");
    }

    #[test]
    fn identity_and_overlay_round_trip() {
        let mut w = HeifWriter::new();
        let base = w.add_coded_item(ITEM_TYPE_HVC1, vec![1; 4], std_props(64, 64));
        let stamp = w.add_coded_item(ITEM_TYPE_HVC1, vec![2; 4], std_props(16, 16));
        let ovl = w
            .add_overlay(
                OverlayDescriptor {
                    canvas_fill: [0, 0, 0, 65535],
                    output_width: 64,
                    output_height: 64,
                    offsets: vec![(0, 0), (8, 8)],
                },
                &[base, stamp],
                vec![],
            )
            .unwrap();
        let iden = w.add_identity(
            ovl,
            vec![
                (
                    Property::Ispe(Ispe {
                        width: 64,
                        height: 64,
                    }),
                    false,
                ),
                (Property::Irot(crate::props::Irot { angle: 1 }), true),
            ],
        );
        w.set_primary(iden);
        let bytes = w.write_to_vec().unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        let g = crate::derived::build_primary_graph(&f).unwrap();
        assert_eq!(
            g.chain_types(),
            vec![ITEM_TYPE_IDEN, ITEM_TYPE_IOVL, ITEM_TYPE_HVC1]
        );
        assert_eq!(g.output_size().unwrap(), (64, 64));
        let rep = crate::miaf::check(&f, crate::miaf::MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{:#?}", rep.violations);
    }

    #[test]
    fn sequence_writer_round_trips_through_the_track_walker() {
        let mut sw = SequenceWriter::new(ITEM_TYPE_HVC1, Property::HvcC(hvcc()), 64, 64, 30);
        sw.push_sample(vec![1; 10], 1, true);
        sw.push_sample(vec![2; 12], 1, false);
        sw.push_sample(vec![3; 14], 2, true);
        let mut still = HeifWriter::new();
        let id = still.add_coded_item(ITEM_TYPE_HVC1, vec![1; 10], std_props(64, 64));
        still.set_primary(id);
        sw.still = Some(still);
        let bytes = sw.write_to_vec().unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        assert!(f.file_type.has_brand(&BRAND_MSF1));
        assert!(f.file_type.has_brand(&BRAND_HEVC));
        let mv = crate::sequence::parse_movie(&f).unwrap().unwrap();
        assert_eq!(mv.timescale, 30);
        let t = &mv.tracks[0];
        assert_eq!(&t.handler, b"pict");
        assert_eq!(t.samples.len(), 3);
        assert_eq!(t.samples[1].dts, 1);
        assert!(!t.samples[1].is_sync);
        assert_eq!(t.samples[2].duration, 2);
        assert_eq!(
            crate::sequence::sample_bytes(&f, &t.samples[2]).unwrap(),
            &[3u8; 14]
        );
        let e = t.primary_entry().unwrap();
        assert!(e.hvcc.is_some() && e.ccst.is_some());
        assert_eq!(f.item_data(id).unwrap().as_ref(), &[1u8; 10]);
        assert_eq!(f.primary_item().unwrap().id, id);
        let rep = crate::miaf::check(&f, crate::miaf::MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{:#?}", rep.violations);
    }

    #[test]
    fn large_ids_switch_to_32_bit_boxes() {
        let mut w = HeifWriter::new();
        w.next_id = 70_000;
        let id = w.add_coded_item(ITEM_TYPE_HVC1, vec![1], std_props(1, 1));
        w.set_primary(id);
        let bytes = w.write_to_vec().unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        assert_eq!(f.primary_item().unwrap().id, 70_000);
        assert_eq!(f.meta().unwrap().items[0].version, 3);
    }
}
