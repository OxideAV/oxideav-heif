//! Derived image items (ISO/IEC 23008-12 §6.6): the `grid` / `iovl` /
//! `iden` descriptors and the derivation graph rooted at an image item.
//!
//! The graph resolves every `dimg` edge, thumbnails (`thmb`), auxiliary
//! images (`auxl`) and metadata (`cdsc`) for an item, with cycle
//! detection, depth / fan-out / total-node limits, so a hostile file
//! cannot make a reader recurse or allocate without bound. Pixel
//! composition lives in the `compose` module; this module is pure
//! container structure and is part of the standalone surface.

use crate::boxes::{fourcc_str, FourCc, Reader};
use crate::error::{HeifError, Result};
use crate::file::HeifFile;
use crate::meta::{
    reference, ItemInfo, Meta, ITEM_TYPE_CFEN, ITEM_TYPE_GRID, ITEM_TYPE_IDEN, ITEM_TYPE_IOVL,
    ITEM_TYPE_TILI, ITEM_TYPE_TMAP,
};
use crate::props::{AuxKind, ItemProperties};

/// Maximum depth of a derivation chain (MIAF §7.3.11.1 allows at most
/// five derivation steps; the HEIF base format is unbounded, so a
/// generous cap protects against cycles that slip past the visited set).
pub const MAX_DERIVATION_DEPTH: usize = 16;
/// Maximum number of input images of one derived item.
pub const MAX_DERIVATION_INPUTS: usize = 1 << 16;
/// Maximum number of distinct items one derivation graph may touch.
pub const MAX_GRAPH_ITEMS: usize = 1 << 16;
/// Maximum canvas area (in pixels) a `grid` / `iovl` may declare.
pub const MAX_CANVAS_PIXELS: u64 = 1 << 30;

/// `ImageGrid` descriptor (§6.6.2.3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct GridDescriptor {
    /// Number of tile rows (`rows_minus_one + 1`).
    pub rows: u16,
    /// Number of tile columns (`columns_minus_one + 1`).
    pub columns: u16,
    /// `output_width`.
    pub output_width: u32,
    /// `output_height`.
    pub output_height: u32,
}
impl GridDescriptor {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(rows: u16, columns: u16, output_width: u32, output_height: u32) -> Self {
        Self {
            rows,
            columns,
            output_width,
            output_height,
        }
    }
}

impl GridDescriptor {
    /// Parse the item body of a `grid` item.
    pub fn parse(body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let version = r.u8("grid version")?;
        if version != 0 {
            return Err(HeifError::unsupported(format!(
                "grid version {version} (readers shall not process unknown versions)"
            )));
        }
        let flags = r.u8("grid flags")?;
        let rows = r.u8("grid rows_minus_one")? as u16 + 1;
        let columns = r.u8("grid columns_minus_one")? as u16 + 1;
        let (output_width, output_height) = if flags & 1 == 1 {
            (r.u32("grid output_width")?, r.u32("grid output_height")?)
        } else {
            (
                r.u16("grid output_width")? as u32,
                r.u16("grid output_height")? as u32,
            )
        };
        if output_width == 0 || output_height == 0 {
            return Err(HeifError::invalid("grid: zero output size"));
        }
        if output_width as u64 * output_height as u64 > MAX_CANVAS_PIXELS {
            return Err(HeifError::exhausted(format!(
                "grid: canvas {output_width}x{output_height} exceeds {MAX_CANVAS_PIXELS} pixels"
            )));
        }
        Ok(Self {
            rows,
            columns,
            output_width,
            output_height,
        })
    }

    /// `reference_count` the `dimg` box shall carry.
    pub fn tile_count(&self) -> usize {
        self.rows as usize * self.columns as usize
    }

    /// Serialize (16-bit fields when they fit, else 32-bit).
    pub fn to_bytes(&self) -> Vec<u8> {
        let large = self.output_width > 0xffff || self.output_height > 0xffff;
        let mut b = vec![
            0,
            large as u8,
            (self.rows - 1) as u8,
            (self.columns - 1) as u8,
        ];
        if large {
            b.extend_from_slice(&self.output_width.to_be_bytes());
            b.extend_from_slice(&self.output_height.to_be_bytes());
        } else {
            b.extend_from_slice(&(self.output_width as u16).to_be_bytes());
            b.extend_from_slice(&(self.output_height as u16).to_be_bytes());
        }
        b
    }
}

/// `ImageOverlay` descriptor (§6.6.2.2.2).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct OverlayDescriptor {
    /// `canvas_fill_value[4]` — R, G, B (sRGB, 16-bit) and A (0..=65535 opacity).
    pub canvas_fill: [u16; 4],
    /// `output_width`.
    pub output_width: u32,
    /// `output_height`.
    pub output_height: u32,
    /// `(horizontal_offset, vertical_offset)` per input, in `dimg` order.
    pub offsets: Vec<(i32, i32)>,
}
impl OverlayDescriptor {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(
        canvas_fill: [u16; 4],
        output_width: u32,
        output_height: u32,
        offsets: Vec<(i32, i32)>,
    ) -> Self {
        Self {
            canvas_fill,
            output_width,
            output_height,
            offsets,
        }
    }
}

impl OverlayDescriptor {
    /// Parse the item body of an `iovl` item; `reference_count` comes
    /// from the item's `dimg` reference.
    pub fn parse(body: &[u8], reference_count: usize) -> Result<Self> {
        let mut r = Reader::new(body);
        let version = r.u8("iovl version")?;
        if version != 0 {
            return Err(HeifError::unsupported(format!("iovl version {version}")));
        }
        let flags = r.u8("iovl flags")?;
        let large = flags & 1 == 1;
        let mut canvas_fill = [0u16; 4];
        for c in canvas_fill.iter_mut() {
            *c = r.u16("iovl canvas_fill_value")?;
        }
        let (output_width, output_height) = if large {
            (r.u32("iovl output_width")?, r.u32("iovl output_height")?)
        } else {
            (
                r.u16("iovl output_width")? as u32,
                r.u16("iovl output_height")? as u32,
            )
        };
        if output_width == 0 || output_height == 0 {
            return Err(HeifError::invalid("iovl: zero output size"));
        }
        if output_width as u64 * output_height as u64 > MAX_CANVAS_PIXELS {
            return Err(HeifError::exhausted(format!(
                "iovl: canvas {output_width}x{output_height} exceeds {MAX_CANVAS_PIXELS} pixels"
            )));
        }
        if reference_count > MAX_DERIVATION_INPUTS {
            return Err(HeifError::exhausted(format!(
                "iovl: {reference_count} inputs exceeds {MAX_DERIVATION_INPUTS}"
            )));
        }
        let mut offsets = Vec::with_capacity(reference_count);
        for _ in 0..reference_count {
            offsets.push(if large {
                (
                    r.i32("iovl horizontal_offset")?,
                    r.i32("iovl vertical_offset")?,
                )
            } else {
                (
                    r.i16("iovl horizontal_offset")? as i32,
                    r.i16("iovl vertical_offset")? as i32,
                )
            });
        }
        Ok(Self {
            canvas_fill,
            output_width,
            output_height,
            offsets,
        })
    }

    /// Serialize (16-bit fields when everything fits, else 32-bit).
    pub fn to_bytes(&self) -> Vec<u8> {
        let large = self.output_width > 0xffff
            || self.output_height > 0xffff
            || self
                .offsets
                .iter()
                .any(|(h, v)| !(-0x8000..=0x7fff).contains(h) || !(-0x8000..=0x7fff).contains(v));
        let mut b = vec![0, large as u8];
        for c in self.canvas_fill {
            b.extend_from_slice(&c.to_be_bytes());
        }
        if large {
            b.extend_from_slice(&self.output_width.to_be_bytes());
            b.extend_from_slice(&self.output_height.to_be_bytes());
            for (h, v) in &self.offsets {
                b.extend_from_slice(&h.to_be_bytes());
                b.extend_from_slice(&v.to_be_bytes());
            }
        } else {
            b.extend_from_slice(&(self.output_width as u16).to_be_bytes());
            b.extend_from_slice(&(self.output_height as u16).to_be_bytes());
            for (h, v) in &self.offsets {
                b.extend_from_slice(&(*h as i16).to_be_bytes());
                b.extend_from_slice(&(*v as i16).to_be_bytes());
            }
        }
        b
    }
}

/// What an image item is, once its body has been interpreted.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageKind {
    /// A coded image item (`hvc1`, `av01`, …).
    Coded(FourCc),
    /// A `grid` derived image.
    Grid(GridDescriptor),
    /// An `iovl` derived image.
    Overlay(OverlayDescriptor),
    /// An `iden` derived image.
    Identity,
    /// A `tmap` (tone map / gain map) derived image; body kept raw.
    ToneMap(Vec<u8>),
    /// A `tili` tiled image item (Amd 2:2026 §6.11).
    Tiled(TiledItem),
    /// A `cfen` colour format enhancement derived image (Amd 1:2025
    /// §6.6.2.5).
    ColourFormatEnhancement(ColourFormatEnhancement),
}

/// The `ColourFormatEnhancement` body of a `cfen` item (HEIF Amd 1:2025
/// §6.6.2.5.2, channel table as amended by Amd 2:2026): per input, the
/// channel(s) its luma plane carries.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ColourFormatEnhancement {
    /// One entry per `dimg` input, in reference order.
    pub inputs: Vec<CfenInput>,
}
impl ColourFormatEnhancement {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(inputs: Vec<CfenInput>) -> Self {
        Self { inputs }
    }
}

/// What one `cfen` input's luma plane carries.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CfenInput {
    /// `is_packed_flag == 0`: the whole luma plane is `channel_id`
    /// (Table 2: 1 unspecified, 2 Y / R / C, 3 Cb / G / M, 4 Cr / B / Y,
    /// 5 alpha, 6 depth, 7 K).
    Channel(u8),
    /// `is_packed_flag == 1`: the luma plane is partitioned into
    /// `rows × cols` regions separated by guard bands.
    Packed {
        /// `num_cols_minus1 + 1`.
        cols: u8,
        /// `num_rows_minus1 + 1`.
        rows: u8,
        /// `hor_guard_band_mul2` (guard band = value × 2).
        hor_guard_band_mul2: u8,
        /// `ver_guard_band_mul2`.
        ver_guard_band_mul2: u8,
        /// `packed_channel_id[j][k]`, row-major.
        channels: Vec<u8>,
    },
}

impl ColourFormatEnhancement {
    /// Parse a `cfen` item body for `reference_count` inputs.
    pub fn parse(body: &[u8], reference_count: usize) -> Result<Self> {
        let mut r = crate::boxes::Reader::new(body);
        let version = r.u8("cfen version")?;
        if version != 0 {
            return Err(HeifError::unsupported(format!(
                "cfen version {version} (Amd 1 defines 0)"
            )));
        }
        let mut inputs = Vec::with_capacity(reference_count.min(64));
        for _ in 0..reference_count {
            let packed = r.u8("cfen is_packed_flag")? & 1 == 1;
            if packed {
                let w = r.u16("cfen packing")?;
                let cols = ((w >> 13) & 7) as u8 + 1;
                let rows = ((w >> 10) & 7) as u8 + 1;
                let hor = ((w >> 5) & 0x1f) as u8;
                let ver = (w & 0x1f) as u8;
                let channels = r
                    .bytes(cols as usize * rows as usize, "cfen packed_channel_id")?
                    .to_vec();
                inputs.push(CfenInput::Packed {
                    cols,
                    rows,
                    hor_guard_band_mul2: hor,
                    ver_guard_band_mul2: ver,
                    channels,
                });
            } else {
                inputs.push(CfenInput::Channel(r.u8("cfen channel_id")?));
            }
        }
        Ok(Self { inputs })
    }

    /// Serialise as a `cfen` item body.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8];
        for i in &self.inputs {
            match i {
                CfenInput::Channel(c) => {
                    b.push(0);
                    b.push(*c);
                }
                CfenInput::Packed {
                    cols,
                    rows,
                    hor_guard_band_mul2,
                    ver_guard_band_mul2,
                    channels,
                } => {
                    b.push(1);
                    let w = ((cols.saturating_sub(1) as u16 & 7) << 13)
                        | ((rows.saturating_sub(1) as u16 & 7) << 10)
                        | ((*hor_guard_band_mul2 as u16 & 0x1f) << 5)
                        | (*ver_guard_band_mul2 as u16 & 0x1f);
                    b.extend_from_slice(&w.to_be_bytes());
                    b.extend_from_slice(channels);
                }
            }
        }
        b
    }
}

/// A `tili` item's tiling: its `tilC` and the properties the
/// `TileItemPropertyAssociationBox` gives every tile (decoder
/// configuration first, as for a coded item).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TiledItem {
    /// `tilC`.
    pub config: crate::props::TilC,
    /// The tiles' properties (empty for external tiles, whose
    /// properties live in their own files).
    pub tile_properties: ItemProperties,
}
impl TiledItem {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(config: crate::props::TilC, tile_properties: ItemProperties) -> Self {
        Self {
            config,
            tile_properties,
        }
    }
}

/// One node of a derivation graph.
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct ImageNode {
    /// The item.
    pub item: ItemInfo,
    /// The item's kind.
    pub kind: ImageKind,
    /// Typed properties of the item.
    pub properties: ItemProperties,
    /// Input images (`dimg` targets), in reference order.
    pub inputs: Vec<ImageNode>,
    /// Alpha auxiliary attached through `auxl`, when present.
    pub alpha: Option<Box<ImageNode>>,
    /// Depth auxiliary attached through `auxl`, when present.
    pub depth: Option<Box<ImageNode>>,
    /// Other auxiliaries (`auxl` with an unrecognized URN).
    pub other_auxiliaries: Vec<ImageNode>,
    /// `true` when the master carries a `prem` reference to `alpha`.
    pub premultiplied_alpha: bool,
    /// Thumbnails (`thmb` sources), in file order.
    pub thumbnails: Vec<ImageNode>,
    /// Metadata items (`cdsc` sources) describing this image.
    pub metadata: Vec<ItemInfo>,
    /// Depth of this node in the derivation chain (0 = root).
    pub depth_in_chain: usize,
}
impl ImageNode {
    /// Every field as a positional argument, in declaration order.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        item: ItemInfo,
        kind: ImageKind,
        properties: ItemProperties,
        inputs: Vec<ImageNode>,
        alpha: Option<Box<ImageNode>>,
        depth: Option<Box<ImageNode>>,
        other_auxiliaries: Vec<ImageNode>,
        premultiplied_alpha: bool,
        thumbnails: Vec<ImageNode>,
        metadata: Vec<ItemInfo>,
        depth_in_chain: usize,
    ) -> Self {
        Self {
            item,
            kind,
            properties,
            inputs,
            alpha,
            depth,
            other_auxiliaries,
            premultiplied_alpha,
            thumbnails,
            metadata,
            depth_in_chain,
        }
    }
}

impl ImageNode {
    /// `ispe` of the reconstructed image, when declared.
    pub fn ispe(&self) -> Option<(u32, u32)> {
        self.properties.ispe().map(|i| (i.width, i.height))
    }

    /// Reconstructed size: `ispe` when declared, else the derivation's
    /// canvas or the single input's output size.
    pub fn reconstructed_size(&self) -> Result<(u32, u32)> {
        if let Some(s) = self.ispe() {
            return Ok(s);
        }
        match &self.kind {
            ImageKind::Grid(g) => Ok((g.output_width, g.output_height)),
            ImageKind::Overlay(o) => Ok((o.output_width, o.output_height)),
            ImageKind::Identity | ImageKind::ToneMap(_) => match self.inputs.first() {
                Some(i) => i.output_size(),
                None => Err(HeifError::invalid(format!(
                    "item {}: derived image without inputs",
                    self.item.id
                ))),
            },
            ImageKind::Coded(t) => Err(HeifError::invalid(format!(
                "item {} ('{}'): no ispe property (HEIF §6.5.3 requires one)",
                self.item.id,
                fourcc_str(t)
            ))),
            ImageKind::Tiled(_) => Err(HeifError::invalid(format!(
                "item {} (tili): no ispe property (Amd 2 §6.11.2 requires one)",
                self.item.id
            ))),
            ImageKind::ColourFormatEnhancement(_) => Err(HeifError::invalid(format!(
                "item {} (cfen): no ispe property (Amd 1 §6.6.2.5.1 requires one)",
                self.item.id
            ))),
        }
    }

    /// Output size after this item's transformative properties (§6.3).
    pub fn output_size(&self) -> Result<(u32, u32)> {
        let rec = self.reconstructed_size()?;
        self.properties.output_size(rec, false)
    }

    /// Every coded item reachable through `dimg` edges (this node
    /// included when coded), in derivation order.
    pub fn coded_items(&self) -> Vec<&ImageNode> {
        let mut out = Vec::new();
        self.collect_coded(&mut out);
        out
    }

    fn collect_coded<'a>(&'a self, out: &mut Vec<&'a ImageNode>) {
        if matches!(self.kind, ImageKind::Coded(_)) {
            out.push(self);
        }
        for i in &self.inputs {
            i.collect_coded(out);
        }
    }

    /// The derivation chain kinds from this node down to the coded
    /// leaves (first input only), e.g. `[iovl, grid, hvc1]`.
    pub fn chain_types(&self) -> Vec<FourCc> {
        let mut v = vec![self.item.item_type];
        if let Some(i) = self.inputs.first() {
            v.extend(i.chain_types());
        }
        v
    }
}

/// Build the derivation graph rooted at `item_id`.
pub fn build_graph<D: AsRef<[u8]>>(file: &HeifFile<D>, item_id: u32) -> Result<ImageNode> {
    let meta = file.meta()?;
    let mut budget = MAX_GRAPH_ITEMS;
    let mut stack = Vec::new();
    build_node(file, meta, item_id, 0, &mut stack, &mut budget)
}

/// Build the derivation graph rooted at the primary item.
pub fn build_primary_graph<D: AsRef<[u8]>>(file: &HeifFile<D>) -> Result<ImageNode> {
    let primary = file.primary_item()?;
    build_graph(file, primary.id)
}

fn build_node<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    meta: &Meta,
    item_id: u32,
    depth: usize,
    stack: &mut Vec<u32>,
    budget: &mut usize,
) -> Result<ImageNode> {
    if depth > MAX_DERIVATION_DEPTH {
        return Err(HeifError::exhausted(format!(
            "derivation chain at item {item_id} deeper than {MAX_DERIVATION_DEPTH}"
        )));
    }
    if stack.contains(&item_id) {
        return Err(HeifError::invalid(format!(
            "derivation cycle: item {item_id} reaches itself (chain {stack:?})"
        )));
    }
    if *budget == 0 {
        return Err(HeifError::exhausted(format!(
            "derivation graph touches more than {MAX_GRAPH_ITEMS} items"
        )));
    }
    *budget -= 1;
    let item = meta
        .item(item_id)
        .ok_or_else(|| HeifError::invalid(format!("item {item_id} is referenced but not in iinf")))?
        .clone();
    if !item.is_image() {
        return Err(HeifError::unsupported(format!(
            "item {item_id} type '{}' is not an image item",
            fourcc_str(&item.item_type)
        )));
    }
    let properties = ItemProperties::resolve(meta, item_id)?;
    stack.push(item_id);
    let input_ids = meta.derivation_inputs(item_id);
    if input_ids.len() > MAX_DERIVATION_INPUTS {
        stack.pop();
        return Err(HeifError::exhausted(format!(
            "item {item_id}: {} inputs exceeds {MAX_DERIVATION_INPUTS}",
            input_ids.len()
        )));
    }
    let kind = match item.item_type {
        ITEM_TYPE_GRID => {
            let body = file.item_data(item_id)?;
            let g = GridDescriptor::parse(&body)?;
            if g.tile_count() != input_ids.len() {
                stack.pop();
                return Err(HeifError::invalid(format!(
                    "grid item {item_id}: {}x{} tiles declared but {} dimg inputs",
                    g.rows,
                    g.columns,
                    input_ids.len()
                )));
            }
            ImageKind::Grid(g)
        }
        ITEM_TYPE_IOVL => {
            let body = file.item_data(item_id)?;
            ImageKind::Overlay(OverlayDescriptor::parse(&body, input_ids.len())?)
        }
        ITEM_TYPE_IDEN => {
            if input_ids.len() != 1 {
                stack.pop();
                return Err(HeifError::invalid(format!(
                    "iden item {item_id}: reference_count {} (shall be 1)",
                    input_ids.len()
                )));
            }
            ImageKind::Identity
        }
        ITEM_TYPE_CFEN => {
            // Amd 1 §6.6.2.5.1: reference_count >= 1.
            if input_ids.is_empty() {
                stack.pop();
                return Err(HeifError::invalid(format!(
                    "cfen item {item_id} has no input (reference_count shall be >= 1)"
                )));
            }
            let body = file.item_data(item_id)?;
            ImageKind::ColourFormatEnhancement(ColourFormatEnhancement::parse(
                &body,
                input_ids.len(),
            )?)
        }
        ITEM_TYPE_TILI => {
            if !input_ids.is_empty() {
                stack.pop();
                return Err(HeifError::invalid(format!(
                    "tili item {item_id} has dimg inputs (its tiles are addressed through deti)"
                )));
            }
            let tilc = properties.tilc().cloned().ok_or_else(|| {
                HeifError::invalid(format!(
                    "tili item {item_id} without a tilC property (Amd 2 §6.11.3)"
                ))
            })?;
            let tile_properties = match &tilc.in_file_tiles {
                Some((_, assoc)) => ItemProperties::from_associations(
                    meta,
                    assoc,
                    &format!("tili item {item_id} tile"),
                )?,
                None => ItemProperties::from_associations(meta, &[], "tili")?,
            };
            ImageKind::Tiled(TiledItem {
                config: tilc,
                tile_properties,
            })
        }
        ITEM_TYPE_TMAP => {
            // HEIF Amd 1 §6.6.2.4.1: reference_count shall be 2 — the
            // base input image, then the gain map input image.
            if input_ids.len() != 2 {
                stack.pop();
                return Err(HeifError::invalid(format!(
                    "tmap item {item_id}: reference_count {} (shall be 2: base, gain map)",
                    input_ids.len()
                )));
            }
            ImageKind::ToneMap(file.item_data(item_id)?.into_owned())
        }
        t => {
            if !input_ids.is_empty() {
                stack.pop();
                return Err(HeifError::invalid(format!(
                    "coded item {item_id} ('{}') carries dimg references",
                    fourcc_str(&t)
                )));
            }
            ImageKind::Coded(t)
        }
    };
    let mut inputs = Vec::with_capacity(input_ids.len());
    for id in input_ids {
        match build_node(file, meta, id, depth + 1, stack, budget) {
            Ok(n) => inputs.push(n),
            Err(e) => {
                stack.pop();
                return Err(e);
            }
        }
    }
    // Auxiliaries, thumbnails and metadata hang off the item; they do
    // not extend the derivation chain but share the budget / cycle set.
    let mut alpha = None;
    let mut depth_aux = None;
    let mut other_auxiliaries = Vec::new();
    let mut premultiplied_alpha = false;
    for aux_id in meta.auxiliaries_of(item_id) {
        let node = match build_node(file, meta, aux_id, depth + 1, stack, budget) {
            Ok(n) => n,
            Err(e) => {
                stack.pop();
                return Err(e);
            }
        };
        match node.properties.auxc().map(|a| a.kind()) {
            Some(AuxKind::Alpha) if alpha.is_none() => {
                premultiplied_alpha = meta.is_premultiplied(item_id, aux_id);
                alpha = Some(Box::new(node));
            }
            Some(AuxKind::Depth) if depth_aux.is_none() => depth_aux = Some(Box::new(node)),
            _ => other_auxiliaries.push(node),
        }
    }
    let mut thumbnails = Vec::new();
    for t in meta.thumbnails_of(item_id) {
        match build_node(file, meta, t, depth + 1, stack, budget) {
            Ok(n) => thumbnails.push(n),
            Err(e) => {
                stack.pop();
                return Err(e);
            }
        }
    }
    let metadata = meta
        .metadata_of(item_id)
        .into_iter()
        .filter_map(|id| meta.item(id).cloned())
        .collect();
    stack.pop();
    Ok(ImageNode {
        item,
        kind,
        properties,
        inputs,
        alpha,
        depth: depth_aux,
        other_auxiliaries,
        premultiplied_alpha,
        thumbnails,
        metadata,
        depth_in_chain: depth,
    })
}

/// `true` for the derived item types this crate composes.
pub fn is_derived_type(t: &FourCc) -> bool {
    matches!(
        *t,
        ITEM_TYPE_GRID
            | ITEM_TYPE_IOVL
            | ITEM_TYPE_IDEN
            | ITEM_TYPE_TMAP
            | ITEM_TYPE_TILI
            | ITEM_TYPE_CFEN
    )
}

#[doc(hidden)]
/// Alias kept for readers of the item-reference vocabulary.
pub const DIMG: FourCc = reference::DIMG;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_descriptor_round_trip() {
        let g = GridDescriptor {
            rows: 2,
            columns: 3,
            output_width: 300,
            output_height: 200,
        };
        let b = g.to_bytes();
        assert_eq!(b, [0, 0, 1, 2, 1, 44, 0, 200]);
        assert_eq!(GridDescriptor::parse(&b).unwrap(), g);
        assert_eq!(g.tile_count(), 6);
        let big = GridDescriptor {
            output_width: 70000,
            ..g
        };
        let b = big.to_bytes();
        assert_eq!(b.len(), 12);
        assert_eq!(GridDescriptor::parse(&b).unwrap(), big);
        assert!(GridDescriptor::parse(&[1, 0, 0, 0, 0, 1, 0, 1]).is_err());
        assert!(GridDescriptor::parse(&[0, 0, 0, 0, 0, 0, 0, 1]).is_err());
        assert!(GridDescriptor::parse(&[0, 0, 0]).is_err());
        assert!(GridDescriptor::parse(&[0, 1, 0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 2]).is_err());
    }

    #[test]
    fn overlay_descriptor_round_trip() {
        let o = OverlayDescriptor {
            canvas_fill: [16384, 16384, 16384, 65535],
            output_width: 256,
            output_height: 256,
            offsets: vec![(0, 0), (96, -96)],
        };
        let b = o.to_bytes();
        assert_eq!(b.len(), 2 + 8 + 4 + 8);
        assert_eq!(OverlayDescriptor::parse(&b, 2).unwrap(), o);
        assert!(OverlayDescriptor::parse(&b, 3).is_err());
        let wide = OverlayDescriptor {
            offsets: vec![(70000, 0)],
            ..o.clone()
        };
        let b = wide.to_bytes();
        assert_eq!(b[1], 1);
        assert_eq!(OverlayDescriptor::parse(&b, 1).unwrap(), wide);
        assert!(OverlayDescriptor::parse(&[0, 0, 0, 0], 0).is_err());
    }
}
