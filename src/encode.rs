//! Still-image encoding through the oxideav encoders (`registry`
//! feature): pixels → HEVC / AV1 items → a MIAF-conformant file via
//! [`HeifWriter`], plus the framework [`Encoder`] (`"heif"`: one frame
//! in, one complete `.heic` file per packet out).
//!
//! HEVC items come from `oxideav-h265`'s registry encoder: it emits
//! Annex B access units with the parameter sets in band; this module
//! splits the NAL units, lifts VPS / SPS / PPS into the `hvcC` record
//! (profile / tier / level from the SPS profile-tier-level, chroma and
//! bit depth from the SPS) and re-frames the VCL NAL units with 4-byte
//! length prefixes (HEIF Annex B.2.2). AV1 items come from
//! `oxideav-av1`'s key-frame encoder (an IVF buffer): the temporal unit
//! is extracted and the `av1C` record built from the Sequence Header
//! OBU. VVC items come from `oxideav-h266`'s IDR encoder (an Annex B
//! access unit, 8-bit 4:2:0, pictures padded to a multiple of 64 and
//! cropped back with `clap`): [`crate::vvcdec::vvc_item_from_annex_b`]
//! lifts the parameter sets into the `vvcC` record and re-frames the
//! picture header + slices as `VVCItemData` (HEIF Annex L).

use oxideav_core::{
    CodecId, CodecOptions, CodecParameters, Encoder, Error as CoreError, Frame, Packet,
    Result as CoreResult, TimeBase,
};

use crate::av1c::Av1Config;
use crate::derived::GridDescriptor;
use crate::error::{HeifError, Result};
use crate::hvcc::{
    join_length_prefixed, split_length_prefixed, HevcConfig, NalArray, NAL_PPS, NAL_SPS, NAL_VPS,
};
use crate::image::{Chroma, HeifFrame, HeifPixelFormat};
use crate::meta::{ITEM_TYPE_AV01, ITEM_TYPE_AVC1, ITEM_TYPE_HVC1, ITEM_TYPE_VVC1};
use crate::props::{Clap, Colr, CropRect, Ispe, Pixi, Property};
use crate::writer::HeifWriter;

/// Which codec produces the coded items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StillCodec {
    /// HEVC (`hvc1` items, `heic` brand).
    Hevc,
    /// AV1 (`av01` items, `avif` brand); lossless or quality-dialled
    /// reduced-header stills at every (depth, chroma) pairing.
    Av1,
    /// VVC (`vvc1` items, `vvic` brand; HEIF Annex L) through
    /// `oxideav-h266`: CABAC intra at `qp`, 8-bit 4:2:0 (the encoder's
    /// line); no lossless mode.
    Vvc,
    /// AVC (`avc1` items, `avci` brand; HEIF Annex E) through
    /// `oxideav-h264`'s registry encoder: one IDR picture at `qp`
    /// (CAVLC Baseline, or CABAC Main with `avc_options` `cabac=1`,
    /// for 8-bit 4:2:0; the §A.2 minimum High-family profile for
    /// 4:2:2 / 4:4:4 / 10 / 12-bit sources), lossless under `pcm`
    /// (High 4:4:4 Predictive transform bypass); a 4:0:0 source is
    /// coded 4:2:0 with neutral chroma (the encoder's grey line).
    Avc,
}

/// Where [`EncodeOptions::transforms`] are written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransformCarrier {
    /// On the displayed items themselves (the coded primary or the
    /// `grid` / `iovl`, its alpha and depth auxiliaries, the
    /// thumbnails and the gain map), as essential properties — the
    /// shape every third-party producer writes and every reader
    /// honours.
    #[default]
    CodedItem,
    /// On an `iden` derived item (HEIF §6.6.2.3) over the un-rotated
    /// coded primary (or `grid` / `iovl`), which becomes the primary
    /// item; the auxiliaries stay on the input without transforms (the
    /// derivation displays its input with them and then transforms the
    /// composite), the thumbnails and the gain map carry the transforms
    /// themselves. The written `iden` round-trips through this crate
    /// and the MIAF checks; see the README's interop table for the
    /// third-party readers.
    Identity,
}

/// One input picture of an [`OverlaySpec`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct OverlayLayer {
    /// The picture (its alpha plane, when present, becomes the layer's
    /// alpha auxiliary — §6.6.2.2.1 straight alpha).
    pub frame: HeifFrame,
    /// `horizontal_offset`: the layer's left edge on the canvas.
    pub x: i32,
    /// `vertical_offset`: the layer's top edge on the canvas.
    pub y: i32,
}
impl OverlayLayer {
    /// Every field as a positional argument, in declaration order.
    pub fn new(frame: HeifFrame, x: i32, y: i32) -> Self {
        Self { frame, x, y }
    }
}

/// An `iovl` overlay (HEIF §6.6.2.2) to write: a canvas filled with
/// `fill`, the `layers` composited bottom-most first at their offsets.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct OverlaySpec {
    /// `output_width`.
    pub width: u32,
    /// `output_height`.
    pub height: u32,
    /// `canvas_fill_value`: R, G, B (sRGB, 16-bit) and A (0..=65535
    /// opacity; a translucent fill yields an output alpha plane).
    pub fill: [u16; 4],
    /// The inputs, bottom-most first.
    pub layers: Vec<OverlayLayer>,
}
impl OverlaySpec {
    /// Every field as a positional argument, in declaration order.
    pub fn new(width: u32, height: u32, fill: [u16; 4], layers: Vec<OverlayLayer>) -> Self {
        Self {
            width,
            height,
            fill,
            layers,
        }
    }
}

/// Encoding options.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct EncodeOptions {
    /// Codec.
    pub codec: StillCodec,
    /// HEVC: `"pcm"` (lossless), `"intra"` (CABAC intra at `qp`).
    /// VVC has no lossless mode: `"pcm"` is refused there.
    pub hevc_mode: String,
    /// HEVC / VVC intra QP (0..=51). The default, [`DEFAULT_QP`], is
    /// the production setting (see the README's defaults section).
    pub qp: u8,
    /// Split the picture into a grid of `tile × tile` tiles (`grid`
    /// derived item): `Some(tile)` always (tiles are at least 64
    /// pixels, MIAF), `Some(0)` never, `None` automatically —
    /// [`GRID_AUTO_TILE`]-pixel tiles once the picture exceeds
    /// [`GRID_AUTO_MIN_PIXELS`] (the codec's working set then scales
    /// with a tile, not the picture, and the tiles are the parallel
    /// unit under a thread budget).
    pub grid_tile: Option<u32>,
    /// Add a thumbnail whose largest dimension is this many pixels.
    pub thumbnail_max_dim: Option<u32>,
    /// Colour information written as `colr`.
    pub colr: Colr,
    /// ICC profile written as a second `colr` (`prof`).
    pub icc_profile: Option<Vec<u8>>,
    /// Exif payload (from the TIFF header) to attach.
    pub exif: Option<Vec<u8>>,
    /// XMP packet to attach.
    pub xmp: Option<String>,
    /// Transformative properties applied to the primary, in `ipma`
    /// order (`clap` → `irot` → `imir` is the §6.5.1 chain readers
    /// apply) — written where [`EncodeOptions::transform_carrier`]
    /// says: as essential properties of every displayed item (the
    /// default), or on an `iden` derived item over the coded picture.
    pub transforms: Vec<Property>,
    /// AV1: quality dial 0..=100 (100 = lossless) for lossy items;
    /// `None` codes lossless (the pre-0.0.3 behaviour).
    pub av1_quality: Option<u8>,
    /// AV1 search effort: `"fast"` (default), `"balanced"`, `"thorough"`.
    pub av1_speed: String,
    /// HEVC intra mode-decision effort (`rd` 0..=2; the encoder's
    /// still default when `None`).
    pub hevc_rd: Option<u32>,
    /// HEVC tile layout (`"CxR"`, e.g. `"4x4"`) for parallel coding.
    pub hevc_tiles: Option<String>,
    /// HEVC coding-tree block size (16 / 32 / 64) of the quadtree coder.
    /// `rd` and `tiles` run on that coder only; when either is set and
    /// this is `None` the size is chosen automatically ([`auto_ctb`]:
    /// the largest of 64 / 32 / 16 whose CTB grid still holds the tile
    /// layout). `None` without `rd` / `tiles` keeps the encoder's
    /// historical coder (byte-stable streams).
    pub hevc_ctb: Option<u32>,
    /// ISO 21496-1 gain map to carry as a `tmap` derived item over
    /// the primary (HEIF Amd 1 §6.6.2.4).
    pub gain_map: Option<GainMapSpec>,
    /// Thread budget (`None` / `Some(1)` = serial, the core contract).
    /// Spent on the tiles of a `grid` (independent jobs, bytes
    /// identical to the serial order), on the HEVC quadtree coder's
    /// wavefront (`wpp`, enabled whenever that coder runs without
    /// `tiles`, so the bytes never depend on the budget) or tile
    /// fan-out, and on the AV1 still encoder's tile search (the tile
    /// layout is a function of the picture size alone — see
    /// [`AV1_TILE_LAYOUT_THREADS`]).
    pub threads: Option<usize>,
    /// HEVC coded bit depth (8 / 10 / 12). `None` follows the source
    /// ([`coded_depth`]): 8-bit sources code at 8 bits (the historical
    /// coder, or the quadtree coder when `rd` / `tiles` / `ctb` ask
    /// for it), 9–10-bit ones at 10, 11–12-bit at 12 and deeper ones
    /// at 10 (Main 10 / Main 12 through the quadtree coder).
    pub hevc_depth: Option<u8>,
    /// HEVC in-loop filters (deblocking + SAO) on the lossy `intra`
    /// mode. `None` = the production default (on).
    pub hevc_filters: Option<bool>,
    /// Extra options handed to the HEVC encoder after the ones this
    /// crate derives (`aq`, `rdoq`, `sdh`, `cqpoffset`, …; a later
    /// key wins). Expert knob; the framework encoder exposes a typed
    /// subset.
    pub hevc_options: Vec<(String, String)>,
    /// Chroma layout packed / planar RGB sources are coded in on AV1
    /// (`None` = 4:2:0; `Some(Chroma::Yuv444)` keeps full chroma).
    /// HEVC items are always 4:2:0; planar YCbCr sources keep their
    /// own layout on AV1 regardless. VVC items are 8-bit 4:2:0 always.
    pub chroma: Option<Chroma>,
    /// VVC encoder tools (`oxideav-h266` `EncoderConfig` knobs, every
    /// value `0` / `1` unless noted): `tiles` (`"CxR"`), `wpp`,
    /// `dep_quant`, `sdh` (sign data hiding; exclusive with
    /// `dep_quant`), `palette`, `mtt_bt`, `mtt_tt`, `alf_clip_rdo`,
    /// `chroma_sao_merge`, `loop_filter_across_tiles`,
    /// `slice_per_tile`, `raster_slices` (a count). Expert knob;
    /// unknown keys are refused.
    pub vvc_options: Vec<(String, String)>,
    /// AVC encoder options passed to `oxideav-h264`'s registry encoder
    /// next to `rc=cqp` / `qp` (its schema: `cabac` (`0` / `1`),
    /// `gop`, `bframes`, `rc`, `bitrate`, `max_bitrate`,
    /// `buffer_size`); a still is one IDR picture, so the entropy
    /// coder is the one that matters. Unknown keys are refused by the
    /// encoder.
    pub avc_options: Vec<(String, String)>,
    /// A depth map to carry as the primary's depth auxiliary item (HEIF
    /// §7.5.3: `auxl` + `auxC` with the depth URN — the HEVC one on
    /// HEVC items, the CICP one otherwise): coded like the alpha plane
    /// (a monochrome picture, or monochrome-in-4:2:0 on HEVC / AVC) at
    /// the primary's coded depth, any size (readers resize it to the
    /// master). The frame's own alpha plane, if any, is ignored.
    pub depth_map: Option<HeifFrame>,
    /// Where `transforms` are written (see [`TransformCarrier`]).
    pub transform_carrier: TransformCarrier,
}
impl EncodeOptions {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        codec: StillCodec,
        hevc_mode: String,
        qp: u8,
        grid_tile: Option<u32>,
        thumbnail_max_dim: Option<u32>,
        colr: Colr,
        icc_profile: Option<Vec<u8>>,
        exif: Option<Vec<u8>>,
        xmp: Option<String>,
        transforms: Vec<Property>,
        av1_quality: Option<u8>,
        av1_speed: String,
        hevc_rd: Option<u32>,
        hevc_tiles: Option<String>,
        hevc_ctb: Option<u32>,
        gain_map: Option<GainMapSpec>,
        threads: Option<usize>,
        hevc_depth: Option<u8>,
        hevc_filters: Option<bool>,
        hevc_options: Vec<(String, String)>,
        chroma: Option<Chroma>,
        vvc_options: Vec<(String, String)>,
        avc_options: Vec<(String, String)>,
        depth_map: Option<HeifFrame>,
        transform_carrier: TransformCarrier,
    ) -> Self {
        Self {
            codec,
            hevc_mode,
            qp,
            grid_tile,
            thumbnail_max_dim,
            colr,
            icc_profile,
            exif,
            xmp,
            transforms,
            av1_quality,
            av1_speed,
            hevc_rd,
            hevc_tiles,
            hevc_ctb,
            gain_map,
            threads,
            hevc_depth,
            hevc_filters,
            hevc_options,
            chroma,
            vvc_options,
            avc_options,
            depth_map,
            transform_carrier,
        }
    }

    /// Set `depth_map`.
    pub fn with_depth_map(mut self, depth_map: Option<HeifFrame>) -> Self {
        self.depth_map = depth_map;
        self
    }

    /// Set `transform_carrier`.
    pub fn with_transform_carrier(mut self, transform_carrier: TransformCarrier) -> Self {
        self.transform_carrier = transform_carrier;
        self
    }

    /// Set `vvc_options`.
    pub fn with_vvc_options(mut self, vvc_options: Vec<(String, String)>) -> Self {
        self.vvc_options = vvc_options;
        self
    }
    /// Builder-style setter for `avc_options`.
    pub fn with_avc_options(mut self, avc_options: Vec<(String, String)>) -> Self {
        self.avc_options = avc_options;
        self
    }

    /// Set `chroma`.
    pub fn with_chroma(mut self, chroma: Option<Chroma>) -> Self {
        self.chroma = chroma;
        self
    }

    /// Set `codec`.
    pub fn with_codec(mut self, codec: StillCodec) -> Self {
        self.codec = codec;
        self
    }

    /// Set `hevc_mode`.
    pub fn with_hevc_mode(mut self, hevc_mode: String) -> Self {
        self.hevc_mode = hevc_mode;
        self
    }

    /// Set `qp`.
    pub fn with_qp(mut self, qp: u8) -> Self {
        self.qp = qp;
        self
    }

    /// Set `grid_tile`.
    pub fn with_grid_tile(mut self, grid_tile: Option<u32>) -> Self {
        self.grid_tile = grid_tile;
        self
    }

    /// Set `thumbnail_max_dim`.
    pub fn with_thumbnail_max_dim(mut self, thumbnail_max_dim: Option<u32>) -> Self {
        self.thumbnail_max_dim = thumbnail_max_dim;
        self
    }

    /// Set `colr`.
    pub fn with_colr(mut self, colr: Colr) -> Self {
        self.colr = colr;
        self
    }

    /// Set `icc_profile`.
    pub fn with_icc_profile(mut self, icc_profile: Option<Vec<u8>>) -> Self {
        self.icc_profile = icc_profile;
        self
    }

    /// Set `exif`.
    pub fn with_exif(mut self, exif: Option<Vec<u8>>) -> Self {
        self.exif = exif;
        self
    }

    /// Set `xmp`.
    pub fn with_xmp(mut self, xmp: Option<String>) -> Self {
        self.xmp = xmp;
        self
    }

    /// Set `transforms`.
    pub fn with_transforms(mut self, transforms: Vec<Property>) -> Self {
        self.transforms = transforms;
        self
    }

    /// Set `av1_quality`.
    pub fn with_av1_quality(mut self, av1_quality: Option<u8>) -> Self {
        self.av1_quality = av1_quality;
        self
    }

    /// Set `av1_speed`.
    pub fn with_av1_speed(mut self, av1_speed: String) -> Self {
        self.av1_speed = av1_speed;
        self
    }

    /// Set `hevc_rd`.
    pub fn with_hevc_rd(mut self, hevc_rd: Option<u32>) -> Self {
        self.hevc_rd = hevc_rd;
        self
    }

    /// Set `hevc_tiles`.
    pub fn with_hevc_tiles(mut self, hevc_tiles: Option<String>) -> Self {
        self.hevc_tiles = hevc_tiles;
        self
    }

    /// Set `hevc_ctb`.
    pub fn with_hevc_ctb(mut self, hevc_ctb: Option<u32>) -> Self {
        self.hevc_ctb = hevc_ctb;
        self
    }

    /// Set `gain_map`.
    pub fn with_gain_map(mut self, gain_map: Option<GainMapSpec>) -> Self {
        self.gain_map = gain_map;
        self
    }

    /// Set `threads`.
    pub fn with_threads(mut self, threads: Option<usize>) -> Self {
        self.threads = threads;
        self
    }

    /// Set `hevc_depth`.
    pub fn with_hevc_depth(mut self, hevc_depth: Option<u8>) -> Self {
        self.hevc_depth = hevc_depth;
        self
    }

    /// Set `hevc_filters`.
    pub fn with_hevc_filters(mut self, hevc_filters: Option<bool>) -> Self {
        self.hevc_filters = hevc_filters;
        self
    }

    /// Set `hevc_options`.
    pub fn with_hevc_options(mut self, hevc_options: Vec<(String, String)>) -> Self {
        self.hevc_options = hevc_options;
        self
    }
}

/// The nominal worker count the AV1 still tile layout is derived from
/// (`oxideav_av1::encoder::auto_tile_layout`): fixed, so the coded
/// bytes of an AV1 item never depend on the thread budget the search
/// actually ran on.
pub const AV1_TILE_LAYOUT_THREADS: usize = 8;

/// A gain map to write next to the base image (see
/// [`EncodeOptions::gain_map`]): the map picture, its ISO 21496-1
/// metadata and the alternate (fully applied) rendition's colour
/// information. The map is coded with the same codec as the base, as
/// a hidden item with an `nclx` `colr` of `colour_primaries =
/// transfer_characteristics = 2` (the clause's rule) carrying
/// `gain_map_matrix` / `gain_map_full_range`; monochrome maps stay
/// monochrome (AV1) or ride the luma of a 4:2:0 picture (HEVC).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct GainMapSpec {
    /// The gain-map picture (monochrome or colour; any size — 21496-1
    /// §6.2.2 resamples it to the base at application).
    pub frame: HeifFrame,
    /// The C.2 metadata (`GainMapMetadata`).
    pub metadata: crate::gainmap::GainMapMetadata,
    /// The alternate image's colour information, written as the
    /// `tmap` item's `colr` (21496-1 §5.3.2).
    pub alternate_colr: Colr,
    /// `matrix_coefficients` of the stored gain map's YCbCr coding
    /// (colour maps; ignored for monochrome).
    pub gain_map_matrix: u16,
    /// `full_range_flag` of the stored gain map.
    pub gain_map_full_range: bool,
    /// `clli` hint for the alternate rendition, written on the `tmap`
    /// item ("should", §6.6.2.4.1).
    pub alternate_clli: Option<crate::props::Clli>,
    /// The `pixi` hint on the `tmap` item: "the approximate amount of
    /// colour resolution available after fully applying the gain map"
    /// (§6.6.2.4.1); also the depth this crate reconstructs the applied
    /// rendition at. 10–12 suits a PQ / HLG alternate over an 8-bit base.
    pub alternate_bit_depth: u8,
}
impl GainMapSpec {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(
        frame: HeifFrame,
        metadata: crate::gainmap::GainMapMetadata,
        alternate_colr: Colr,
        gain_map_matrix: u16,
        gain_map_full_range: bool,
        alternate_clli: Option<crate::props::Clli>,
        alternate_bit_depth: u8,
    ) -> Self {
        Self {
            frame,
            metadata,
            alternate_colr,
            gain_map_matrix,
            gain_map_full_range,
            alternate_clli,
            alternate_bit_depth,
        }
    }

    /// Set `frame`.
    pub fn with_frame(mut self, frame: HeifFrame) -> Self {
        self.frame = frame;
        self
    }

    /// Set `metadata`.
    pub fn with_metadata(mut self, metadata: crate::gainmap::GainMapMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Set `alternate_colr`.
    pub fn with_alternate_colr(mut self, alternate_colr: Colr) -> Self {
        self.alternate_colr = alternate_colr;
        self
    }

    /// Set `gain_map_matrix`.
    pub fn with_gain_map_matrix(mut self, gain_map_matrix: u16) -> Self {
        self.gain_map_matrix = gain_map_matrix;
        self
    }

    /// Set `gain_map_full_range`.
    pub fn with_gain_map_full_range(mut self, gain_map_full_range: bool) -> Self {
        self.gain_map_full_range = gain_map_full_range;
        self
    }

    /// Set `alternate_clli`.
    pub fn with_alternate_clli(mut self, alternate_clli: Option<crate::props::Clli>) -> Self {
        self.alternate_clli = alternate_clli;
        self
    }

    /// Set `alternate_bit_depth`.
    pub fn with_alternate_bit_depth(mut self, alternate_bit_depth: u8) -> Self {
        self.alternate_bit_depth = alternate_bit_depth;
        self
    }
}

/// The production HEVC intra QP: on a 12 MP photograph the historical
/// coder with the in-loop filters lands at or above the OS encoder's
/// default-quality PSNR (see the README) — the tuned default of
/// `oxideav convert in.png out.heic`.
pub const DEFAULT_QP: u8 = 18;

/// Tile edge of the automatic grid (`grid_tile == None`): the OS
/// producer's own tiling of a 12 MP picture.
pub const GRID_AUTO_TILE: u32 = 512;

/// Pictures with more luma samples than this are tiled automatically
/// (4 MP: 2048 × 2048).
pub const GRID_AUTO_MIN_PIXELS: u64 = 4 * 1024 * 1024;

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            codec: StillCodec::Hevc,
            hevc_mode: "intra".into(),
            qp: DEFAULT_QP,
            grid_tile: None,
            thumbnail_max_dim: None,
            colr: Colr::MIAF_DEFAULT,
            icc_profile: None,
            exif: None,
            xmp: None,
            transforms: Vec::new(),
            av1_quality: None,
            av1_speed: "fast".into(),
            hevc_rd: None,
            hevc_tiles: None,
            hevc_ctb: None,
            gain_map: None,
            threads: None,
            hevc_depth: None,
            hevc_filters: None,
            hevc_options: Vec::new(),
            chroma: None,
            vvc_options: Vec::new(),
            avc_options: Vec::new(),
            depth_map: None,
            transform_carrier: TransformCarrier::CodedItem,
        }
    }
}

impl EncodeOptions {
    /// The thread budget as a worker count (1 = serial).
    pub fn workers(&self) -> usize {
        self.threads.unwrap_or(1).max(1)
    }

    /// The grid tile a `width × height` picture is coded with under
    /// [`EncodeOptions::grid_tile`]'s rule (`None` = single item).
    pub fn effective_grid_tile(&self, width: u32, height: u32) -> Option<u32> {
        match self.grid_tile {
            Some(0) => None,
            Some(t) => Some(t),
            None => (width as u64 * height as u64 > GRID_AUTO_MIN_PIXELS).then_some(GRID_AUTO_TILE),
        }
    }

    /// Whether the HEVC items run on the quadtree coder (`rd` /
    /// `tiles` / `ctb` set, or a coded layout the historical 8-bit
    /// 4:2:0 coder does not take).
    pub fn hevc_quadtree(&self, coded: HeifPixelFormat) -> bool {
        self.hevc_rd.is_some()
            || self.hevc_tiles.is_some()
            || self.hevc_ctb.is_some()
            || coded.bit_depth != 8
            || coded.chroma != Chroma::Yuv420
    }

    /// The layout HEVC items of a planar `source` picture are coded
    /// in: the source's own chroma sampling (4:0:0 / 4:2:0 / 4:2:2 /
    /// 4:4:4) at [`EncodeOptions::hevc_depth`] or the depth the source
    /// implies (8 → 8, 9–10 → 10, 11–12 → 12, deeper → 10), so a planar
    /// source loses no chroma and the lossless mode (`pcm`) is lossless
    /// on every layout up to 12 bits; a lossless identity-matrix
    /// picture ([`EncodeOptions::colr`] with `matrix_coefficients = 0`,
    /// i.e. an RGB source kept as G, B, R planes) is 4:4:4 the same
    /// way. Packed RGB sources are converted to
    /// [`crate::api_registry::packed_target`]'s layout
    /// ([`EncodeOptions::chroma`], 4:2:0 by default) before they reach
    /// this rule.
    pub fn hevc_coded_format(&self, source: HeifPixelFormat) -> Result<HeifPixelFormat> {
        HeifPixelFormat::new(
            source.chroma,
            coded_depth(source.bit_depth, self.hevc_depth)?,
            false,
        )
    }

    /// The layout a VVC item codes a planar `source` in (round 476,
    /// oxideav-h266 0.0.10): the source's chroma sampling at the coded
    /// depth capped at [`VVC_MAX_DEPTH`] (an explicit deeper
    /// `hevc_depth` is a typed refusal).
    pub fn vvc_coded_format(&self, source: HeifPixelFormat) -> Result<HeifPixelFormat> {
        let depth = match self.hevc_depth {
            Some(d) if d > VVC_MAX_DEPTH => {
                return Err(HeifError::unsupported(format!(
                    "VVC items code 8 – {VVC_MAX_DEPTH}-bit pictures (depth {d})"
                )))
            }
            d => coded_depth(source.bit_depth, d)?.min(VVC_MAX_DEPTH),
        };
        HeifPixelFormat::new(source.chroma, depth, false)
    }

    /// The layout an AVC item codes a planar `source` in (round 476,
    /// oxideav-h264 0.1.9): the source's chroma sampling at the coded
    /// depth ([`coded_depth`], 8 / 10 / 12 — the High-family layouts
    /// the registry encoder takes), except that a 4:0:0 source is
    /// coded 4:2:0 with neutral chroma — the registry encoder's own
    /// line for grey input (its `avcC` then announces 4:2:0, which is
    /// what the item decodes to).
    pub fn avc_coded_format(&self, source: HeifPixelFormat) -> Result<HeifPixelFormat> {
        let chroma = match source.chroma {
            Chroma::Mono => Chroma::Yuv420,
            other => other,
        };
        HeifPixelFormat::new(
            chroma,
            coded_depth(source.bit_depth, self.hevc_depth)?,
            false,
        )
    }
}

/// The depth a lossy item codes a `source_depth` picture at when the
/// caller gives none: 8 → 8, 9–10 → 10, 11–12 → 12, deeper → 10
/// (a 16-bit source carries no 16 significant bits; Main 10 is the
/// layout every reader opens). An explicit `depth` must be 8 / 10 /
/// 12.
pub fn coded_depth(source_depth: u8, depth: Option<u8>) -> Result<u8> {
    Ok(match depth {
        Some(d @ (8 | 10 | 12)) => d,
        Some(other) => {
            return Err(HeifError::unsupported(format!(
                "coded depth {other} (8, 10 or 12)"
            )))
        }
        None => match source_depth {
            ..=8 => 8,
            9..=10 => 10,
            11..=12 => 12,
            _ => 10,
        },
    })
}

/// The HEVC coding-tree block size `rd` / `tiles` need (they run on
/// the quadtree coder, which the encoder selects through `ctb`): the
/// largest of 64 / 32 / 16 whose CTB grid over a `width × height`
/// picture has at least as many columns and rows as the `tiles`
/// layout asks for (64 without a layout).
pub fn auto_ctb(width: u32, height: u32, tiles: Option<&str>) -> u32 {
    let (cols, rows) = tiles
        .and_then(|t| t.split_once(['x', 'X']))
        .and_then(|(c, r)| Some((c.trim().parse::<u32>().ok()?, r.trim().parse::<u32>().ok()?)))
        .unwrap_or((1, 1));
    [64u32, 32, 16]
        .into_iter()
        .find(|c| width.div_ceil(*c) >= cols && height.div_ceil(*c) >= rows)
        .unwrap_or(16)
}

#[doc(hidden)]
/// A coded picture ready to become an item.
#[derive(Clone, Debug)]
pub struct CodedPicture {
    /// Item type.
    pub item_type: [u8; 4],
    /// Item payload (length-prefixed NAL units / AV1 temporal unit).
    pub data: Vec<u8>,
    /// Decoder configuration property.
    pub config: Property,
    /// Coded picture size (before any `clap`).
    pub coded_width: u32,
    /// Coded picture size.
    pub coded_height: u32,
    /// Sample layout of the coded picture.
    pub layout: HeifPixelFormat,
}

/// Align a dimension up to `n`.
fn align_up(v: u32, n: u32) -> u32 {
    v.div_ceil(n) * n
}

#[doc(hidden)]
/// Pad a frame to `w × h` by edge replication (encoders need aligned
/// sizes; the visible extent is restored with `clap`).
pub fn pad_frame(f: &HeifFrame, w: u32, h: u32) -> Result<HeifFrame> {
    if (w, h) == (f.width, f.height) {
        return Ok(f.tight());
    }
    let mut out = HeifFrame::zeroed(w, h, f.format)?;
    for p in 0..f.format.plane_count() {
        let (sw, sh) = f.plane_dims(p);
        let (dw, dh) = out.plane_dims(p);
        for y in 0..dh {
            let sy = y.min(sh - 1);
            for x in 0..dw {
                let sx = x.min(sw - 1);
                out.set_sample(p, x, y, f.sample(p, sx, sy));
            }
        }
    }
    Ok(out)
}

#[doc(hidden)]
/// Convert a frame to 8-bit 4:2:0 (the layout the historical HEVC
/// coder accepts): monochrome gains neutral chroma, 4:2:2 / 4:4:4
/// chroma is box-averaged, depths above 8 are rounded down.
pub fn to_yuv420_8(f: &HeifFrame) -> Result<HeifFrame> {
    to_yuv420(f, 8)
}

/// Convert a frame to 4:2:0 at `depth` bits (8 / 10 / 12 / 16):
/// monochrome gains neutral chroma, 4:2:2 / 4:4:4 chroma is
/// box-averaged (rounded to nearest), deeper sources are rounded down
/// to `depth`, shallower ones scaled up by bit replication. A frame
/// already in that layout comes back tightly packed. The alpha plane
/// is dropped. [`to_layout`] with [`Chroma::Yuv420`].
pub fn to_yuv420(f: &HeifFrame, depth: u8) -> Result<HeifFrame> {
    to_layout(f, Chroma::Yuv420, depth)
}

/// Convert a frame to the colour layout `chroma` at `depth` bits —
/// the coding layout of an item: a monochrome target keeps the luma
/// alone, a monochrome source gains neutral chroma, chroma more
/// finely sampled than the target is box-averaged over the luma
/// block each target sample covers (rounded to nearest), chroma more
/// coarsely sampled is replicated from the co-sited sample, deeper
/// samples are rounded to `depth`, shallower ones scaled up by bit
/// replication. A frame already in that layout comes back tightly
/// packed. The alpha plane is dropped.
pub fn to_layout(f: &HeifFrame, chroma: Chroma, depth: u8) -> Result<HeifFrame> {
    let fmt = HeifPixelFormat::new(chroma, depth, false)?;
    if f.format.without_alpha() == fmt {
        let bps = fmt.bytes_per_sample();
        let planes = (0..fmt.plane_count())
            .map(|i| {
                let (w, h) = f.plane_dims(i);
                let stride = w as usize * bps;
                let mut data = Vec::with_capacity(stride * h as usize);
                for y in 0..h {
                    data.extend_from_slice(f.row(i, y));
                }
                crate::image::Plane { stride, data }
            })
            .collect();
        return Ok(HeifFrame {
            width: f.width,
            height: f.height,
            format: fmt,
            planes,
        });
    }
    let src_depth = f.format.bit_depth;
    let max = fmt.max_value() as u32;
    let round = |v: u32| -> u16 {
        if src_depth == depth {
            v as u16
        } else if src_depth > depth {
            let shift = src_depth - depth;
            ((v + (1 << (shift - 1))) >> shift).min(max) as u16
        } else {
            // Bit replication: 0 → 0, max → max.
            let shift = depth - src_depth;
            ((v << shift) | (v >> (src_depth - shift))).min(max) as u16
        }
    };
    let mut out = if chroma == Chroma::Mono {
        HeifFrame::zeroed(f.width, f.height, fmt)?
    } else {
        HeifFrame::filled(f.width, f.height, fmt, 1 << (depth - 1))?
    };
    for y in 0..f.height {
        for x in 0..f.width {
            out.set_sample(0, x, y, round(f.sample(0, x, y) as u32));
        }
    }
    if f.format.chroma != Chroma::Mono && chroma != Chroma::Mono {
        let (sx, sy) = f.format.chroma.shift();
        let (tx, ty) = chroma.shift();
        let (cw, ch) = out.plane_dims(1);
        for p in 1..3 {
            let (spw, sph) = f.plane_dims(p);
            for cy in 0..ch {
                for cx in 0..cw {
                    // Average the source chroma samples covering the
                    // luma block of this target position (one sample
                    // when the source is the coarser sampling).
                    let x0 = (cx << tx) >> sx;
                    let y0 = (cy << ty) >> sy;
                    let x1 = ((((cx + 1) << tx) - 1) >> sx).min(spw - 1);
                    let y1 = ((((cy + 1) << ty) - 1) >> sy).min(sph - 1);
                    let mut sum = 0u32;
                    let mut n = 0u32;
                    for yy in y0..=y1 {
                        for xx in x0..=x1 {
                            sum += f.sample(p, xx, yy) as u32;
                            n += 1;
                        }
                    }
                    out.set_sample(p, cx, cy, round((sum + n / 2) / n));
                }
            }
        }
    }
    Ok(out)
}

/// The two-byte NAL unit header for a parsed header.
fn nal_header_bytes(h: &oxideav_h265::NalHeader) -> [u8; 2] {
    [
        (h.nal_unit_type << 1) | (h.nuh_layer_id >> 5),
        ((h.nuh_layer_id & 0x1f) << 3) | ((h.temporal_id + 1) & 0x07),
    ]
}

/// Build an `avcC` record + `AVCItemData` (HEIF E.2.2: length-prefixed
/// NAL units of exactly one access unit) from an Annex B access unit;
/// returns `(record, item data, width, height, layout)` with the size
/// from the SPS (cropped, H.264 §7.4.2.1.1). The record is
/// `oxideav-h264`'s ISO/IEC 14496-15 §5.3.2.1 builder (profile /
/// compatibility / level / chroma format / bit depths derived from the
/// parameter sets); the item data keeps the VCL and SEI NAL units.
pub fn avc_item_from_annex_b(
    annex_b: &[u8],
) -> Result<(crate::avcc::AvcConfig, Vec<u8>, u32, u32, HeifPixelFormat)> {
    let record = oxideav_h264::avcc::AvcDecoderConfigurationRecord::from_annex_b(annex_b)
        .map_err(|e| HeifError::invalid(format!("AVC access unit: {e}")))?;
    let bytes = record
        .to_bytes()
        .map_err(|e| HeifError::invalid(format!("avcC: {e}")))?;
    let cfg = crate::avcc::AvcConfig::parse(&bytes)?;
    let sample = oxideav_h264::avcc::annex_b_to_length_prefixed(annex_b, 4, true)
        .map_err(|e| HeifError::invalid(format!("AVC access unit: {e}")))?;
    let data = avc_item_data(&sample)?;
    let (w, h) = avc_sps_size(&cfg)?;
    let layout = cfg.layout()?;
    Ok((cfg, data, w, h, layout))
}

/// The `AVCItemData` / sample of a 4-byte length-prefixed access unit:
/// its VCL and SEI NAL units (parameter sets ride the record, access
/// unit delimiters and filler are dropped).
fn avc_item_data(sample: &[u8]) -> Result<Vec<u8>> {
    let nals: Vec<&[u8]> = split_length_prefixed(sample, 4)?
        .into_iter()
        .filter(|n| matches!(n.first().map(|h| h & 0x1f), Some(1..=6)))
        .collect();
    if !nals.iter().any(|n| matches!(n[0] & 0x1f, 1..=5)) {
        return Err(HeifError::invalid("AVC access unit without VCL NAL units"));
    }
    join_length_prefixed(&nals, 4)
}

/// The SPS of an `avcC` record (its first array entry).
pub(crate) fn avcc_sps(cfg: &crate::avcc::AvcConfig) -> Result<oxideav_h264::sps::Sps> {
    let first = cfg
        .sps
        .first()
        .ok_or_else(|| HeifError::invalid("avcC without an SPS"))?;
    let parsed = oxideav_h264::nal::parse_nal_unit(first)
        .map_err(|e| HeifError::invalid(format!("AVC NAL parse: {e}")))?;
    oxideav_h264::sps::Sps::parse(&parsed.rbsp)
        .map_err(|e| HeifError::invalid(format!("AVC SPS parse: {e}")))
}

/// The cropped picture size an `avcC` record's SPS announces (H.264
/// §7.4.2.1.1).
pub(crate) fn avc_sps_size(cfg: &crate::avcc::AvcConfig) -> Result<(u32, u32)> {
    let sps = avcc_sps(cfg)?;
    // Cropped picture size (§7.4.2.1.1): CropUnitX / CropUnitY from the
    // chroma format (1 for 4:0:0 and separate planes), the map-unit
    // height doubled for field coding.
    let chroma_idc = sps.chroma_format_idc as u8;
    let chroma_array_type = if sps.separate_colour_plane_flag {
        0
    } else {
        chroma_idc
    };
    let (sub_w, sub_h) = match chroma_array_type {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let frame_mbs_only = sps.frame_mbs_only_flag as u32;
    let crop_unit_x = if chroma_array_type == 0 { 1 } else { sub_w };
    let crop_unit_y = if chroma_array_type == 0 { 1 } else { sub_h } * (2 - frame_mbs_only);
    let coded_w = (sps.pic_width_in_mbs_minus1 + 1) * 16;
    let coded_h: u32 = (2 - frame_mbs_only) * (sps.pic_height_in_map_units_minus1 + 1) * 16;
    let (w, h) = match &sps.frame_cropping {
        Some(c) => (
            coded_w
                .checked_sub(crop_unit_x * (c.left + c.right))
                .ok_or_else(|| HeifError::invalid("AVC SPS crop wider than the picture"))?,
            coded_h
                .checked_sub(crop_unit_y * (c.top + c.bottom))
                .ok_or_else(|| HeifError::invalid("AVC SPS crop taller than the picture"))?,
        ),
        None => (coded_w, coded_h),
    };
    Ok((w, h))
}

/// Build an `hvcC` record + `HEVCItemData` from an Annex B access unit.
pub fn hevc_item_from_annex_b(
    annex_b: &[u8],
) -> Result<(HevcConfig, Vec<u8>, u32, u32, HeifPixelFormat)> {
    let nals = oxideav_h265::collect_nal_units(annex_b)
        .map_err(|e| HeifError::invalid(format!("HEVC Annex B split: {e}")))?;
    let mut arrays: Vec<NalArray> = Vec::new();
    let mut vcl: Vec<Vec<u8>> = Vec::new();
    let mut sps: Option<oxideav_h265::SeqParameterSet> = None;
    for n in &nals {
        let mut coded = nal_header_bytes(&n.header).to_vec();
        coded.extend_from_slice(&n.escaped);
        let t = n.header.nal_unit_type;
        match t {
            NAL_VPS | NAL_SPS | NAL_PPS => {
                if t == NAL_SPS && sps.is_none() {
                    sps = Some(
                        oxideav_h265::SeqParameterSet::parse(&n.rbsp)
                            .map_err(|e| HeifError::invalid(format!("SPS parse: {e}")))?,
                    );
                }
                match arrays.iter_mut().find(|a| a.nal_unit_type == t) {
                    Some(a) => a.nal_units.push(coded),
                    None => arrays.push(NalArray {
                        complete: true,
                        reserved_bit: false,
                        nal_unit_type: t,
                        nal_units: vec![coded],
                    }),
                }
            }
            _ if t < 32 => vcl.push(coded),
            _ => {} // SEI / AUD / EOS ride out; MIAF items need none.
        }
    }
    let sps = sps.ok_or_else(|| HeifError::invalid("HEVC access unit without an SPS"))?;
    if vcl.is_empty() {
        return Err(HeifError::invalid("HEVC access unit without VCL NAL units"));
    }
    arrays.sort_by_key(|a| a.nal_unit_type);
    let ptl = &sps.ptl;
    let mut cfg = HevcConfig {
        configuration_version: 1,
        general_profile_space: ptl.general_profile_space,
        general_tier_flag: ptl.general_tier_flag,
        general_profile_idc: ptl.general_profile_idc,
        general_profile_compatibility_flags: profile_compat_flags(ptl.general_profile_idc),
        general_constraint_indicator_flags: 0,
        general_level_idc: ptl.general_level_idc,
        min_spatial_segmentation_idc: 0,
        parallelism_type: 0,
        chroma_format_idc: sps.chroma_format_idc,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        avg_frame_rate: 0,
        constant_frame_rate: 0,
        num_temporal_layers: 1,
        temporal_id_nested: true,
        length_size: 4,
        arrays,
        raw: Vec::new(),
    };
    cfg.raw = cfg.serialize();
    let refs: Vec<&[u8]> = vcl.iter().map(Vec::as_slice).collect();
    let data = join_length_prefixed(&refs, 4)?;
    let cw = &sps.conformance_window;
    let (sub_w, sub_h) = match sps.chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let width = sps.pic_width_in_luma_samples - sub_w * (cw.left_offset + cw.right_offset);
    let height = sps.pic_height_in_luma_samples - sub_h * (cw.top_offset + cw.bottom_offset);
    let layout = HeifPixelFormat::new(
        Chroma::from_idc(sps.chroma_format_idc).unwrap_or(Chroma::Yuv420),
        sps.bit_depth_luma_minus8 + 8,
        false,
    )?;
    Ok((cfg, data, width, height, layout))
}

/// `general_profile_compatibility_flags` with the bit of `profile_idc`
/// set (bit 31 ↔ profile 0), plus the Main flag for Main Still Picture
/// and Main 10 (H.265 A.3.2 / A.3.3 compatibility rules).
fn profile_compat_flags(profile_idc: u8) -> u32 {
    let mut f = 0u32;
    if profile_idc < 32 {
        f |= 1u32 << (31 - profile_idc);
    }
    if profile_idc == 3 || profile_idc == 2 {
        f |= 1u32 << 30; // Main-compatible
    }
    if profile_idc == 3 {
        f |= 1u32 << 29; // Main 10-compatible
    }
    f
}

#[doc(hidden)]
/// Encode one 4:2:0 / monochrome picture as an HEVC item (serial;
/// see [`encode_hevc_picture_owned`]). `w`/`h` must be multiples of
/// 16 (the encoder's constraint); use [`pad_frame`].
pub fn encode_hevc_picture(frame: &HeifFrame, mode: &str, qp: u8) -> Result<CodedPicture> {
    encode_hevc_picture_with(frame, mode, qp, &[])
}

/// The HEVC encoder options that carry the item's colour information
/// into the bitstream VUI (§E.2.1 `video_signal_type`: range + H.273
/// colour description — the field an OS image reader takes the sample
/// range from) and mark the access unit as a Main Still Picture.
fn hevc_signal_options(colr: &Colr, opts: &EncodeOptions) -> Vec<(String, String)> {
    let mut v = vec![("still".to_string(), "1".to_string())];
    match colr {
        Colr::Nclx {
            primaries,
            transfer,
            matrix,
            full_range,
        } => {
            v.push((
                "range".into(),
                if *full_range { "full" } else { "limited" }.into(),
            ));
            v.push(("colorprim".into(), primaries.to_string()));
            v.push(("transfer".into(), transfer.to_string()));
            v.push(("matrix".into(), matrix.to_string()));
        }
        // ICC / other: full range (the MIAF default), no description.
        _ => v.push(("range".into(), "full".into())),
    }
    if let Some(rd) = opts.hevc_rd {
        v.push(("rd".into(), rd.min(2).to_string()));
    }
    if let Some(t) = &opts.hevc_tiles {
        v.push(("tiles".into(), t.clone()));
    }
    v
}

#[doc(hidden)]
/// [`encode_hevc_picture`] with extra codec options (`ctb`, `vpsid`,
/// …) passed through to the HEVC encoder.
pub fn encode_hevc_picture_with(
    frame: &HeifFrame,
    mode: &str,
    qp: u8,
    extra: &[(&str, &str)],
) -> Result<CodedPicture> {
    encode_hevc_picture_owned(frame.clone(), mode, qp, extra, 1)
}

/// The framework pixel format of an HEVC coded layout (4:2:0 or
/// monochrome at 8 / 10 / 12 bits).
fn hevc_input_format(f: HeifPixelFormat) -> Result<oxideav_core::PixelFormat> {
    use oxideav_core::PixelFormat as P;
    Ok(match (f.chroma, f.bit_depth) {
        (Chroma::Yuv420, 8) => P::Yuv420P,
        (Chroma::Yuv420, 10) => P::Yuv420P10Le,
        (Chroma::Yuv420, 12) => P::Yuv420P12Le,
        (Chroma::Yuv420, 16) => P::Yuv420P16Le,
        (Chroma::Mono, 8) => P::Gray8,
        (Chroma::Mono, 10) => P::Gray10Le,
        (Chroma::Mono, 12) => P::Gray12Le,
        (Chroma::Mono, 16) => P::Gray16Le,
        (Chroma::Yuv422, 8) => P::Yuv422P,
        (Chroma::Yuv422, 10) => P::Yuv422P10Le,
        (Chroma::Yuv422, 12) => P::Yuv422P12Le,
        (Chroma::Yuv444, 8) => P::Yuv444P,
        (Chroma::Yuv444, 10) => P::Yuv444P10Le,
        (Chroma::Yuv444, 12) => P::Yuv444P12Le,
        _ => {
            return Err(HeifError::unsupported(format!(
                "HEVC item encoding: no coded layout for {:?} {}-bit",
                f.chroma, f.bit_depth
            )))
        }
    })
}

/// Encode one picture as an HEVC item, moving its planes into the
/// codec's input (no copy on this side): 4:2:0 / monochrome at 8 /
/// 10 / 12 bits (16 on `pcm`), `w` / `h` multiples of 16 (the
/// encoder's constraint; see [`pad_frame`]). `threads` is handed to
/// the encoder as its `ExecutionContext` (the quadtree coder's
/// wavefront / tile fan-out; the historical coder ignores it).
pub fn encode_hevc_picture_owned(
    frame: HeifFrame,
    mode: &str,
    qp: u8,
    extra: &[(&str, &str)],
    threads: usize,
) -> Result<CodedPicture> {
    if frame.format.has_alpha {
        return Err(HeifError::unsupported(
            "HEVC item encoding takes a colour-only picture (drop the alpha plane)",
        ));
    }
    let pf = hevc_input_format(frame.format)?;
    let mut params = CodecParameters::video(CodecId::new(crate::decode::CODEC_ID_HEVC));
    params.width = Some(frame.width);
    params.height = Some(frame.height);
    params.pixel_format = Some(pf);
    let mut options = CodecOptions::new()
        .set("mode", mode)
        .set("qp", qp.to_string());
    for (k, v) in extra {
        options = options.set(*k, *v);
    }
    params.options = options;
    let mut enc = oxideav_h265::make_encoder(&params)
        .map_err(|e| HeifError::unsupported(format!("HEVC encoder: {e}")))?;
    if threads > 1 {
        enc.set_execution_context(&oxideav_core::ExecutionContext::with_threads(threads));
    }
    let planes = frame
        .planes
        .into_iter()
        .map(|p| oxideav_core::VideoPlane {
            stride: p.stride,
            data: p.data,
        })
        .collect();
    let vf = oxideav_core::VideoFrame {
        pts: Some(0),
        planes,
    };
    enc.send_frame(&Frame::Video(vf))
        .map_err(|e| HeifError::invalid(format!("HEVC encoder rejected the frame: {e}")))?;
    enc.flush()
        .map_err(|e| HeifError::invalid(format!("HEVC encoder flush: {e}")))?;
    let mut annex_b = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => annex_b.extend_from_slice(&p.data),
            Err(CoreError::NeedMore) | Err(CoreError::Eof) => break,
            Err(e) => return Err(HeifError::invalid(format!("HEVC encoder: {e}"))),
        }
    }
    drop(enc);
    let (cfg, data, w, h, layout) = hevc_item_from_annex_b(&annex_b)?;
    Ok(CodedPicture {
        item_type: ITEM_TYPE_HVC1,
        data,
        config: Property::HvcC(cfg),
        coded_width: w,
        coded_height: h,
        layout,
    })
}

#[doc(hidden)]
/// The registry pixel format an AVC item codes `fmt` in, and the
/// registry encoder options of `opts` for it (round 476): `rc=cqp` at
/// `opts.qp`, `lossless=1` under `pcm` (High 4:4:4 Predictive
/// transform bypass; the decoded picture equals the source),
/// [`EncodeOptions::avc_options`] on top (`cabac`, `profile`,
/// `transform_8x8`, `deblock`, `intra_4x4`, `gop`, …), and
/// `format=avcc` last: the packets are 4-byte length-prefixed samples
/// and the encoder's output parameters carry the `avcC` record.
fn avc_encoder_setup(
    fmt: HeifPixelFormat,
    width: u32,
    height: u32,
    opts: &EncodeOptions,
    extra: &[(&str, &str)],
) -> Result<CodecParameters> {
    let pf = HeifPixelFormat::new(fmt.chroma, fmt.bit_depth, false)?
        .to_core()
        .filter(|pf| oxideav_h264::h264_encoder::accepted_pixel_formats().contains(pf))
        .ok_or_else(|| {
            HeifError::unsupported(format!(
                "AVC item encoding: the registry encoder takes 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 \
                 at 8 / 10 / 12 bits, not {:?} {}-bit",
                fmt.chroma, fmt.bit_depth
            ))
        })?;
    if width % 16 != 0 || height % 16 != 0 {
        return Err(HeifError::invalid(format!(
            "AVC item encoding: {width}x{height} is not macroblock-aligned (pad to 16)"
        )));
    }
    let mut params = CodecParameters::video(CodecId::new(crate::decode::CODEC_ID_AVC));
    params.width = Some(width);
    params.height = Some(height);
    params.pixel_format = Some(pf);
    let mut options = CodecOptions::new()
        .set("rc", "cqp")
        .set("qp", opts.qp.min(51).to_string());
    if opts.hevc_mode == "pcm" {
        options = options.set("lossless", "1");
    }
    for (k, v) in extra {
        options = options.set(*k, *v);
    }
    for (k, v) in &opts.avc_options {
        options = options.set(k.as_str(), v.as_str());
    }
    params.options = options.set("format", "avcc");
    Ok(params)
}

/// Run `frame` through an AVC registry encoder `enc` as its next
/// picture (`pts`): the access units it yields.
fn avc_encode_frame(
    enc: &mut dyn Encoder,
    frame: HeifFrame,
    pts: i64,
    flush: bool,
) -> Result<Vec<Packet>> {
    let planes = frame
        .planes
        .into_iter()
        .map(|p| oxideav_core::VideoPlane {
            stride: p.stride,
            data: p.data,
        })
        .collect();
    let vf = oxideav_core::VideoFrame {
        pts: Some(pts),
        planes,
    };
    enc.send_frame(&Frame::Video(vf))
        .map_err(|e| HeifError::invalid(format!("AVC encoder rejected the frame: {e}")))?;
    if flush {
        enc.flush()
            .map_err(|e| HeifError::invalid(format!("AVC encoder flush: {e}")))?;
    }
    let mut packets = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(CoreError::NeedMore) | Err(CoreError::Eof) => break,
            Err(e) => return Err(HeifError::invalid(format!("AVC encoder: {e}"))),
        }
    }
    Ok(packets)
}

/// The `avcC` record an AVC registry encoder announces (`format=avcc`:
/// its output parameters' `extradata`).
fn avc_encoder_record(enc: &dyn Encoder) -> Result<crate::avcc::AvcConfig> {
    let extradata = &enc.output_params().extradata;
    if extradata.is_empty() {
        return Err(HeifError::invalid(
            "AVC encoder: no avcC record in the output parameters",
        ));
    }
    crate::avcc::AvcConfig::parse(extradata)
}

/// Encode one picture as an AVC item (HEIF Annex E `avc1` + `avcC`)
/// through `oxideav-h264`'s registry encoder: one IDR picture at
/// `opts.qp` (`rc=cqp`), CAVLC Baseline / CABAC Main (`avc_options`
/// `cabac=1`) for 8-bit 4:2:0, the §A.2 minimum High-family profile
/// for every other layout (4:2:2 / 4:4:4, 10 / 12 bits; round 476,
/// oxideav-h264 0.1.9) or an explicit `profile`; lossless (High 4:4:4
/// Predictive transform bypass) under `opts.hevc_mode == "pcm"`. `w` /
/// `h` are multiples of 16 (see [`pad_frame`]). The encoder's `avcC`
/// record (`format=avcc`) is the item's, its length-prefixed sample
/// the `AVCItemData`.
pub fn encode_avc_picture_owned(frame: HeifFrame, opts: &EncodeOptions) -> Result<CodedPicture> {
    if frame.format.has_alpha {
        return Err(HeifError::unsupported(
            "AVC item encoding takes a colour-only picture (drop the alpha plane)",
        ));
    }
    let params = avc_encoder_setup(frame.format, frame.width, frame.height, opts, &[])?;
    let mut enc = oxideav_h264::h264_encoder::make_encoder(&params)
        .map_err(|e| HeifError::unsupported(format!("AVC encoder: {e}")))?;
    let packets = avc_encode_frame(enc.as_mut(), frame, 0, true)?;
    let [packet] = packets.as_slice() else {
        return Err(HeifError::invalid(format!(
            "AVC item encoding: {} access units for one picture",
            packets.len()
        )));
    };
    let cfg = avc_encoder_record(enc.as_ref())?;
    drop(enc);
    let data = avc_item_data(&packet.data)?;
    let (w, h) = avc_sps_size(&cfg)?;
    let layout = cfg.layout()?;
    Ok(CodedPicture {
        item_type: ITEM_TYPE_AVC1,
        data,
        config: Property::AvcC(cfg),
        coded_width: w,
        coded_height: h,
        layout,
    })
}

/// The sample of an AVC image sequence from an Annex B access unit:
/// its VCL and SEI NAL units 4-byte length-prefixed (parameter sets
/// ride in the `avc1` sample entry, access unit delimiters are
/// dropped). Access units with an SPS go through
/// [`avc_item_from_annex_b`] for the record.
pub fn avc_sample_from_annex_b(annex_b: &[u8]) -> Result<Vec<u8>> {
    let sample = oxideav_h264::avcc::annex_b_to_length_prefixed(annex_b, 4, true)
        .map_err(|e| HeifError::invalid(format!("AVC access unit: {e}")))?;
    avc_item_data(&sample)
}

/// One `oxideav-h264` registry encoder session for an AVC image
/// sequence (HEIF Annex E.3): an IDR picture followed by P pictures
/// (`gop` = the frame count unless `avc_options` says otherwise — an
/// all-IDR `gop=1` sequence is fine: the encoder rotates `idr_pic_id`
/// per H.264 §7.4.3 itself). Every frame yields exactly one access unit
/// (B pictures, which reorder, are refused); the session's `avcC`
/// record (`format=avcc`) becomes the `avc1` sample entry's; every
/// High-family layout the registry encoder takes codes as it is.
struct AvcSequenceEncoder {
    enc: Option<Box<dyn Encoder>>,
    frames: usize,
    sent: i64,
    cfg: Option<crate::avcc::AvcConfig>,
    size: (u32, u32),
}

impl AvcSequenceEncoder {
    fn new(frames: usize) -> Self {
        Self {
            enc: None,
            frames,
            sent: 0,
            cfg: None,
            size: (0, 0),
        }
    }

    /// Code `frame` (macroblock-aligned, a layout the registry encoder
    /// takes) as the next sample: the coded picture and whether it is a
    /// sync sample.
    fn encode(&mut self, frame: HeifFrame, opts: &EncodeOptions) -> Result<(CodedPicture, bool)> {
        if frame.format.has_alpha {
            return Err(HeifError::unsupported(
                "AVC sequence encoding takes colour-only pictures (drop the alpha plane)",
            ));
        }
        if self.enc.is_none() {
            let gop = self.frames.max(1).to_string();
            let params = avc_encoder_setup(
                frame.format,
                frame.width,
                frame.height,
                opts,
                &[("gop", gop.as_str())],
            )?;
            let enc = oxideav_h264::h264_encoder::make_encoder(&params)
                .map_err(|e| HeifError::unsupported(format!("AVC encoder: {e}")))?;
            self.cfg = Some(avc_encoder_record(enc.as_ref())?);
            self.enc = Some(enc);
            self.size = (frame.width, frame.height);
        } else if (frame.width, frame.height) != self.size {
            return Err(HeifError::invalid(
                "AVC sequence encoding: frames of different coded sizes",
            ));
        }
        let enc = self.enc.as_mut().expect("created above");
        let packets = avc_encode_frame(enc.as_mut(), frame, self.sent, false)?;
        self.sent += 1;
        let [packet] = packets.as_slice() else {
            return Err(HeifError::unsupported(format!(
                "AVC sequence encoding: {} access units for one frame (B pictures reorder; \
                 a sequence takes one access unit per frame in display order)",
                packets.len()
            )));
        };
        let data = avc_item_data(&packet.data)?;
        let cfg = self.cfg.clone().expect("recorded with the encoder");
        let layout = cfg.layout()?;
        Ok((
            CodedPicture {
                item_type: ITEM_TYPE_AVC1,
                data,
                config: Property::AvcC(cfg),
                coded_width: self.size.0,
                coded_height: self.size.1,
                layout,
            },
            packet.is_keyframe(),
        ))
    }
}

#[doc(hidden)]
/// Encode one picture as an AV1 still item (`reduced_still_picture_header`)
/// at its own (depth, chroma) pairing — 8 / 10 / 12-bit, 4:0:0 /
/// 4:2:0 / 4:2:2 / 4:4:4 — lossless when `opts.av1_quality` is `None`
/// or 100, else at that quality. Dimensions must be multiples of 8.
pub fn encode_av1_picture(frame: &HeifFrame, opts: &EncodeOptions) -> Result<CodedPicture> {
    encode_av1_picture_owned(frame.without_alpha(), opts)
}

/// [`encode_av1_picture`] over an owned colour-only picture: the
/// planes are widened into the codec's `u16` input plane by plane
/// (each source plane freed as it goes). The tile layout is derived
/// from the picture size for [`AV1_TILE_LAYOUT_THREADS`] workers and
/// the search runs on `opts.threads`, so the bytes never depend on the
/// budget.
pub fn encode_av1_picture_owned(frame: HeifFrame, opts: &EncodeOptions) -> Result<CodedPicture> {
    use oxideav_av1::encoder::{
        encode_still_yuv, still::auto_tile_layout, ChromaFormat, StillOptions, StillSpeed, YuvFrame,
    };
    if !matches!(frame.format.bit_depth, 8 | 10 | 12) {
        return Err(HeifError::unsupported(format!(
            "AV1 items code 8 / 10 / 12-bit pictures, not {}-bit",
            frame.format.bit_depth
        )));
    }
    if frame.format.has_alpha {
        return Err(HeifError::unsupported(
            "AV1 item encoding takes a colour-only picture (drop the alpha plane)",
        ));
    }
    let format = match frame.format.chroma {
        Chroma::Mono => ChromaFormat::Monochrome,
        Chroma::Yuv420 => ChromaFormat::Yuv420,
        Chroma::Yuv422 => ChromaFormat::Yuv422,
        Chroma::Yuv444 => ChromaFormat::Yuv444,
    };
    let (width, height, fmt) = (frame.width, frame.height, frame.format);
    let bps = fmt.bytes_per_sample();
    let mut planes = frame.planes.into_iter();
    let mut plane16 = |p: usize| -> Vec<u16> {
        let plane = planes.next().expect("validated plane count");
        let (w, h) = fmt.plane_dims(p, width, height);
        let mut v = Vec::with_capacity((w * h) as usize);
        for y in 0..h as usize {
            let row = &plane.data[y * plane.stride..y * plane.stride + w as usize * bps];
            if bps == 1 {
                v.extend(row.iter().map(|b| *b as u16));
            } else {
                v.extend(
                    row.chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]])),
                );
            }
        }
        v
    };
    let y = plane16(0);
    let (u, v) = if fmt.chroma == Chroma::Mono {
        (Vec::new(), Vec::new())
    } else {
        (plane16(1), plane16(2))
    };
    drop(planes);
    let input = YuvFrame {
        width,
        height,
        bit_depth: fmt.bit_depth,
        format,
        y,
        u,
        v,
    };
    let mut so = match opts.av1_quality {
        None | Some(100..) => StillOptions::from_quality(100),
        Some(q) => StillOptions::from_quality(q),
    };
    so.speed = match opts.av1_speed.as_str() {
        "balanced" => StillSpeed::Balanced,
        "thorough" => StillSpeed::Thorough,
        _ => StillSpeed::Fast,
    };
    so.reduced_header = true;
    let (cols, rows) = auto_tile_layout(width, height, AV1_TILE_LAYOUT_THREADS);
    so.tile_cols_log2 = cols;
    so.tile_rows_log2 = rows;
    so.auto_tiles = false;
    so.threads = opts.workers();
    match &opts.colr {
        Colr::Nclx {
            primaries,
            transfer,
            matrix,
            full_range,
        } => {
            so.full_range = *full_range;
            so.color_description = Some((*primaries as u8, *transfer as u8, *matrix as u8));
        }
        _ => {
            so.full_range = true;
            so.color_description = None;
        }
    }
    let still = encode_still_yuv(&input, &so)
        .map_err(|e| HeifError::unsupported(format!("AV1 still encoder: {e:?}")))?;
    let raw = still.codec_config.to_bytes();
    let cfg = Av1Config::parse(&raw)?;
    let layout = crate::decode::av1_layout(&cfg)?;
    Ok(CodedPicture {
        item_type: ITEM_TYPE_AV01,
        data: still.temporal_unit_bytes,
        config: Property::Av1C(cfg),
        coded_width: width,
        coded_height: height,
        layout,
    })
}

/// The `oxideav-h266` encoder configuration for a `width × height`
/// picture under [`EncodeOptions::vvc_options`].
fn vvc_encoder_config(
    width: u32,
    height: u32,
    opts: &EncodeOptions,
) -> Result<oxideav_h266::encoder::EncoderConfig> {
    let mut cfg = oxideav_h266::encoder::EncoderConfig::new(width, height);
    let flag = |k: &str, v: &str| -> Result<bool> {
        match v {
            "1" | "true" | "on" => Ok(true),
            "0" | "false" | "off" => Ok(false),
            other => Err(HeifError::invalid(format!(
                "VVC option {k}: '{other}' (0 / 1)"
            ))),
        }
    };
    for (k, v) in &opts.vvc_options {
        match k.as_str() {
            "tiles" => {
                let (c, r) = v
                    .split_once(['x', 'X'])
                    .and_then(|(c, r)| {
                        Some((c.trim().parse::<u8>().ok()?, r.trim().parse::<u8>().ok()?))
                    })
                    .ok_or_else(|| HeifError::invalid(format!("VVC option tiles: '{v}' (CxR)")))?;
                cfg.tile_columns = c.max(1);
                cfg.tile_rows = r.max(1);
            }
            "wpp" => cfg.wpp = flag(k, v)?,
            "dep_quant" => cfg.dep_quant = flag(k, v)?,
            "sdh" => cfg.sign_data_hiding = flag(k, v)?,
            "palette" => cfg.palette = flag(k, v)?,
            "mtt_bt" => cfg.enable_mtt_bt_picker = flag(k, v)?,
            "mtt_tt" => cfg.enable_mtt_tt_picker = flag(k, v)?,
            "alf_clip_rdo" => cfg.enable_alf_clip_rdo = flag(k, v)?,
            "chroma_sao_merge" => cfg.enable_chroma_sao_merge = flag(k, v)?,
            "loop_filter_across_tiles" => cfg.loop_filter_across_tiles = flag(k, v)?,
            "slice_per_tile" => cfg.slice_per_tile = flag(k, v)?,
            "raster_slices" => {
                cfg.raster_slice_count = v.trim().parse::<u8>().map_err(|_| {
                    HeifError::invalid(format!("VVC option raster_slices: '{v}' (a count)"))
                })?;
            }
            other => {
                return Err(HeifError::invalid(format!(
                    "unknown VVC encoder option '{other}'"
                )))
            }
        }
    }
    Ok(cfg)
}

/// Alignment of the pictures handed to the VVC item encoder (round
/// 476, oxideav-h266 0.0.10): the encoder codes ANY picture size —
/// it pads the coded size to its own `encoder::CODED_SIZE_ALIGN` and
/// crops back with the SPS conformance window (H.266 §7.4.3.4) — so
/// this crate only evens the dimensions (the window is expressed in
/// chroma units, which cannot cut an odd 4:2:0 / 4:2:2 edge exactly);
/// the odd column / row comes off with `clap` as on every codec.
pub const VVC_ALIGNMENT: u32 = 2;

/// The deepest layout the VVC encoder codes (oxideav-h266 0.0.10:
/// 8 – 10 bits at 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4).
pub const VVC_MAX_DEPTH: u8 = 10;

#[doc(hidden)]
/// Encode one picture as a VVC item (HEIF Annex L) through
/// `oxideav-h266`'s IDR encoder at `opts.qp` (lossless under
/// `opts.hevc_mode == "pcm"`: transform-skip coding at `Qp′ =
/// QpPrimeTsMin`, the decoded picture equals the source), with
/// [`EncodeOptions::vvc_options`]; 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 at
/// 8 – 10 bits (round 476), `w` / `h` multiples of [`VVC_ALIGNMENT`]
/// (see [`pad_frame`]). The encoder is serial.
pub fn encode_vvc_picture_owned(frame: HeifFrame, opts: &EncodeOptions) -> Result<CodedPicture> {
    if frame.format.has_alpha {
        return Err(HeifError::unsupported(
            "VVC item encoding takes a colour-only picture (drop the alpha plane)",
        ));
    }
    let fmt = frame.format;
    if !(8..=VVC_MAX_DEPTH).contains(&fmt.bit_depth) {
        return Err(HeifError::unsupported(format!(
            "VVC items code 8 – {VVC_MAX_DEPTH}-bit pictures, not {}-bit",
            fmt.bit_depth
        )));
    }
    if frame.width % VVC_ALIGNMENT != 0 || frame.height % VVC_ALIGNMENT != 0 {
        return Err(HeifError::invalid(format!(
            "VVC item encoding: {}x{} is not a multiple of {VVC_ALIGNMENT} (pad with clap)",
            frame.width, frame.height
        )));
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    let idc = fmt.chroma.idc();
    let mut pb = oxideav_h266::reconstruct::PictureBuffer::with_chroma_format_bd(
        w,
        h,
        u32::from(idc),
        0,
        u32::from(fmt.bit_depth),
    );
    let bps = fmt.bytes_per_sample();
    for (i, dst) in [&mut pb.luma, &mut pb.cb, &mut pb.cr]
        .into_iter()
        .enumerate()
        .take(fmt.plane_count())
    {
        let (pw, ph) = fmt.plane_dims(i, frame.width, frame.height);
        let src = &frame.planes[i];
        for y in 0..ph as usize {
            let row = &src.data[y * src.stride..y * src.stride + pw as usize * bps];
            let out = &mut dst.samples[y * dst.stride..y * dst.stride + pw as usize];
            if bps == 1 {
                for (o, s) in out.iter_mut().zip(row) {
                    *o = *s as u16;
                }
            } else {
                for (o, s) in out.iter_mut().zip(row.chunks_exact(2)) {
                    *o = u16::from_le_bytes([s[0], s[1]]);
                }
            }
        }
    }
    drop(frame);
    let mut cfg = vvc_encoder_config(w as u32, h as u32, opts)?;
    cfg.chroma_format_idc = idc;
    cfg.bit_depth = fmt.bit_depth;
    cfg.lossless = opts.hevc_mode == "pcm";
    let (annex_b, _rec) = oxideav_h266::encoder_pipeline::encode_idr_with_residuals_cfg(
        &pb,
        i32::from(opts.qp.min(63)),
        cfg,
    )
    .map_err(|e| HeifError::unsupported(format!("VVC encoder: {e}")))?;
    drop(pb);
    let (cfg, data, cw, ch, layout) = crate::vvcdec::vvc_item_from_annex_b(&annex_b)?;
    Ok(CodedPicture {
        item_type: ITEM_TYPE_VVC1,
        data,
        config: Property::VvcC(cfg),
        coded_width: cw,
        coded_height: ch,
        layout,
    })
}

fn encode_picture(frame: HeifFrame, opts: &EncodeOptions) -> Result<CodedPicture> {
    encode_picture_with(frame, opts, &[])
}

/// Codec options that give the alpha auxiliary's HEVC parameter sets
/// their own ids (VPS / SPS / PPS 1): Apple ImageIO refuses a file
/// whose two items carry byte-identical parameter sets (verified by
/// cross-muxing), and distinct ids make them differ on every mode.
const ALPHA_HEVC_OPTIONS: &[(&str, &str)] = &[("vpsid", "1"), ("spsid", "1"), ("ppsid", "1")];

fn encode_picture_with(
    frame: HeifFrame,
    opts: &EncodeOptions,
    extra: &[(&str, &str)],
) -> Result<CodedPicture> {
    encode_picture_for(frame, opts, &opts.colr, extra)
}

/// The HEVC encoder options of an item coded in `coded` layout: the
/// VUI colour signal, the quadtree coder's `ctb` / `rd` / `tiles` /
/// `wpp`, the in-loop filters and the caller's extras.
fn hevc_item_options(
    coded: HeifPixelFormat,
    width: u32,
    height: u32,
    colr: &Colr,
    opts: &EncodeOptions,
) -> Vec<(String, String)> {
    let mut signal = hevc_signal_options(colr, opts);
    let lossy = opts.hevc_mode != "pcm";
    if lossy {
        // `rd` / `tiles` ride the quadtree coder, which the encoder
        // enables through `ctb` (and which every non-8-bit-4:2:0 layout
        // runs on): supply it when the caller did not, and let the
        // wavefront carry the thread budget on a single-tile picture
        // (always on there, so the bytes never depend on the budget).
        if opts.hevc_quadtree(coded) {
            let ctb = opts
                .hevc_ctb
                .unwrap_or_else(|| auto_ctb(width, height, opts.hevc_tiles.as_deref()));
            signal.push(("ctb".into(), ctb.to_string()));
            if opts.hevc_tiles.is_none() {
                signal.push(("wpp".into(), "1".into()));
            }
            // The coder's own still default is the full-RD level 2
            // (~12x the level-0 CPU); a layout that merely needs the
            // quadtree coder keeps the fast search unless `rd` asks.
            if opts.hevc_rd.is_none() {
                signal.push(("rd".into(), "0".into()));
            }
        }
        if opts.hevc_filters.unwrap_or(HEVC_FILTERS_DEFAULT) {
            signal.push(("deblock".into(), "1".into()));
            signal.push(("sao".into(), "1".into()));
        }
    }
    signal.extend(opts.hevc_options.iter().cloned());
    signal
}

/// [`encode_picture_with`] for an item whose colour information is
/// `colr` (the alpha auxiliary signals its own range).
fn encode_picture_for(
    frame: HeifFrame,
    opts: &EncodeOptions,
    colr: &Colr,
    extra: &[(&str, &str)],
) -> Result<CodedPicture> {
    match opts.codec {
        StillCodec::Hevc => {
            let signal = hevc_item_options(frame.format, frame.width, frame.height, colr, opts);
            let mut all: Vec<(&str, &str)> = signal
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            all.extend_from_slice(extra);
            encode_hevc_picture_owned(frame, &opts.hevc_mode, opts.qp, &all, opts.workers())
        }
        StillCodec::Av1 => encode_av1_picture_owned(frame, opts),
        StillCodec::Vvc => encode_vvc_picture_owned(frame, opts),
        StillCodec::Avc => encode_avc_picture_owned(frame, opts),
    }
}

fn alignment(opts: &EncodeOptions) -> u32 {
    match opts.codec {
        StillCodec::Hevc | StillCodec::Avc => 16,
        StillCodec::Av1 => 8,
        StillCodec::Vvc => VVC_ALIGNMENT,
    }
}

/// The transformative chain of `opts` as essential property entries —
/// what the thumbnails and the gain map always carry (they are coded
/// pictures of the oriented output's companions), and what every
/// displayed item carries under [`TransformCarrier::CodedItem`], so
/// third-party readers that only honour properties on the item they
/// decode still render the intended orientation.
fn transform_props(opts: &EncodeOptions) -> Vec<(Property, bool)> {
    opts.transforms.iter().map(|t| (t.clone(), true)).collect()
}

/// The transforms of the primary's own items (coded picture / `grid` /
/// `iovl`, alpha and depth auxiliaries): the chain under
/// [`TransformCarrier::CodedItem`], nothing under
/// [`TransformCarrier::Identity`] (the `iden` carries it).
fn item_transform_props(opts: &EncodeOptions) -> Vec<(Property, bool)> {
    match opts.transform_carrier {
        TransformCarrier::Identity => Vec::new(),
        _ => transform_props(opts),
    }
}

/// `opts.transforms` must be the MIAF §7.3.6.7 chain — at most one
/// `clap` (first), then at most one `irot`, then at most one `imir` —
/// so the crop is in the primary's own coordinates (it is folded into
/// the padding crop of a coded item and cut off a thumbnail) and every
/// written item passes the order check.
fn validate_transforms(opts: &EncodeOptions) -> Result<()> {
    let mut last = 0u8;
    for t in &opts.transforms {
        let rank = match t {
            Property::Clap(_) => 1,
            Property::Irot(_) => 2,
            Property::Imir(_) => 3,
            other => {
                return Err(HeifError::unsupported(format!(
                    "transforms: '{}' is not a transformative property this encoder writes (clap / irot / imir)",
                    crate::boxes::fourcc_str(&other.box_type())
                )))
            }
        };
        if rank <= last {
            return Err(HeifError::invalid(
                "transforms: MIAF §7.3.6.7 order is clap → irot → imir, each at most once",
            ));
        }
        last = rank;
    }
    Ok(())
}

/// The caller's `clap`, when the chain has one.
fn user_clap(opts: &EncodeOptions) -> Option<&Clap> {
    opts.transforms.iter().find_map(|t| match t {
        Property::Clap(c) => Some(c),
        _ => None,
    })
}

/// The orientation part of the chain (`irot` / `imir`, essential): what
/// companions of another geometry — thumbnails, the depth map, the
/// gain map — carry; a crop is the primary's alone.
fn orientation_props(opts: &EncodeOptions) -> Vec<(Property, bool)> {
    opts.transforms
        .iter()
        .filter(|t| !matches!(t, Property::Clap(_)))
        .map(|t| (t.clone(), true))
        .collect()
}

/// One `clap` for a coded picture of `pic.coded_*` whose visible
/// picture is `vis`: the padding crop (origin kept, the codec
/// alignment rows / columns dropped) folded with the caller's crop
/// `user` (given in the visible picture's coordinates, relative to its
/// centre): the clean aperture is the caller's, its offsets move by
/// half the padding, exactly (ISO/IEC 14496-12 §12.1.4.1 rationals).
fn clap_composed(
    pic: &CodedPicture,
    vis: (u32, u32),
    user: Option<&Clap>,
) -> Option<(Property, bool)> {
    let (pw, ph) = (
        (pic.coded_width - vis.0) as i64,
        (pic.coded_height - vis.1) as i64,
    );
    match user {
        None => clap_for(pic, vis),
        Some(u) => {
            let fold = |n: i32, d: u32, pad: i64| -> (i32, u32) {
                let d = d.max(1) as i64;
                let (mut n2, mut d2) = (2 * n as i64 - pad * d, 2 * d);
                let g = gcd(n2.unsigned_abs(), d2 as u64).max(1) as i64;
                n2 /= g;
                d2 /= g;
                (
                    n2.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                    d2.min(u32::MAX as i64) as u32,
                )
            };
            let (hn, hd) = fold(u.horiz_off_n, u.horiz_off_d, pw);
            let (vn, vd) = fold(u.vert_off_n, u.vert_off_d, ph);
            Some((
                Property::Clap(Clap::new(
                    u.width_n, u.width_d, u.height_n, u.height_d, hn, hd, vn, vd,
                )),
                true,
            ))
        }
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// The transformative chain of a coded item with the primary's
/// geometry (the master picture, its alpha plane): the single folded
/// `clap`, then the orientation — under [`TransformCarrier::CodedItem`];
/// the padding crop alone under [`TransformCarrier::Identity`] (the
/// `iden` carries the caller's chain over the visible picture).
fn coded_item_chain(
    pic: &CodedPicture,
    vis: (u32, u32),
    opts: &EncodeOptions,
) -> Vec<(Property, bool)> {
    match opts.transform_carrier {
        TransformCarrier::Identity => clap_for(pic, vis).into_iter().collect(),
        _ => {
            let mut v: Vec<(Property, bool)> = clap_composed(pic, vis, user_clap(opts))
                .into_iter()
                .collect();
            v.extend(orientation_props(opts));
            v
        }
    }
}

/// The chain of a companion coded picture of its own geometry: its
/// padding crop, then the orientation (always for thumbnails and the
/// gain map — they picture the oriented output; for the depth map only
/// under [`TransformCarrier::CodedItem`], the `iden` orienting it
/// otherwise).
fn companion_chain(
    pic: &CodedPicture,
    vis: (u32, u32),
    opts: &EncodeOptions,
    oriented: bool,
) -> Vec<(Property, bool)> {
    let mut v: Vec<(Property, bool)> = clap_for(pic, vis).into_iter().collect();
    if oriented {
        v.extend(orientation_props(opts));
    }
    v
}

/// The descriptive properties of a derived item over a picture in
/// layout `cfmt`: the `colr` set of `opts` (`nclx`, the ICC profile)
/// and a `pixi` — what a `grid`, `iovl` or `iden` carries so it stands
/// on its own (a `tmap` base needs a `colr`, HEIF Amd 1 §6.6.2.4.1).
fn derived_props(cfmt: HeifPixelFormat, opts: &EncodeOptions) -> Vec<(Property, bool)> {
    let mut props = vec![(
        Property::Colr(nclx_for_icc(&opts.colr, opts.icc_profile.is_some())),
        false,
    )];
    if let Some(icc) = &opts.icc_profile {
        props.push((
            Property::Colr(Colr::Icc {
                restricted: false,
                profile: icc.clone(),
            }),
            false,
        ));
    }
    props.push((
        Property::Pixi(Pixi {
            bits_per_channel: vec![cfmt.bit_depth; cfmt.chroma.colour_planes()],
        }),
        false,
    ));
    props
}

/// Under [`TransformCarrier::Identity`] with a non-empty chain: an
/// `iden` derived item over `input` (a `width × height` picture in
/// layout `cfmt`) carrying the descriptive set and the chain, the new
/// primary; otherwise `input` itself.
fn identity_over(
    w: &mut HeifWriter,
    input: u32,
    width: u32,
    height: u32,
    cfmt: HeifPixelFormat,
    opts: &EncodeOptions,
) -> u32 {
    if opts.transform_carrier != TransformCarrier::Identity || opts.transforms.is_empty() {
        return input;
    }
    let mut props = vec![(Property::Ispe(Ispe { width, height }), false)];
    props.extend(derived_props(cfmt, opts));
    props.extend(transform_props(opts));
    // The input is an intermediate, not an alternative: hidden (§6.4.2).
    w.set_hidden(input, true);
    w.add_identity(input, props)
}

/// Codec options that give the depth auxiliary's HEVC parameter sets
/// their own ids (VPS / SPS / PPS 3; the alpha uses 1, the gain map 2).
const DEPTH_HEVC_OPTIONS: &[(&str, &str)] = &[("vpsid", "3"), ("spsid", "3"), ("ppsid", "3")];

/// Code `plane` (a monochrome picture at the source depth) as an
/// auxiliary of `master` — the alpha plane (`urn` an alpha URN,
/// `premultiplied` adds the `prem` reference) or a depth map (a depth
/// URN): monochrome for AV1 / VVC, monochrome-in-4:2:0 for HEVC / AVC
/// (the layout every producer writes and every reader opens), at
/// `depth` bits, padded to the codec alignment with a `clap`, a
/// single-channel `pixi`, no colour information (the plane is not a
/// colour), an essential `auxC`, the item's transforms per the carrier,
/// hidden. Returns the auxiliary's id.
#[allow(clippy::too_many_arguments)]
fn add_auxiliary_plane(
    w: &mut HeifWriter,
    master: u32,
    plane: HeifFrame,
    depth: u8,
    urn: &str,
    hevc_extra: &[(&str, &str)],
    premultiplied: bool,
    opts: &EncodeOptions,
) -> Result<u32> {
    let plane = plane.without_alpha();
    let native_mono = matches!(opts.codec, StillCodec::Av1 | StillCodec::Vvc);
    let a_src = if native_mono {
        mono_at_depth(plane, depth)?
    } else {
        to_yuv420(&plane, depth)?
    };
    let vis = (a_src.width, a_src.height);
    let extra: &[(&str, &str)] = if opts.codec == StillCodec::Hevc {
        hevc_extra
    } else {
        &[]
    };
    let padded = pad_owned(a_src, alignment(opts))?;
    // No colour description: full range, every code point unspecified.
    let aux_colr = Colr::Nclx {
        primaries: 2,
        transfer: 2,
        matrix: 2,
        full_range: true,
    };
    let pic = encode_picture_for(padded, opts, &aux_colr, extra)?;
    let mut props: Vec<(Property, bool)> = coded_props(&pic, &opts.colr, None)
        .into_iter()
        .filter(|(p, _)| !matches!(p, Property::Colr(_)))
        .map(|(p, e)| match p {
            // One channel: the plane (the neutral chroma of a 4:2:0
            // coding carries nothing).
            Property::Pixi(_) => (
                Property::Pixi(Pixi {
                    bits_per_channel: vec![pic.layout.bit_depth],
                }),
                e,
            ),
            other => (other, e),
        })
        .collect();
    props.push((
        Property::AuxC(crate::props::AuxC {
            aux_type: urn.into(),
            aux_subtype: Vec::new(),
        }),
        true,
    ));
    let is_depth = urn == crate::props::AUX_URN_DEPTH || urn == crate::props::AUX_URN_DEPTH_HEVC;
    if is_depth {
        // The map has its own geometry: the crop is the primary's.
        props.extend(companion_chain(
            &pic,
            vis,
            opts,
            opts.transform_carrier != TransformCarrier::Identity,
        ));
    } else {
        // The alpha plane has the master's geometry: the same chain.
        props.extend(coded_item_chain(&pic, vis, opts));
    }
    Ok(if is_depth {
        w.add_depth(master, pic.item_type, pic.data, props)
    } else {
        w.add_alpha(master, pic.item_type, pic.data, props, premultiplied)
    })
}

/// The alpha URN of `opts.codec`'s items: HEVC alpha items use the
/// codec-specific URN (HEIF §7.5.3.2 / Annex B.2.3.2), the form both
/// HEVC producers write and the one Apple ImageIO opens; the others the
/// codec-independent CICP URN (§7.5.3.1; E.2.4: the HEVC URNs may be
/// used with AVC, the CICP one is what every reader keys on).
fn alpha_urn(opts: &EncodeOptions) -> &'static str {
    match opts.codec {
        StillCodec::Hevc => crate::props::AUX_URN_ALPHA_HEVC,
        StillCodec::Av1 | StillCodec::Vvc | StillCodec::Avc => crate::props::AUX_URN_ALPHA,
    }
}

/// The depth URN of `opts.codec`'s items (the same split as the alpha).
fn depth_urn(opts: &EncodeOptions) -> &'static str {
    match opts.codec {
        StillCodec::Hevc => crate::props::AUX_URN_DEPTH_HEVC,
        StillCodec::Av1 | StillCodec::Vvc | StillCodec::Avc => crate::props::AUX_URN_DEPTH,
    }
}

/// The `nclx` to write next to `colr` when an ICC profile is present:
/// HEIF §6.5.5 requires `colour_primaries = transfer_characteristics = 2`
/// for an nclx paired with an ICC (only the matrix / range stay
/// meaningful, since the ICC governs colour); the caller's `nclx` is
/// used unchanged when there is no ICC.
fn nclx_for_icc(colr: &Colr, icc_present: bool) -> Colr {
    match (colr, icc_present) {
        (
            Colr::Nclx {
                matrix, full_range, ..
            },
            true,
        ) => Colr::Nclx {
            primaries: 2,
            transfer: 2,
            matrix: *matrix,
            full_range: *full_range,
        },
        _ => colr.clone(),
    }
}

/// Standard descriptive properties of a coded item.
fn coded_props(pic: &CodedPicture, colr: &Colr, icc: Option<&[u8]>) -> Vec<(Property, bool)> {
    let mut v = vec![
        (pic.config.clone(), true),
        (
            Property::Ispe(Ispe {
                width: pic.coded_width,
                height: pic.coded_height,
            }),
            false,
        ),
        (
            Property::Pixi(Pixi {
                bits_per_channel: vec![pic.layout.bit_depth; pic.layout.chroma.colour_planes()],
            }),
            false,
        ),
        (Property::Colr(nclx_for_icc(colr, icc.is_some())), false),
    ];
    if let Some(icc) = icc {
        v.push((
            Property::Colr(Colr::Icc {
                restricted: false,
                profile: icc.to_vec(),
            }),
            false,
        ));
    }
    v
}

/// Production default of the HEVC in-loop filters on lossy items.
pub const HEVC_FILTERS_DEFAULT: bool = true;

/// Pad `frame` to the codec's block alignment, moving it when it is
/// already aligned and tightly packed (no copy).
fn pad_owned(frame: HeifFrame, a: u32) -> Result<HeifFrame> {
    let (pw, ph) = (
        align_up(frame.width, a).max(a),
        align_up(frame.height, a).max(a),
    );
    let bps = frame.format.bytes_per_sample();
    let tight = frame
        .planes
        .iter()
        .enumerate()
        .all(|(i, p)| p.stride == frame.plane_dims(i).0 as usize * bps);
    if (pw, ph) == (frame.width, frame.height) && tight {
        return Ok(frame);
    }
    pad_frame(&frame, pw, ph)
}

/// The `clap` restoring `vis` (the visible size) over a coded picture
/// of `pic`'s size, when they differ.
fn clap_for(pic: &CodedPicture, vis: (u32, u32)) -> Option<(Property, bool)> {
    ((pic.coded_width, pic.coded_height) != vis).then(|| {
        (
            Property::Clap(Clap::for_rect(
                pic.coded_width,
                pic.coded_height,
                CropRect {
                    x: 0,
                    y: 0,
                    width: vis.0,
                    height: vis.1,
                },
            )),
            true,
        )
    })
}

/// Encode a coded item for `frame` at its visible size, padding to
/// the codec's alignment and attaching a `clap` when needed.
fn add_picture_item(
    w: &mut HeifWriter,
    frame: HeifFrame,
    opts: &EncodeOptions,
    extra: Vec<(Property, bool)>,
) -> Result<u32> {
    let vis = (frame.width, frame.height);
    let padded = pad_owned(frame, alignment(opts))?;
    let pic = encode_picture(padded, opts)?;
    let mut props = coded_props(&pic, &opts.colr, opts.icc_profile.as_deref());
    props.extend(extra);
    props.extend(coded_item_chain(&pic, vis, opts));
    Ok(w.add_coded_item(pic.item_type, pic.data, props))
}

/// The colour-only picture `encode_still` codes for `frame`: AV1 keeps
/// the source's (depth, chroma) pairing up to 12 bits, HEVC codes
/// 4:2:0 at [`EncodeOptions::hevc_coded_format`]. One new allocation
/// at most (the converted picture); an already-matching source is
/// copied once without its alpha plane.
fn coding_picture(frame: &HeifFrame, opts: &EncodeOptions) -> Result<HeifFrame> {
    let coded = coded_layout(frame.format, opts)?;
    to_layout(frame, coded.chroma, coded.bit_depth)
}

/// The colour layout a planar `source` is coded in by `opts.codec`:
/// HEVC, AV1 and AVC keep the source's chroma sampling at the coded
/// depth ([`coded_depth`]: the source's own up to 12 bits, 10 for
/// deeper sources, or [`EncodeOptions::hevc_depth`]) — AVC codes a
/// 4:0:0 source 4:2:0 ([`EncodeOptions::avc_coded_format`]); VVC keeps
/// the sampling at up to 10 bits ([`EncodeOptions::vvc_coded_format`]).
/// The alpha plane is not part of it.
pub fn coded_layout(source: HeifPixelFormat, opts: &EncodeOptions) -> Result<HeifPixelFormat> {
    match opts.codec {
        StillCodec::Hevc => opts.hevc_coded_format(source),
        StillCodec::Av1 => HeifPixelFormat::new(
            source.chroma,
            coded_depth(source.bit_depth, opts.hevc_depth)?,
            false,
        ),
        StillCodec::Avc => opts.avc_coded_format(source),
        StillCodec::Vvc => opts.vvc_coded_format(source),
    }
}

/// Re-quantise every plane of `f` to `depth` bits (round to nearest
/// going down, bit replication going up), keeping the chroma layout.
pub fn to_depth(f: &HeifFrame, depth: u8) -> Result<HeifFrame> {
    let fmt = HeifPixelFormat::new(f.format.chroma, depth, f.format.has_alpha)?;
    let src_depth = f.format.bit_depth;
    if src_depth == depth {
        return Ok(f.tight());
    }
    let max = fmt.max_value() as u32;
    let mut out = HeifFrame::zeroed(f.width, f.height, fmt)?;
    for p in 0..fmt.plane_count() {
        let (w, h) = out.plane_dims(p);
        for y in 0..h {
            for x in 0..w {
                let v = f.sample(p, x, y) as u32;
                let o = if src_depth > depth {
                    let s = src_depth - depth;
                    ((v + (1 << (s - 1))) >> s).min(max)
                } else {
                    let s = depth - src_depth;
                    ((v << s) | (v >> (src_depth - s))).min(max)
                };
                out.set_sample(p, x, y, o as u16);
            }
        }
    }
    Ok(out)
}

/// [`coding_picture`] over an owned frame: a frame already in the
/// coding layout is split into its colour planes and alpha plane
/// without a copy.
fn coding_picture_owned(
    frame: HeifFrame,
    opts: &EncodeOptions,
) -> Result<(HeifFrame, Option<HeifFrame>)> {
    let coded = coded_layout(frame.format, opts)?;
    let bps = frame.format.bytes_per_sample();
    let tight = frame
        .planes
        .iter()
        .enumerate()
        .all(|(i, p)| p.stride == frame.plane_dims(i).0 as usize * bps);
    if frame.format.without_alpha() != coded || !tight {
        let alpha = frame.alpha_as_frame();
        return Ok((coding_picture(&frame, opts)?, alpha));
    }
    let (width, height, format) = (frame.width, frame.height, frame.format);
    let mut planes = frame.planes;
    let alpha = format.alpha_plane().map(|i| HeifFrame {
        width,
        height,
        format: HeifPixelFormat {
            chroma: Chroma::Mono,
            bit_depth: format.bit_depth,
            has_alpha: false,
        },
        planes: vec![planes.remove(i)],
    });
    Ok((
        HeifFrame {
            width,
            height,
            format: coded,
            planes,
        },
        alpha,
    ))
}

/// One timed picture of [`encode_sequence`]: a frame in the image's
/// layout, the options carrying its colour / metadata, and its display
/// duration in milliseconds.
pub(crate) struct SequenceInput {
    pub(crate) frame: HeifFrame,
    pub(crate) opts: EncodeOptions,
    pub(crate) duration_ms: u32,
}

/// Write `timed` as an image-sequence track (plus an alpha track when
/// the pictures carry alpha) — the [`crate::encode_all`] shape. Every
/// picture is coded as a sync sample with the first picture's codec and
/// coding layout; `still`, when given, becomes the file-level `meta`
/// (image items decoded before the sequence), otherwise a cover item
/// aliasing sample 0 is written (MIAF §7.2.1.4). Timescale is 1000.
pub(crate) fn encode_sequence(
    timed: Vec<SequenceInput>,
    still: Option<HeifWriter>,
) -> Result<Vec<u8>> {
    use crate::writer::{SequenceAlphaTrack, SequenceSample, SequenceWriter};

    let Some(first) = timed.first() else {
        return Err(HeifError::invalid("sequence: no timed frames"));
    };
    let vis = (first.frame.width, first.frame.height);
    if vis.0 > u16::MAX as u32 || vis.1 > u16::MAX as u32 {
        return Err(HeifError::unsupported(format!(
            "sequence: {}x{} exceeds the 16-bit track size",
            vis.0, vis.1
        )));
    }
    let has_alpha = first.frame.format.has_alpha;
    let first_opts = first.opts.clone();
    let mut sw: Option<SequenceWriter> = None;
    let mut alpha_track: Option<SequenceAlphaTrack> = None;
    // AVC sequences code as one encoder session each (colour, alpha):
    // IDR + P pictures, one access unit per frame.
    let avc = first_opts.codec == StillCodec::Avc;
    let mut avc_colour = avc.then(|| AvcSequenceEncoder::new(timed.len()));
    let mut avc_alpha = avc.then(|| AvcSequenceEncoder::new(timed.len()));
    for (i, input) in timed.into_iter().enumerate() {
        let SequenceInput {
            frame,
            opts,
            duration_ms,
        } = input;
        if (frame.width, frame.height) != vis || frame.format.has_alpha != has_alpha {
            return Err(HeifError::invalid(format!(
                "sequence: frame {i} is {}x{} (alpha: {}), frame 0 is {}x{} (alpha: {}) — \
                 encode_all frames share one geometry and alpha presence",
                frame.width, frame.height, frame.format.has_alpha, vis.0, vis.1, has_alpha
            )));
        }
        if opts.codec != first_opts.codec {
            return Err(HeifError::invalid("sequence: frames must share one codec"));
        }
        let native_av1 = native_av1(&frame, &opts);
        let (colour, alpha) = coding_picture_owned(frame, &opts)?;
        let depth = colour.format.bit_depth;
        let padded = pad_owned(colour, alignment(&opts))?;
        let (pic, sync) = match avc_colour.as_mut() {
            Some(s) => s.encode(padded, &opts)?,
            None => (encode_picture_for(padded, &opts, &opts.colr, &[])?, true),
        };
        let w = match sw.as_mut() {
            Some(w) => {
                if w.config != pic.config {
                    return Err(HeifError::invalid(format!(
                        "sequence: frame {i} codes with a different decoder configuration \
                         (depth or chroma layout) than frame 0"
                    )));
                }
                w
            }
            None => {
                let mut w = SequenceWriter::new(
                    pic.item_type,
                    pic.config.clone(),
                    pic.coded_width as u16,
                    pic.coded_height as u16,
                    1000,
                );
                let icc = opts.icc_profile.as_deref();
                w.entry_properties.push(Property::Pixi(Pixi {
                    bits_per_channel: vec![pic.layout.bit_depth; pic.layout.chroma.colour_planes()],
                }));
                w.entry_properties
                    .push(Property::Colr(nclx_for_icc(&opts.colr, icc.is_some())));
                if let Some(icc) = icc {
                    w.entry_properties.push(Property::Colr(Colr::Icc {
                        restricted: false,
                        profile: icc.to_vec(),
                    }));
                }
                if let Some((clap, _)) = clap_for(&pic, vis) {
                    w.entry_properties.push(clap);
                }
                sw.insert(w)
            }
        };
        w.push_sample(pic.data, duration_ms, sync);
        if let Some(alpha) = alpha {
            let a_src = if native_av1 {
                mono_at_depth(alpha, depth)?
            } else {
                to_yuv420(&alpha, depth)?
            };
            let extra: &[(&str, &str)] = if opts.codec == StillCodec::Hevc {
                ALPHA_HEVC_OPTIONS
            } else {
                &[]
            };
            let padded = pad_owned(a_src, alignment(&opts))?;
            let alpha_colr = Colr::Nclx {
                primaries: 2,
                transfer: 2,
                matrix: 2,
                full_range: true,
            };
            let (apic, alpha_sync) = match avc_alpha.as_mut() {
                Some(s) => s.encode(padded, &opts)?,
                None => (encode_picture_for(padded, &opts, &alpha_colr, extra)?, true),
            };
            let urn = match opts.codec {
                StillCodec::Hevc => crate::props::AUX_URN_ALPHA_HEVC,
                StillCodec::Av1 | StillCodec::Vvc | StillCodec::Avc => crate::props::AUX_URN_ALPHA,
            };
            let at = alpha_track.get_or_insert_with(|| {
                let mut props = vec![Property::Pixi(Pixi {
                    bits_per_channel: vec![apic.layout.bit_depth],
                })];
                props.extend(clap_for(&apic, vis).map(|(c, _)| c));
                SequenceAlphaTrack::new(
                    apic.item_type,
                    apic.config.clone(),
                    apic.coded_width as u16,
                    apic.coded_height as u16,
                    urn.to_string(),
                    Vec::new(),
                    props,
                )
            });
            if at.config != apic.config {
                return Err(HeifError::invalid(format!(
                    "sequence: alpha of frame {i} codes with a different decoder configuration"
                )));
            }
            at.samples
                .push(SequenceSample::new(apic.data, duration_ms, alpha_sync));
        }
    }
    let mut sw = sw.expect("at least one timed frame");
    sw.alpha = alpha_track;
    match still {
        Some(still) => sw.still = Some(still),
        None => {
            // Cover image aliasing sample 0 (one copy of the bytes), so
            // the file carries the file-level `meta` MIAF requires.
            let mut still = HeifWriter::new();
            let mut props: Vec<(Property, bool)> = vec![
                (sw.config.clone(), true),
                (
                    Property::Ispe(Ispe {
                        width: sw.width as u32,
                        height: sw.height as u32,
                    }),
                    false,
                ),
            ];
            props.extend(
                sw.entry_properties
                    .iter()
                    .map(|p| (p.clone(), matches!(p, Property::Clap(_)))),
            );
            let cover = still.add_coded_item(sw.entry_type, Vec::new(), props);
            still.set_primary(cover);
            sw.still = Some(still);
            sw.cover_sample = Some(0);
        }
    }
    sw.write_to_vec()
}

/// Whether the items are AV1 (coded in the source's chroma layout,
/// alpha as a monochrome item) rather than HEVC (4:2:0 items).
fn native_av1(_frame: &HeifFrame, opts: &EncodeOptions) -> bool {
    opts.codec == StillCodec::Av1
}

/// The `tile × tile` tile at `(x, y)` of `f` (both multiples of the
/// codec alignment, so the chroma planes cut on sample boundaries):
/// the inside part copied plane by plane, the samples beyond the
/// picture's right / bottom edge replicated from the last column /
/// row — no padded copy of the whole picture is ever made.
fn tile_from(f: &HeifFrame, x: u32, y: u32, tile: u32) -> Result<HeifFrame> {
    let mut out = HeifFrame::zeroed(tile, tile, f.format)?;
    let bps = f.format.bytes_per_sample();
    for p in 0..f.format.plane_count() {
        let (sx, sy) = if p == 0 || Some(p) == f.format.alpha_plane() {
            (0, 0)
        } else {
            f.format.chroma.shift()
        };
        let (spw, sph) = f.plane_dims(p);
        let (pw, ph) = out.plane_dims(p);
        let (x0, y0) = (x >> sx, y >> sy);
        let run = (spw.saturating_sub(x0)).min(pw) as usize;
        let src = &f.planes[p];
        let dst_stride = out.planes[p].stride;
        let dst = &mut out.planes[p].data;
        for row in 0..ph {
            let sy_ = (y0 + row).min(sph - 1) as usize;
            let s = &src.data[sy_ * src.stride + x0 as usize * bps..];
            let d = &mut dst[row as usize * dst_stride..(row as usize + 1) * dst_stride];
            if run > 0 {
                d[..run * bps].copy_from_slice(&s[..run * bps]);
                let last = &s[(run - 1) * bps..run * bps].to_vec();
                for px in d[run * bps..].chunks_exact_mut(bps) {
                    px.copy_from_slice(last);
                }
            }
        }
    }
    Ok(out)
}

/// Code the tiles of a `cols × rows` grid over `colour` (the tiles
/// beyond its edges are edge-replicated per tile) on `opts.workers()`
/// threads and hand them to the writer in row-major order as they
/// complete — item ids and bytes are those of the serial encode.
/// Returns the tile item ids.
fn encode_grid_tiles(
    w: &mut HeifWriter,
    colour: &HeifFrame,
    tile: u32,
    cols: u32,
    rows: u32,
    opts: &EncodeOptions,
) -> Result<Vec<u32>> {
    let n = (rows * cols) as usize;
    let tile_at = |i: usize| -> Result<HeifFrame> {
        let (r, c) = ((i as u32) / cols, (i as u32) % cols);
        tile_from(colour, c * tile, r * tile, tile)
    };
    // Each tile's codec runs serial; the budget is spent across tiles.
    let tile_opts = EncodeOptions {
        threads: None,
        ..opts.clone()
    };
    let mut ids = Vec::with_capacity(n);
    let mut add = |w: &mut HeifWriter, pic: CodedPicture| {
        let props = coded_props(&pic, &opts.colr, None);
        let id = w.add_coded_item(pic.item_type, pic.data, props);
        w.set_hidden(id, true);
        ids.push(id);
    };
    let workers = oxideav_core::ExecutionContext::with_threads(opts.workers()).effective_workers(n);
    if workers <= 1 {
        for i in 0..n {
            add(w, encode_picture(tile_at(i)?, &tile_opts)?);
        }
        return Ok(ids);
    }
    // Work-stealing over a shared counter; finished tiles come back
    // over a channel and are reordered on the main thread, which hands
    // each tile to the writer as soon as its predecessors are in — the
    // side buffer holds only the out-of-order completions.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Result<CodedPicture>)>();
    let result: Result<()> = std::thread::scope(|s| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            let tile_opts = &tile_opts;
            let tile_at = &tile_at;
            s.spawn(move || loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let coded = tile_at(i).and_then(|t| encode_picture(t, tile_opts));
                if tx.send((i, coded)).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        let mut pending: std::collections::BTreeMap<usize, CodedPicture> =
            std::collections::BTreeMap::new();
        let mut expect = 0usize;
        while expect < n {
            let (i, coded) = rx
                .recv()
                .map_err(|_| HeifError::invalid("grid: a tile worker stopped early"))?;
            pending.insert(i, coded?);
            while let Some(pic) = pending.remove(&expect) {
                add(w, pic);
                expect += 1;
            }
        }
        Ok(())
    });
    result?;
    Ok(ids)
}

/// Encode `frame` (any planar layout; converted to the codec's coding
/// layout — 4:2:0 at the coded depth for HEVC, its own layout for AV1) into a complete HEIF file per
/// `opts`. Returns the file bytes.
pub fn encode_still(frame: &HeifFrame, opts: &EncodeOptions) -> Result<Vec<u8>> {
    frame.validate()?;
    let mut w = HeifWriter::new();
    let colour = coding_picture(frame, opts)?;
    let master = encode_still_planes(&mut w, colour, frame.alpha_as_frame(), opts)?;
    w.set_primary(master);
    w.write_to_vec()
}

/// [`encode_still`] over an owned frame: a picture already in the
/// coding layout (4:2:0 at the coded depth for HEVC, its own layout
/// for AV1) is coded in place — no copy of the input planes is made
/// on this side; the codec's input is the frame's own planes.
pub fn encode_still_owned(frame: HeifFrame, opts: &EncodeOptions) -> Result<Vec<u8>> {
    frame.validate()?;
    let mut w = HeifWriter::new();
    let (colour, alpha) = coding_picture_owned(frame, opts)?;
    let master = encode_still_planes(&mut w, colour, alpha, opts)?;
    w.set_primary(master);
    w.write_to_vec()
}

/// [`encode_still`]'s item graph added to an existing writer: the
/// primary (coded item or grid), its alpha auxiliary, thumbnail,
/// metadata and gain map. Returns the primary's item id (the caller
/// sets it as primary, or builds further on it).
pub fn encode_still_into(
    w: &mut HeifWriter,
    frame: &HeifFrame,
    opts: &EncodeOptions,
) -> Result<u32> {
    frame.validate()?;
    let colour = coding_picture(frame, opts)?;
    encode_still_planes(w, colour, frame.alpha_as_frame(), opts)
}

/// The item graph over an already-converted colour picture and its
/// (source-depth, monochrome) alpha plane.
fn encode_still_planes(
    w: &mut HeifWriter,
    colour: HeifFrame,
    alpha: Option<HeifFrame>,
    opts: &EncodeOptions,
) -> Result<u32> {
    validate_transforms(opts)?;
    let native_av1 = native_av1(&colour, opts);
    let (cw, ch, cfmt) = (colour.width, colour.height, colour.format);
    let (master, thumb) = match opts.effective_grid_tile(cw, ch) {
        Some(tile) if cw > tile || ch > tile => {
            // MIAF §7.3.11.4.2: tiles are at least 64 pixels; keep them
            // aligned to the codec block size on both axes.
            let tile = align_up(tile.max(crate::miaf::MIN_TILE_EDGE), alignment(opts) * 2);
            let cols = cw.div_ceil(tile);
            let rows = ch.div_ceil(tile);
            if rows > 256 || cols > 256 {
                return Err(HeifError::invalid("grid: more than 256 rows or columns"));
            }
            let thumb = thumbnail_source(&colour, opts)?;
            let tiles = encode_grid_tiles(w, &colour, tile, cols, rows, opts)?;
            drop(colour);
            let desc = GridDescriptor {
                rows: rows as u16,
                columns: cols as u16,
                output_width: cw,
                output_height: ch,
            };
            let mut gprops = vec![(
                Property::Colr(nclx_for_icc(&opts.colr, opts.icc_profile.is_some())),
                false,
            )];
            if let Some(icc) = &opts.icc_profile {
                gprops.push((
                    Property::Colr(Colr::Icc {
                        restricted: false,
                        profile: icc.clone(),
                    }),
                    false,
                ));
            }
            gprops.extend(item_transform_props(opts));
            (w.add_grid(desc, &tiles, gprops)?, thumb)
        }
        _ => {
            let thumb = thumbnail_source(&colour, opts)?;
            (add_picture_item(w, colour, opts, Vec::new())?, thumb)
        }
    };
    // Alpha auxiliary: the alpha plane as a picture — monochrome for
    // AV1 / VVC, monochrome-in-4:2:0 for HEVC / AVC (the layout every
    // HEVC producer writes and every reader opens).
    if let Some(alpha) = alpha {
        add_auxiliary_plane(
            w,
            master,
            alpha,
            cfmt.bit_depth,
            alpha_urn(opts),
            ALPHA_HEVC_OPTIONS,
            false,
            opts,
        )?;
    }
    // Depth auxiliary (HEIF §7.5.3), coded like the alpha plane with its
    // own parameter-set ids.
    if let Some(depth) = &opts.depth_map {
        depth.validate()?;
        add_auxiliary_plane(
            w,
            master,
            depth.clone(),
            cfmt.bit_depth,
            depth_urn(opts),
            DEPTH_HEVC_OPTIONS,
            false,
            opts,
        )?;
    }
    // The primary: the picture itself, or an `iden` carrying the
    // transforms over it.
    let primary = identity_over(w, master, cw, ch, cfmt, opts);
    // Thumbnail (cut from the coding picture before it was coded).
    if let Some(small) = thumb {
        add_thumbnail_item(w, primary, small, opts)?;
    }
    // Metadata.
    if let Some(exif) = &opts.exif {
        w.add_exif(primary, exif);
    }
    if let Some(xmp) = &opts.xmp {
        w.add_xmp(primary, xmp);
    }
    // Gain map → hidden coded item + tmap derived item (HEIF Amd 1
    // §6.6.2.4); the base stays the primary and an altr [tmap, base]
    // group gives tone-map readers the alternate.
    if let Some(gm) = &opts.gain_map {
        add_gain_map(w, primary, cfmt, native_av1, gm, opts)?;
    }
    Ok(primary)
}

/// The gain map of `gm` as a hidden coded item and a `tmap` derived item
/// over `base` (HEIF Amd 1 §6.6.2.4), with the `altr` [tmap, base]
/// fallback. The map carries the primary's orientation (`irot` /
/// `imir`: it is resampled onto the base's *oriented* output, ISO
/// 21496-1 §6.2.2, so it must be oriented the same way); a `clap` of the
/// primary is not copied — the map covers the displayed region.
fn add_gain_map(
    w: &mut HeifWriter,
    base: u32,
    cfmt: HeifPixelFormat,
    native_av1: bool,
    gm: &GainMapSpec,
    opts: &EncodeOptions,
) -> Result<()> {
    gm.frame.validate()?;
    let g_src = if native_av1 {
        // The map keeps its own depth up to 12 bits.
        to_depth(&gm.frame.without_alpha(), gm.frame.format.bit_depth.min(12))?
    } else {
        to_yuv420(&gm.frame, cfmt.bit_depth)?
    };
    let vis = (g_src.width, g_src.height);
    let padded = pad_owned(g_src, alignment(opts))?;
    let gain_colr = Colr::Nclx {
        primaries: 2,
        transfer: 2,
        matrix: if gm.frame.format.chroma == Chroma::Mono {
            2
        } else {
            gm.gain_map_matrix
        },
        full_range: gm.gain_map_full_range,
    };
    let extra: &[(&str, &str)] = if opts.codec == StillCodec::Hevc {
        GAIN_MAP_HEVC_OPTIONS
    } else {
        &[]
    };
    let pic = encode_picture_for(padded, opts, &gain_colr, extra)?;
    let mut props = coded_props(&pic, &gain_colr, None);
    if gm.frame.format.chroma == Chroma::Mono && pic.layout.chroma != Chroma::Mono {
        // The map rides the luma plane: one channel of information.
        for (p, _) in props.iter_mut() {
            if let Property::Pixi(_) = p {
                *p = Property::Pixi(Pixi {
                    bits_per_channel: vec![pic.layout.bit_depth],
                });
            }
        }
    }
    props.extend(companion_chain(&pic, vis, opts, true));
    let gain_id = w.add_coded_item(pic.item_type, pic.data, props);
    let mut tprops = vec![(
        Property::Pixi(Pixi {
            bits_per_channel: vec![
                gm.alternate_bit_depth.clamp(8, 16);
                cfmt.chroma.colour_planes()
            ],
        }),
        false,
    )];
    if let Some(c) = gm.alternate_clli {
        tprops.push((Property::Clli(c), false));
    }
    w.add_tone_map(
        base,
        gain_id,
        &gm.metadata,
        Property::Colr(gm.alternate_colr.clone()),
        tprops,
    )?;
    Ok(())
}

/// Encode an `iovl` overlay (HEIF §6.6.2.2) as a complete file: every
/// layer a coded item (with its alpha auxiliary when the picture has
/// one) in the first layer's coding layout (MIAF §7.3.11.3: the inputs
/// share bit depth and colour information — `opts.colr` describes them
/// all), the `iovl` over them at the layers' offsets with the canvas
/// fill, the overlay the primary (or an `iden` over it under
/// [`TransformCarrier::Identity`]); `opts.transforms`, `thumbnail_max_dim`
/// (a thumbnail of the composed canvas), `exif` / `xmp` / `icc_profile`
/// apply to the overlay. `grid_tile`, `gain_map` and `depth_map` are
/// refused here (the layers are not tiled, the overlay has no gain map
/// or depth auxiliary on this path).
pub fn encode_overlay(spec: &OverlaySpec, opts: &EncodeOptions) -> Result<Vec<u8>> {
    let mut w = HeifWriter::new();
    let id = encode_overlay_into(&mut w, spec, opts)?;
    w.set_primary(id);
    w.write_to_vec()
}

/// [`encode_overlay`] into an existing writer; returns the primary's id
/// (the caller sets the primary).
pub fn encode_overlay_into(
    w: &mut HeifWriter,
    spec: &OverlaySpec,
    opts: &EncodeOptions,
) -> Result<u32> {
    validate_transforms(opts)?;
    if spec.layers.is_empty() {
        return Err(HeifError::invalid("overlay: no layers"));
    }
    if spec.width == 0 || spec.height == 0 {
        return Err(HeifError::invalid("overlay: zero canvas size"));
    }
    if opts.gain_map.is_some() || opts.depth_map.is_some() || opts.grid_tile.is_some_and(|t| t > 0)
    {
        return Err(HeifError::unsupported(
            "overlay: gain_map / depth_map / grid_tile do not apply to an iovl primary",
        ));
    }
    if (spec.width as u64) * (spec.height as u64) > crate::derived::MAX_CANVAS_PIXELS {
        return Err(HeifError::exhausted(
            "overlay: canvas exceeds MAX_CANVAS_PIXELS",
        ));
    }
    // Every layer in the first's coding layout.
    let first = &spec.layers[0].frame;
    first.validate()?;
    let coded = coded_layout(first.format, opts)?;
    let mut ids = Vec::with_capacity(spec.layers.len());
    let mut composed: Vec<HeifFrame> = Vec::new();
    let want_thumb = opts.thumbnail_max_dim.is_some();
    // The layers are inputs of the overlay: the primary's transforms
    // apply to the composed canvas, never to a layer.
    let layer_opts = EncodeOptions {
        transforms: Vec::new(),
        ..opts.clone()
    };
    for layer in &spec.layers {
        layer.frame.validate()?;
        let colour = to_layout(&layer.frame, coded.chroma, coded.bit_depth)?;
        let alpha = layer.frame.alpha_as_frame();
        if want_thumb {
            composed.push(match &alpha {
                Some(a) => colour.with_alpha_plane(a)?,
                None => colour.clone(),
            });
        }
        let id = add_picture_item(w, colour, &layer_opts, Vec::new())?;
        // A layer is an intermediate of the overlay, not an alternative.
        w.set_hidden(id, true);
        if let Some(a) = alpha {
            add_auxiliary_plane(
                w,
                id,
                a,
                coded.bit_depth,
                alpha_urn(opts),
                ALPHA_HEVC_OPTIONS,
                false,
                &layer_opts,
            )?;
        }
        ids.push(id);
    }
    let desc = crate::derived::OverlayDescriptor {
        canvas_fill: spec.fill,
        output_width: spec.width,
        output_height: spec.height,
        offsets: spec.layers.iter().map(|l| (l.x, l.y)).collect(),
    };
    let mut oprops = derived_props(coded, opts);
    oprops.extend(item_transform_props(opts));
    let overlay = w.add_overlay(desc.clone(), &ids, oprops)?;
    let primary = identity_over(w, overlay, spec.width, spec.height, coded, opts);
    if want_thumb {
        // The thumbnail pictures the composed canvas (colour only).
        let inputs: Vec<crate::compose::OverlayInput<'_>> = composed
            .iter()
            .map(|f| crate::compose::OverlayInput::new(f, false))
            .collect();
        let canvas = crate::compose::composite_overlay(&desc, &inputs, Some(&opts.colr))?;
        if let Some(small) = thumbnail_source(&canvas.without_alpha(), opts)? {
            add_thumbnail_item(w, primary, small, opts)?;
        }
    }
    if let Some(exif) = &opts.exif {
        w.add_exif(primary, exif);
    }
    if let Some(xmp) = &opts.xmp {
        w.add_xmp(primary, xmp);
    }
    Ok(primary)
}

/// A monochrome plane at `depth` bits (the coded depth of the item it
/// accompanies): moved when it already is, else re-quantised.
fn mono_at_depth(plane: HeifFrame, depth: u8) -> Result<HeifFrame> {
    if plane.format.bit_depth == depth {
        return Ok(plane);
    }
    let mut f = to_yuv420(&plane, depth)?;
    f.planes.truncate(1);
    f.format = HeifPixelFormat::new(Chroma::Mono, depth, false)?;
    Ok(f)
}

/// The thumbnail picture `opts` asks for over `colour`, when the
/// picture is larger than the requested size.
fn thumbnail_source(colour: &HeifFrame, opts: &EncodeOptions) -> Result<Option<HeifFrame>> {
    let Some(max_dim) = opts.thumbnail_max_dim else {
        return Ok(None);
    };
    let max_dim = max_dim.max(1);
    // The thumbnail pictures the displayed region: the caller's crop is
    // cut off here and not written on the (differently sized) item.
    let cropped;
    let colour = match user_clap(opts) {
        Some(c) => {
            cropped = crate::compose::apply_clap(colour, c)?;
            &cropped
        }
        None => colour,
    };
    if colour.width.max(colour.height) <= max_dim {
        return Ok(None);
    }
    let scale = colour.width.max(colour.height) as f64 / max_dim as f64;
    let tw = ((colour.width as f64 / scale).round() as u32).max(1);
    let th = ((colour.height as f64 / scale).round() as u32).max(1);
    Ok(Some(crate::compose::resize_nearest(colour, tw, th)?))
}

/// Code `small` as the thumbnail of `master`.
fn add_thumbnail_item(
    w: &mut HeifWriter,
    master: u32,
    small: HeifFrame,
    opts: &EncodeOptions,
) -> Result<()> {
    let vis = (small.width, small.height);
    let padded = pad_owned(small, alignment(opts))?;
    let pic = encode_picture(padded, opts)?;
    let mut props = coded_props(&pic, &opts.colr, None);
    props.extend(companion_chain(&pic, vis, opts, true));
    w.add_thumbnail(master, pic.item_type, pic.data, props);
    Ok(())
}

/// Encode a still as a low-overhead image file (ISO/IEC
/// 23008-12:2025/Amd 2:2026 Annex O: `ftyp` major brand `mif3` with
/// the equivalent file's brand — `heic` / `avif` — as minor version
/// (O.2.1.1), then one `MinimizedImageBox` with explicit codec types —
/// `hvc1` / `hvcC` or `av01` / `av1C`). Readers expand it to
/// the O.4 equivalent `meta` + `mdat` ([`crate::HeifFile`] does so on
/// parse).
///
/// The box has no `clap`, grid or thumbnail slot, so the picture (and
/// the alpha / gain map) must be a multiple of the codec block size
/// (16 for HEVC, 8 for AV1), `grid_tile` / `thumbnail_max_dim` must be
/// unset, and `transforms` must be one of the eight Exif orientations
/// as `irot` then `imir` (O.4.6 slots 9 / 10). Colour, ICC, alpha
/// (auxiliary item), Exif, XMP and the gain map (`tmap` with its
/// alternate colour and `clli`) are carried.
pub fn encode_still_minimized(frame: &HeifFrame, opts: &EncodeOptions) -> Result<Vec<u8>> {
    use crate::mini::{MiniChroma, MiniGainMap, MiniHdrBoxes, MinimizedImage, SampleFormat};
    frame.validate()?;
    if opts.grid_tile.is_some_and(|t| t > 0) || opts.thumbnail_max_dim.is_some() {
        return Err(HeifError::unsupported(
            "low-overhead file: the MinimizedImageBox has no grid or thumbnail item",
        ));
    }
    let orientation = exif_orientation(&opts.transforms)?;
    let native_av1 = native_av1(frame, opts);
    let colour = coding_picture(frame, opts)?;
    let (colour_w, colour_h, coded_depth) = (colour.width, colour.height, colour.format.bit_depth);
    let a = alignment(opts);
    let exact = |f: &HeifFrame, what: &str| -> Result<()> {
        if f.width % a != 0 || f.height % a != 0 {
            return Err(HeifError::unsupported(format!(
                "low-overhead file: the {what} is {}x{}, not a multiple of the {a}-pixel coding block (the MinimizedImageBox carries no clap)",
                f.width, f.height
            )));
        }
        Ok(())
    };
    exact(&colour, "picture")?;
    let config_body = |p: &Property| crate::props::write::property_box(p)[8..].to_vec();
    let codec_types = match opts.codec {
        StillCodec::Hevc => (ITEM_TYPE_HVC1, *b"hvcC"),
        StillCodec::Av1 => (crate::meta::ITEM_TYPE_AV01, *b"av1C"),
        StillCodec::Vvc => (ITEM_TYPE_VVC1, *b"vvcC"),
        // No brand infers AVC types; the explicit types carry them.
        StillCodec::Avc => (ITEM_TYPE_AVC1, *b"avcC"),
    };
    let chroma_of = |c: Chroma| MiniChroma {
        subsampling: c.idc(),
        horizontally_centered: false,
        vertically_centered: c == Chroma::Yuv420,
    };
    let pic = encode_picture(colour, opts)?;
    let (cw, ch) = (pic.coded_width, pic.coded_height);
    if (cw, ch) != (colour_w, colour_h) {
        return Err(HeifError::unsupported(format!(
            "low-overhead file: coded picture {cw}x{ch} differs from the {colour_w}x{colour_h} image"
        )));
    }
    let signalled = nclx_for_icc(&opts.colr, opts.icc_profile.is_some());
    let (full_range, explicit_cicp, icc) = match (&signalled, &opts.icc_profile) {
        (
            Colr::Nclx {
                primaries,
                transfer,
                matrix,
                full_range,
            },
            icc,
        ) => {
            let cicp = (*primaries as u8, *transfer as u8, *matrix as u8);
            let default = {
                let (p, t) = if icc.is_some() { (2, 2) } else { (1, 13) };
                (
                    p,
                    t,
                    if pic.layout.chroma == Chroma::Mono {
                        2
                    } else {
                        6
                    },
                )
            };
            (*full_range, (cicp != default).then_some(cicp), icc.clone())
        }
        (Colr::Icc { profile, .. }, _) => (true, None, Some(profile.clone())),
        (_, icc) => (true, None, icc.clone()),
    };
    let mut m = MinimizedImage {
        explicit_codec_types: Some(codec_types),
        format: SampleFormat::Integer {
            bits: pic.layout.bit_depth,
        },
        full_range,
        chroma: chroma_of(pic.layout.chroma),
        orientation,
        width: cw,
        height: ch,
        explicit_cicp,
        icc,
        alpha: false,
        alpha_premultiplied: false,
        hdr: false,
        hdr_boxes: MiniHdrBoxes::default(),
        gain_map: None,
        main_codec_config: config_body(&pic.config),
        main_data: pic.data,
        alpha_codec_config: None,
        alpha_data: Vec::new(),
        exif_xmp_compressed: false,
        exif: opts.exif.as_ref().map(|t| {
            let mut b = 0u32.to_be_bytes().to_vec();
            b.extend_from_slice(t);
            b
        }),
        xmp: opts.xmp.as_ref().map(|x| x.as_bytes().to_vec()),
    };
    if let Some(alpha) = frame.alpha_as_frame() {
        let a_src = if native_av1 {
            mono_at_depth(alpha, coded_depth)?
        } else {
            to_yuv420(&alpha, coded_depth)?
        };
        let extra: &[(&str, &str)] = if opts.codec == StillCodec::Hevc {
            ALPHA_HEVC_OPTIONS
        } else {
            &[]
        };
        let alpha_colr = Colr::Nclx {
            primaries: 2,
            transfer: 2,
            matrix: 2,
            full_range: true,
        };
        let apic = encode_picture_for(a_src, opts, &alpha_colr, extra)?;
        m.alpha = true;
        let cfg = config_body(&apic.config);
        if cfg != m.main_codec_config {
            m.alpha_codec_config = Some(cfg);
        }
        m.alpha_data = apic.data;
    }
    if let Some(gm) = &opts.gain_map {
        gm.frame.validate()?;
        let g_src = if native_av1 {
            to_depth(&gm.frame.without_alpha(), gm.frame.format.bit_depth.min(12))?
        } else {
            to_yuv420(&gm.frame, coded_depth)?
        };
        exact(&g_src, "gain map")?;
        let gain_colr = Colr::Nclx {
            primaries: 2,
            transfer: 2,
            matrix: if gm.frame.format.chroma == Chroma::Mono {
                2
            } else {
                gm.gain_map_matrix
            },
            full_range: gm.gain_map_full_range,
        };
        let extra: &[(&str, &str)] = if opts.codec == StillCodec::Hevc {
            GAIN_MAP_HEVC_OPTIONS
        } else {
            &[]
        };
        let gpic = encode_picture_for(g_src, opts, &gain_colr, extra)?;
        if gm.frame.format.chroma == Chroma::Mono && gpic.layout.chroma != Chroma::Mono {
            // The box derives the gain map's pixi from its coded chroma
            // layout (O.4.7.4), so a luma-only map coded 4:2:0 would read
            // back as a three-channel map.
            return Err(HeifError::unsupported(
                "low-overhead file: a single-channel gain map needs a 4:0:0 coding (AV1)",
            ));
        }
        let Colr::Nclx {
            matrix: gmatrix,
            full_range: gfull,
            ..
        } = gain_colr
        else {
            unreachable!("built as nclx above")
        };
        let (tmap_cicp, tmap_icc) = match &gm.alternate_colr {
            Colr::Nclx {
                primaries,
                transfer,
                matrix,
                full_range,
            } => (
                Some((
                    *primaries as u8,
                    *transfer as u8,
                    *matrix as u8,
                    *full_range,
                )),
                None,
            ),
            Colr::Icc { profile, .. } => (None, Some(profile.clone())),
            Colr::Other { .. } => (None, None),
        };
        let gcfg = config_body(&gpic.config);
        let mut tmap_hdr = MiniHdrBoxes::default();
        if let Some(c) = gm.alternate_clli {
            let mut b = c.max_content_light_level.to_be_bytes().to_vec();
            b.extend_from_slice(&c.max_pic_average_light_level.to_be_bytes());
            tmap_hdr.clli = Some(b);
        }
        m.hdr = true;
        m.gain_map = Some(MiniGainMap {
            width: gpic.coded_width,
            height: gpic.coded_height,
            matrix_coefficients: gmatrix as u8,
            full_range: gfull,
            chroma: chroma_of(gpic.layout.chroma),
            format: SampleFormat::Integer {
                bits: gpic.layout.bit_depth,
            },
            tmap_cicp,
            tmap_icc,
            tmap_hdr,
            metadata: gm.metadata.serialize(),
            codec_config: (gcfg != m.main_codec_config).then_some(gcfg),
            data: gpic.data,
        });
    }
    // The minor version names the brand of the equivalent file (O.2.1.1)
    // — the form black-box readers key their codec choice on.
    let minor = match opts.codec {
        StillCodec::Hevc => crate::ftyp::BRAND_HEIC,
        StillCodec::Av1 => crate::ftyp::BRAND_AVIF,
        // Explicit codec types ride along, so the full `vvcC` is
        // carried and `vvic` (not the compact-record `vvi3`) names
        // the equivalent file.
        StillCodec::Vvc => crate::ftyp::BRAND_VVIC,
        StillCodec::Avc => crate::ftyp::BRAND_AVCI,
    };
    m.to_file_with_minor(u32::from_be_bytes(minor))
}

/// The Exif orientation (1..=8) an `irot` / `imir` chain expresses, in
/// the order the low-overhead expansion associates them (O.4.6:
/// `irot` then `imir`).
fn exif_orientation(transforms: &[Property]) -> Result<u8> {
    let mut angle = None;
    let mut axis = None;
    for t in transforms {
        match t {
            Property::Irot(r) if angle.is_none() && axis.is_none() => angle = Some(r.angle & 3),
            Property::Imir(m) if axis.is_none() => axis = Some(m.axis & 1),
            other => {
                return Err(HeifError::unsupported(format!(
                    "low-overhead file: transform '{}' (or its order) has no Exif orientation",
                    crate::boxes::fourcc_str(&other.box_type())
                )))
            }
        }
    }
    let o = match (angle.unwrap_or(0), axis) {
        (0, None) => 1,
        (0, Some(1)) => 2,
        (2, None) => 3,
        (0, Some(0)) => 4,
        (1, Some(0)) => 5,
        (3, None) => 6,
        (1, Some(1)) => 7,
        (1, None) => 8,
        (a, m) => {
            return Err(HeifError::unsupported(format!(
                "low-overhead file: irot {a} + imir {m:?} is not one of the O.4.6 orientations"
            )))
        }
    };
    Ok(o)
}

/// Codec options giving the gain-map item's HEVC parameter sets their
/// own ids (VPS / SPS / PPS 2), for the same reason as the alpha's.
const GAIN_MAP_HEVC_OPTIONS: &[(&str, &str)] = &[("vpsid", "2"), ("spsid", "2"), ("ppsid", "2")];

/// Accepted framework pixel formats of the `"heif"` encoder: the planar
/// YCbCr / grey layouts (`HeifFrame::from_core`) plus the packed RGB /
/// RGBA / grey+alpha ones converted by [`packed_to_planar`].
pub const ENCODER_PIXEL_FORMATS: &[oxideav_core::PixelFormat] = &[
    oxideav_core::PixelFormat::Rgb24,
    oxideav_core::PixelFormat::Rgba,
    oxideav_core::PixelFormat::Bgr24,
    oxideav_core::PixelFormat::Bgra,
    oxideav_core::PixelFormat::Rgb48Le,
    oxideav_core::PixelFormat::Rgba64Le,
    oxideav_core::PixelFormat::Gray8,
    oxideav_core::PixelFormat::Gray16Le,
    oxideav_core::PixelFormat::Ya8,
    oxideav_core::PixelFormat::Ya16Le,
    oxideav_core::PixelFormat::Yuv420P,
    oxideav_core::PixelFormat::YuvJ420P,
    oxideav_core::PixelFormat::Yuv422P,
    oxideav_core::PixelFormat::YuvJ422P,
    oxideav_core::PixelFormat::Yuv444P,
    oxideav_core::PixelFormat::YuvJ444P,
    oxideav_core::PixelFormat::Yuva420P,
    oxideav_core::PixelFormat::Yuva444P,
    oxideav_core::PixelFormat::Yuv420P10Le,
    oxideav_core::PixelFormat::Yuv444P10Le,
    oxideav_core::PixelFormat::Gbrp8,
    oxideav_core::PixelFormat::Gbrap8,
    oxideav_core::PixelFormat::Gbrp10Le,
    oxideav_core::PixelFormat::Gbrap10Le,
    oxideav_core::PixelFormat::Gbrp12Le,
    oxideav_core::PixelFormat::Gbrap12Le,
];

/// Convert a packed RGB / RGBA / BGR / BGRA (8 or 16-bit) or packed
/// grey + alpha frame to the planar layout the encoder consumes: 4:4:4
/// YCbCr (or monochrome for grey + alpha) at the source depth, through
/// the H.273 matrix and range of `colr` (the MIAF default — BT.601,
/// full range — for an ICC / absent one), with the alpha carried as a
/// plane. Row-major, `stride` honoured. See [`packed_to_planar_for`]
/// for a conversion straight into a coding layout.
pub fn packed_to_planar(
    vf: &oxideav_core::VideoFrame,
    width: u32,
    height: u32,
    pf: oxideav_core::PixelFormat,
    colr: &Colr,
) -> Result<HeifFrame> {
    let (_, _, wide, alpha, grey) = packed_layout(pf)?;
    let target = HeifPixelFormat::new(
        if grey { Chroma::Mono } else { Chroma::Yuv444 },
        if wide { 16 } else { 8 },
        alpha,
    )?;
    packed_to_planar_for(vf, width, height, pf, colr, target)
}

/// `(bytes per pixel, channel order into [r, g, b, a] / [y, a], 16-bit,
/// alpha, grey)` of a packed framework layout.
fn packed_layout(pf: oxideav_core::PixelFormat) -> Result<(usize, [usize; 4], bool, bool, bool)> {
    use oxideav_core::PixelFormat as P;
    Ok(match pf {
        P::Rgb24 => (3, [0, 1, 2, 0], false, false, false),
        P::Rgba => (4, [0, 1, 2, 3], false, true, false),
        P::Bgr24 => (3, [2, 1, 0, 0], false, false, false),
        P::Bgra => (4, [2, 1, 0, 3], false, true, false),
        P::Rgb48Le => (6, [0, 1, 2, 0], true, false, false),
        P::Rgba64Le => (8, [0, 1, 2, 3], true, true, false),
        P::Ya8 => (2, [0, 0, 0, 1], false, true, true),
        P::Ya16Le => (4, [0, 0, 0, 1], true, true, true),
        other => {
            return Err(HeifError::unsupported(format!(
                "framework pixel format {other:?} is neither planar YCbCr / grey nor packed RGB(A)"
            )))
        }
    })
}

/// Convert a packed frame (the layouts of [`packed_to_planar`])
/// straight into `target` — 4:2:0 / 4:2:2 / 4:4:4 YCbCr or monochrome
/// at any depth, with or without an alpha plane — row pair by row
/// pair: no full-resolution 4:4:4 intermediate exists. The colour
/// conversion is [`crate::compose::fill_to_ycbcr`] at the target
/// depth (8-bit sources widen by bit replication to the 16-bit
/// input of that conversion; 16-bit sources are used as they are),
/// subsampled chroma is the rounded box average of the full-resolution
/// chroma samples (as [`to_yuv420`] computes it), the alpha channel is
/// rounded to the target depth. A monochrome target takes the
/// luma of the conversion (grey sources through the same range
/// mapping).
pub fn packed_to_planar_for(
    vf: &oxideav_core::VideoFrame,
    width: u32,
    height: u32,
    pf: oxideav_core::PixelFormat,
    colr: &Colr,
    target: HeifPixelFormat,
) -> Result<HeifFrame> {
    let (bpp, order, wide, src_alpha, grey) = packed_layout(pf)?;
    if target.has_alpha && !src_alpha {
        return Err(HeifError::invalid(format!(
            "packed {pf:?} has no alpha channel for the {target:?} target"
        )));
    }
    let planes = vf.image_planes();
    let src = planes
        .first()
        .ok_or_else(|| HeifError::invalid("packed frame without a plane"))?;
    packed_bytes_to_planar(
        &src.data, src.stride, width, height, bpp, order, wide, grey, colr, target,
    )
}

/// The kernel of [`packed_to_planar_for`] over raw packed rows: `bpp`
/// bytes per pixel, `order` the channel index of R, G, B, A (or Y at
/// `order[0]` and A at `order[3]` for `grey`), `wide` for 16-bit
/// little-endian samples. Shared with the contract's
/// [`encode_rgb8`](crate::encode_rgb8) / [`encode_rgba8`](crate::encode_rgba8).
#[allow(clippy::too_many_arguments)]
pub(crate) fn packed_bytes_to_planar(
    data: &[u8],
    src_stride: usize,
    width: u32,
    height: u32,
    bpp: usize,
    order: [usize; 4],
    wide: bool,
    grey: bool,
    colr: &Colr,
    target: HeifPixelFormat,
) -> Result<HeifFrame> {
    let row_bytes = width as usize * bpp;
    if width == 0 || height == 0 {
        return Err(HeifError::invalid("zero-sized packed frame"));
    }
    if src_stride < row_bytes || data.len() < src_stride * (height as usize - 1) + row_bytes {
        return Err(HeifError::invalid(format!(
            "packed frame too small for {width}x{height}x{bpp}"
        )));
    }
    let src = PackedRows {
        data,
        stride: src_stride,
    };
    let depth = target.bit_depth;
    let max = target.max_value() as u32;
    let src_depth: u8 = if wide { 16 } else { 8 };
    let mut out = HeifFrame::zeroed(width, height, target)?;
    let alpha_plane = target.alpha_plane();
    let read = |px: &[u8], ch: usize| -> u16 {
        if wide {
            u16::from_le_bytes([px[2 * ch], px[2 * ch + 1]])
        } else {
            px[ch] as u16
        }
    };
    // 16-bit scale for the H.273 conversion.
    let to16 = |v: u16| -> u16 {
        if wide {
            v
        } else {
            (v as u32 * 257) as u16
        }
    };
    // Alpha to the target depth (round to nearest; replicate upwards).
    let alpha_to = |v: u16| -> u16 {
        let v = v as u32;
        if src_depth == depth {
            v as u16
        } else if src_depth > depth {
            let s = src_depth - depth;
            ((v + (1 << (s - 1))) >> s).min(max) as u16
        } else {
            let s = depth - src_depth;
            ((v << s) | (v >> (src_depth - s))).min(max) as u16
        }
    };
    let (sx, sy) = target.chroma.shift();
    let chroma = target.chroma != Chroma::Mono;
    let (cw, chh) = out.plane_dims(if chroma { 1 } else { 0 });
    let bps = target.bytes_per_sample();
    // One row of full-resolution Cb / Cr, accumulated per chroma row
    // group (two luma rows at 4:2:0).
    let mut cb_acc: Vec<u32> = vec![0; if chroma { cw as usize } else { 0 }];
    let mut cr_acc: Vec<u32> = vec![0; cb_acc.len()];
    let mut cnt: Vec<u32> = vec![0; cb_acc.len()];
    let flush_chroma =
        |out: &mut HeifFrame, cy: u32, cb_acc: &mut [u32], cr_acc: &mut [u32], cnt: &mut [u32]| {
            for (cx, ((cb, cr), n)) in cb_acc
                .iter_mut()
                .zip(cr_acc.iter_mut())
                .zip(cnt.iter_mut())
                .enumerate()
            {
                let n0 = (*n).max(1);
                out.set_sample(1, cx as u32, cy, ((*cb + n0 / 2) / n0) as u16);
                out.set_sample(2, cx as u32, cy, ((*cr + n0 / 2) / n0) as u16);
                *cb = 0;
                *cr = 0;
                *n = 0;
            }
        };
    let luma_stride = out.planes[0].stride;
    for y in 0..height {
        let row = &src.data[y as usize * src.stride..y as usize * src.stride + row_bytes];
        let cy = y >> sy;
        for (x, px) in row.chunks_exact(bpp).enumerate() {
            let ycc = if grey {
                let g = to16(read(px, order[0]));
                crate::compose::fill_to_ycbcr([g, g, g], depth, Some(colr))
            } else {
                crate::compose::fill_to_ycbcr(
                    [
                        to16(read(px, order[0])),
                        to16(read(px, order[1])),
                        to16(read(px, order[2])),
                    ],
                    depth,
                    Some(colr),
                )
            };
            let off = y as usize * luma_stride + x * bps;
            if bps == 1 {
                out.planes[0].data[off] = ycc[0] as u8;
            } else {
                out.planes[0].data[off..off + 2].copy_from_slice(&ycc[0].to_le_bytes());
            }
            if chroma {
                let cx = x >> sx;
                cb_acc[cx] += ycc[1] as u32;
                cr_acc[cx] += ycc[2] as u32;
                cnt[cx] += 1;
            }
            if let Some(ap) = alpha_plane {
                let a = alpha_to(read(px, order[3]));
                let aoff = y as usize * out.planes[ap].stride + x * bps;
                if bps == 1 {
                    out.planes[ap].data[aoff] = a as u8;
                } else {
                    out.planes[ap].data[aoff..aoff + 2].copy_from_slice(&a.to_le_bytes());
                }
            }
        }
        // The chroma row completes with the last luma row it covers.
        if chroma && (y + 1 == height || ((y + 1) >> sy) != cy) && cy < chh {
            flush_chroma(&mut out, cy, &mut cb_acc, &mut cr_acc, &mut cnt);
        }
    }
    Ok(out)
}

/// Borrowed packed rows for [`packed_bytes_to_planar`].
struct PackedRows<'a> {
    data: &'a [u8],
    stride: usize,
}

/// Typed options of the `"heif"` framework encoder (declared schema:
/// `oxideav info heif` lists them; unknown keys are refused). The
/// enum fields' listed default is empty because a `const` schema can
/// only hold `String::new()`; the effective defaults are `hevc`,
/// `intra`, `full`, `auto`, `on` and `420` (see [`Default`] and the
/// `help` text).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct HeifEncoderOptions {
    /// `codec`: `hevc` (alias `h265`), `av1`, `vvc` (alias `h266`) or
    /// `avc` (alias `h264`; 8-bit 4:2:0 `avc1` items at `qp`).
    pub codec: String,
    /// `mode` (HEVC): `intra` (CABAC at `qp`) or `pcm` (lossless).
    pub mode: String,
    /// `qp` (HEVC intra), 0..=51.
    pub qp: u32,
    /// `grid`: `auto` (512-px tiles above 4 MP), `none`, or a tile
    /// size in pixels.
    pub grid: String,
    /// `thumbnail`: largest thumbnail dimension (0 = none).
    pub thumbnail: u32,
    /// `irot`: quarter turns anti-clockwise (0..=3) written as the
    /// primary's `irot` (HEIF §6.5.10); 0 = none.
    pub irot: u32,
    /// `imir`: `none`, `vertical` (top / bottom exchanged, axis 0) or
    /// `horizontal` (left / right exchanged, axis 1) — the primary's
    /// `imir` (§6.5.12), applied after `irot`.
    pub imir: String,
    /// `range`: `full` or `limited` sample range of the written `nclx`.
    pub range: String,
    /// `quality` (AV1, `mode=intra`): 0..=100, 100 = lossless.
    pub quality: u32,
    /// `speed` (AV1): `fast` / `balanced` / `thorough`.
    pub speed: String,
    /// `rd` (HEVC intra): mode-decision effort 0..=2 (255 = the
    /// historical coder for 8-bit 4:2:0, level 0 of the quadtree coder
    /// for every other layout).
    pub rd: u32,
    /// `tiles` (HEVC): `CxR` tile layout (empty = none).
    pub tiles: String,
    /// `ctb` (HEVC): coding-tree block size 16 / 32 / 64 of the
    /// quadtree coder (0 = automatic when `rd` / `tiles` need it).
    pub ctb: u32,
    /// `threads`: `auto` (the host's parallelism), a worker count, or
    /// empty = the budget of `set_execution_context` (serial until
    /// one is given).
    pub threads: String,
    /// `depth`: coded bit depth 8 / 10 / 12 (0 = follow the source;
    /// [`coded_depth`]).
    pub depth: u32,
    /// `filters` (HEVC intra): `on` / `off` — deblocking + SAO.
    pub filters: String,
    /// `chroma`: `420` / `444` — the chroma layout packed RGB sources
    /// are converted to (planar sources keep their own).
    pub chroma: String,
    /// `cabac` (AVC, round 476): CABAC entropy coding (Main profile on
    /// 8-bit 4:2:0, the High-family profiles otherwise) instead of
    /// CAVLC. Other codecs ignore it.
    pub cabac: bool,
    /// `qp` was given (an absent `qp` follows `quality` when that was
    /// given, else [`DEFAULT_QP`]).
    qp_given: bool,
    /// `quality` was given.
    quality_given: bool,
    /// `mode` was given (`quality=100` without a `mode` is lossless).
    mode_given: bool,
}
impl HeifEncoderOptions {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        codec: String,
        mode: String,
        qp: u32,
        grid: String,
        thumbnail: u32,
        irot: u32,
        imir: String,
        range: String,
        quality: u32,
        speed: String,
        rd: u32,
        tiles: String,
        ctb: u32,
        threads: String,
        depth: u32,
        filters: String,
        chroma: String,
    ) -> Self {
        Self {
            codec,
            mode,
            qp,
            grid,
            thumbnail,
            irot,
            imir,
            range,
            quality,
            speed,
            rd,
            tiles,
            ctb,
            threads,
            depth,
            filters,
            chroma,
            cabac: false,
            qp_given: false,
            quality_given: false,
            mode_given: false,
        }
    }

    /// Set `codec`.
    pub fn with_codec(mut self, codec: String) -> Self {
        self.codec = codec;
        self
    }

    /// Set `mode`.
    pub fn with_mode(mut self, mode: String) -> Self {
        self.mode = mode;
        self.mode_given = true;
        self
    }

    /// Set `qp`.
    pub fn with_qp(mut self, qp: u32) -> Self {
        self.qp = qp;
        self.qp_given = true;
        self
    }

    /// Set `grid`.
    pub fn with_grid(mut self, grid: String) -> Self {
        self.grid = grid;
        self
    }

    /// Set `thumbnail`.
    pub fn with_thumbnail(mut self, thumbnail: u32) -> Self {
        self.thumbnail = thumbnail;
        self
    }

    /// Set `irot`.
    pub fn with_irot(mut self, irot: u32) -> Self {
        self.irot = irot;
        self
    }

    /// Set `imir`.
    pub fn with_imir(mut self, imir: String) -> Self {
        self.imir = imir;
        self
    }

    /// Set `range`.
    pub fn with_range(mut self, range: String) -> Self {
        self.range = range;
        self
    }

    /// Set `quality`.
    pub fn with_quality(mut self, quality: u32) -> Self {
        self.quality = quality;
        self.quality_given = true;
        self
    }

    /// Set `speed`.
    pub fn with_speed(mut self, speed: String) -> Self {
        self.speed = speed;
        self
    }

    /// Set `rd`.
    pub fn with_rd(mut self, rd: u32) -> Self {
        self.rd = rd;
        self
    }

    /// Set `tiles`.
    pub fn with_tiles(mut self, tiles: String) -> Self {
        self.tiles = tiles;
        self
    }

    /// Set `ctb`.
    pub fn with_ctb(mut self, ctb: u32) -> Self {
        self.ctb = ctb;
        self
    }

    /// Set `threads`.
    pub fn with_threads(mut self, threads: String) -> Self {
        self.threads = threads;
        self
    }

    /// Set `depth`.
    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    /// Set `filters`.
    pub fn with_filters(mut self, filters: String) -> Self {
        self.filters = filters;
        self
    }

    /// Set `chroma`.
    pub fn with_chroma(mut self, chroma: String) -> Self {
        self.chroma = chroma;
        self
    }

    /// Builder-style setter for `cabac`.
    pub fn with_cabac(mut self, cabac: bool) -> Self {
        self.cabac = cabac;
        self
    }
}

impl Default for HeifEncoderOptions {
    fn default() -> Self {
        Self {
            codec: "hevc".into(),
            mode: "intra".into(),
            qp: DEFAULT_QP as u32,
            grid: "auto".into(),
            thumbnail: 0,
            irot: 0,
            imir: String::new(),
            range: "full".into(),
            quality: 60,
            speed: "fast".into(),
            rd: 255,
            tiles: String::new(),
            ctb: 0,
            threads: String::new(),
            depth: 0,
            filters: "on".into(),
            chroma: "420".into(),
            cabac: false,
            qp_given: false,
            quality_given: false,
            mode_given: false,
        }
    }
}

impl oxideav_core::CodecOptionsStruct for HeifEncoderOptions {
    const SCHEMA: &'static [oxideav_core::OptionField] = &[
        oxideav_core::OptionField {
            name: "codec",
            kind: oxideav_core::OptionKind::Enum(&[
                "hevc", "h265", "av1", "vvc", "h266", "avc", "h264",
            ]),
            default: oxideav_core::OptionValue::String(String::new()),
            help: "Coded item codec: hevc (heic file), av1 (avif file), vvc (vvic file) or avc (avci file)",
        },
        oxideav_core::OptionField {
            name: "mode",
            kind: oxideav_core::OptionKind::Enum(&["intra", "pcm"]),
            default: oxideav_core::OptionValue::String(String::new()),
            help: "HEVC coding: intra (CABAC at qp) or pcm (lossless)",
        },
        oxideav_core::OptionField {
            name: "qp",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(DEFAULT_QP as u32),
            help: "HEVC intra quantiser 0..=51 (lower = better; 18 matches the OS encoder's default quality)",
        },
        oxideav_core::OptionField {
            name: "grid",
            kind: oxideav_core::OptionKind::String,
            default: oxideav_core::OptionValue::String(String::new()),
            help: "Grid tiling: auto (512-px tiles above 4 MP; default), none, or a tile size (MIAF 64-px floor)",
        },
        oxideav_core::OptionField {
            name: "thumbnail",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(0),
            help: "Add a thumbnail whose largest dimension is this many pixels (0 = none)",
        },
        oxideav_core::OptionField {
            name: "irot",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(0),
            help: "Rotate the written picture by this many quarter turns anti-clockwise (0..=3; an essential irot on the primary)",
        },
        oxideav_core::OptionField {
            name: "imir",
            kind: oxideav_core::OptionKind::Enum(&["none", "vertical", "horizontal"]),
            default: oxideav_core::OptionValue::String(String::new()),
            help: "Mirror the written picture after the rotation: vertical (top / bottom exchanged) or horizontal (left / right exchanged); an essential imir on the primary",
        },
        oxideav_core::OptionField {
            name: "range",
            kind: oxideav_core::OptionKind::Enum(&["full", "limited"]),
            default: oxideav_core::OptionValue::String(String::new()),
            help: "Sample range written in nclx: full (default) or limited",
        },
        oxideav_core::OptionField {
            name: "quality",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(60),
            help: "Quality 0..=100: AV1 codes mode=intra at it (100 = lossless, the same bytes as mode=pcm; absent = 60); on HEVC / VVC / AVC, when qp is not given, qp = round((100 - quality) * 51 / 100) and 100 is lossless (mode=pcm) unless mode is given; neither given = qp 18 / AV1 quality 60",
        },
        oxideav_core::OptionField {
            name: "speed",
            kind: oxideav_core::OptionKind::Enum(&["fast", "balanced", "thorough"]),
            default: oxideav_core::OptionValue::String(String::new()),
            help: "AV1 search effort (default fast)",
        },
        oxideav_core::OptionField {
            name: "rd",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(255),
            help: "HEVC intra mode-decision effort 0..=2 on the quadtree coder (255 = historical coder for 8-bit 4:2:0; level 2 lands ~5% under the OS encoder's size at ~12x the CPU)",
        },
        oxideav_core::OptionField {
            name: "tiles",
            kind: oxideav_core::OptionKind::String,
            default: oxideav_core::OptionValue::String(String::new()),
            help: "HEVC tile layout CxR (e.g. 4x4) for parallel coding; empty = none",
        },
        oxideav_core::OptionField {
            name: "ctb",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(0),
            help: "HEVC coding-tree block size 16 / 32 / 64 (0 = automatic: chosen when rd / tiles need the quadtree coder)",
        },
        oxideav_core::OptionField {
            name: "threads",
            kind: oxideav_core::OptionKind::String,
            default: oxideav_core::OptionValue::String(String::new()),
            help: "Thread budget: auto, a worker count, or empty = the execution context's (serial until one is given); spent on grid tiles, the HEVC wavefront / tiles and the AV1 tile search",
        },
        oxideav_core::OptionField {
            name: "depth",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(0),
            help: "Coded bit depth 8 / 10 / 12 (0 = follow the source: 8-bit stays 8, 16-bit codes Main 10)",
        },
        oxideav_core::OptionField {
            name: "filters",
            kind: oxideav_core::OptionKind::Enum(&["on", "off"]),
            default: oxideav_core::OptionValue::String(String::new()),
            help: "HEVC in-loop filters (deblocking + SAO) on lossy items (default on)",
        },
        oxideav_core::OptionField {
            name: "chroma",
            kind: oxideav_core::OptionKind::Enum(&["420", "444"]),
            default: oxideav_core::OptionValue::String(String::new()),
            help: "Chroma layout packed RGB sources are coded in (default 420; planar sources keep their own)",
        },
        oxideav_core::OptionField {
            name: "cabac",
            kind: oxideav_core::OptionKind::Bool,
            default: oxideav_core::OptionValue::Bool(false),
            help: "AVC entropy coding: CABAC instead of CAVLC (other codecs ignore it)",
        },
    ];

    fn apply(&mut self, key: &str, value: &oxideav_core::OptionValue) -> CoreResult<()> {
        match key {
            "codec" => self.codec = value.as_str()?.to_string(),
            "mode" => {
                self.mode = value.as_str()?.to_string();
                self.mode_given = true;
            }
            "qp" => {
                self.qp = value.as_u32()?;
                self.qp_given = true;
            }
            "grid" => self.grid = value.as_str()?.to_string(),
            "thumbnail" => self.thumbnail = value.as_u32()?,
            "irot" => self.irot = value.as_u32()?,
            "imir" => self.imir = value.as_str()?.to_string(),
            "range" => self.range = value.as_str()?.to_string(),
            "quality" => {
                self.quality = value.as_u32()?;
                self.quality_given = true;
            }
            "speed" => self.speed = value.as_str()?.to_string(),
            "rd" => self.rd = value.as_u32()?,
            "tiles" => self.tiles = value.as_str()?.to_string(),
            "ctb" => self.ctb = value.as_u32()?,
            "threads" => self.threads = value.as_str()?.to_string(),
            "depth" => self.depth = value.as_u32()?,
            "filters" => self.filters = value.as_str()?.to_string(),
            "chroma" => self.chroma = value.as_str()?.to_string(),
            "cabac" => self.cabac = value.as_bool()?,
            other => {
                return Err(CoreError::invalid(format!(
                    "heif: unknown option '{other}'"
                )))
            }
        }
        Ok(())
    }
}

/// The HEVC / VVC / AVC quantiser a gateway `quality` (0..=100) maps
/// to when `qp` is not given (round 476): `round((100 − quality) × 51
/// / 100)` — monotone, 0 → 51, 50 → 26, 90 → 5, 99 → 1; `quality` 100
/// is the lossless mode instead ([`HeifEncoderOptions::to_encode_options`]).
pub fn qp_of_quality(quality: u32) -> u8 {
    (((100 - quality.min(100)) * 51 + 50) / 100) as u8
}

impl HeifEncoderOptions {
    /// The `EncodeOptions` these settings describe (round 476): an
    /// absent `qp` on HEVC / VVC / AVC follows a given `quality`
    /// ([`qp_of_quality`]; `quality` 100 without a `mode` is the
    /// lossless `pcm`), else [`DEFAULT_QP`] — so bytes with neither
    /// given are unchanged; AV1 codes `mode=intra` at `quality`.
    pub fn to_encode_options(&self) -> CoreResult<EncodeOptions> {
        let codec = match self.codec.as_str() {
            "hevc" | "h265" => StillCodec::Hevc,
            "av1" => StillCodec::Av1,
            "vvc" | "h266" => StillCodec::Vvc,
            "avc" | "h264" => StillCodec::Avc,
            other => {
                return Err(CoreError::invalid(format!(
                    "heif: unknown codec option '{other}'"
                )))
            }
        };
        let quality_steers = codec != StillCodec::Av1 && !self.qp_given && self.quality_given;
        let mode = if quality_steers && !self.mode_given && self.quality >= 100 {
            "pcm".to_string()
        } else {
            self.mode.clone()
        };
        let qp = if quality_steers {
            qp_of_quality(self.quality)
        } else {
            u8::try_from(self.qp.min(51)).unwrap_or(51)
        };
        let grid_tile = match self.grid.trim() {
            "" | "auto" => None,
            "none" | "0" => Some(0),
            n => Some(n.parse::<u32>().map_err(|_| {
                CoreError::invalid(format!("heif: grid '{n}' (auto, none or a tile size)"))
            })?),
        };
        let threads = match self.threads.trim() {
            "" => None,
            "auto" => Some(oxideav_core::ExecutionContext::auto().threads),
            n => Some(n.parse::<usize>().map_err(|_| {
                CoreError::invalid(format!("heif: threads '{n}' (auto or a worker count)"))
            })?),
        };
        let mut transforms = Vec::new();
        if self.irot > 3 {
            return Err(CoreError::invalid(format!(
                "heif: irot {} (0..=3 quarter turns)",
                self.irot
            )));
        }
        if self.irot > 0 {
            transforms.push(Property::Irot(crate::props::Irot::new(self.irot as u8)));
        }
        match self.imir.as_str() {
            "" | "none" => {}
            "vertical" => transforms.push(Property::Imir(crate::props::Imir::new(0))),
            "horizontal" => transforms.push(Property::Imir(crate::props::Imir::new(1))),
            other => {
                return Err(CoreError::invalid(format!(
                    "heif: imir '{other}' (none / vertical / horizontal)"
                )))
            }
        }
        let mut opts = EncodeOptions {
            codec,
            hevc_mode: mode.clone(),
            qp,
            grid_tile,
            thumbnail_max_dim: (self.thumbnail > 0).then_some(self.thumbnail),
            transforms,
            // AV1: `mode=pcm` is lossless, `mode=intra` codes at `quality`.
            av1_quality: (mode != "pcm").then_some(self.quality.min(100) as u8),
            av1_speed: self.speed.clone(),
            hevc_rd: (self.rd <= 2).then_some(self.rd),
            hevc_tiles: (!self.tiles.is_empty()).then_some(self.tiles.clone()),
            hevc_ctb: match self.ctb {
                0 => None,
                16 | 32 | 64 => Some(self.ctb),
                other => {
                    return Err(CoreError::invalid(format!(
                        "heif: ctb {other} (16, 32 or 64; 0 = automatic)"
                    )))
                }
            },
            threads,
            hevc_depth: match self.depth {
                0 => None,
                8 | 10 | 12 => Some(self.depth as u8),
                other => {
                    return Err(CoreError::invalid(format!(
                        "heif: depth {other} (8, 10 or 12; 0 = follow the source)"
                    )))
                }
            },
            hevc_filters: Some(self.filters != "off"),
            chroma: (self.chroma == "444").then_some(Chroma::Yuv444),
            avc_options: if self.cabac {
                vec![("cabac".into(), "1".into())]
            } else {
                Vec::new()
            },
            ..EncodeOptions::default()
        };
        if let Colr::Nclx { full_range, .. } = &mut opts.colr {
            *full_range = self.range != "limited";
        }
        Ok(opts)
    }

    /// The chroma layout packed RGB sources are converted to.
    pub fn packed_chroma(&self) -> Chroma {
        if self.chroma == "444" {
            Chroma::Yuv444
        } else {
            Chroma::Yuv420
        }
    }
}

/// The `"heif"` framework encoder: every video frame becomes one
/// packet holding a complete HEIF file.
pub struct HeifEncoder {
    params: CodecParameters,
    opts: EncodeOptions,
    /// `threads` was given explicitly (the execution context then
    /// does not override it).
    explicit_threads: bool,
    /// `range` was given explicitly (the stream's / frame's colour
    /// signal then does not override it).
    explicit_range: bool,
    queue: std::collections::VecDeque<Packet>,
    flushed: bool,
}

impl HeifEncoder {
    /// Construct from stream parameters; the options are the declared
    /// [`HeifEncoderOptions`] schema (`codec`, `mode`, `qp`, `grid`,
    /// `thumbnail`, `range`, `threads`, …); unknown keys are refused.
    pub fn new(params: &CodecParameters) -> CoreResult<Self> {
        let typed = oxideav_core::parse_options::<HeifEncoderOptions>(&params.options)?;
        let opts = typed.to_encode_options()?;
        Ok(Self {
            params: params.clone(),
            explicit_threads: opts.threads.is_some(),
            explicit_range: params.options.get("range").is_some(),
            opts,
            queue: std::collections::VecDeque::new(),
            flushed: false,
        })
    }

    /// The effective encode options (after `set_execution_context`).
    pub fn options(&self) -> &EncodeOptions {
        &self.opts
    }
}

impl Encoder for HeifEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.params.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.params
    }

    fn set_execution_context(&mut self, ctx: &oxideav_core::ExecutionContext) {
        if !self.explicit_threads {
            self.opts.threads = (ctx.threads > 1).then_some(ctx.threads);
        }
    }

    fn send_frame(&mut self, frame: &Frame) -> CoreResult<()> {
        let Frame::Video(vf) = frame else {
            return Err(CoreError::invalid("heif: only video frames can be encoded"));
        };
        let (w, h) = match (self.params.width, self.params.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
            _ => return Err(CoreError::invalid("heif: width / height must be set")),
        };
        let pf = self
            .params
            .pixel_format
            .ok_or_else(|| CoreError::invalid("heif: pixel_format must be set"))?;
        // The frame's colour description: its own signal, else the
        // stream's; `range` given explicitly pins the range.
        let signal = vf.color_signal().unwrap_or(self.params.color_signal);
        let mut color = crate::api::ColorInfo::from_color_signal(&signal);
        if self.explicit_range {
            color.range = match &self.opts.colr {
                Colr::Nclx {
                    full_range: true, ..
                } => crate::api::ColorRange::Full,
                Colr::Nclx { .. } => crate::api::ColorRange::Limited,
                _ => color.range,
            };
        }
        // The metadata to embed (r476, core 0.1.40): the frame's blobs
        // refined over the stream's — ICC / Exif / XMP items, as the
        // contract `HeifImage::metadata` would be, unless the options
        // carry their own.
        let metadata = crate::api_registry::metadata_of_blobs(&vf.blobs(), self.params.blobs());
        let bytes = match crate::api::PixelFormat::try_from(pf) {
            // Planar YCbCr / grey / GBR and packed RGB(A): the contract
            // path, one implementation.
            Ok(_) => {
                let image = crate::api::HeifImage::from_video_frame_parts(vf, w, h, pf)?
                    .with_color(color)
                    .with_metadata(metadata.clone());
                if image.format.is_packed() {
                    // Borrowed rows straight into the coding layout.
                    let plane = &vf.image_planes()[0];
                    crate::api_registry::encode_packed_bytes(
                        w,
                        h,
                        image.format,
                        plane.stride,
                        &plane.data,
                        &color,
                        &metadata,
                        &self.opts,
                    )?
                } else {
                    crate::api_registry::encode_owned(image, &self.opts)?
                }
            }
            // The other packed layouts (BGR(A), 16-bit RGB(A), grey +
            // alpha): the same kernel into the coding layout, then the
            // still writer.
            Err(_) => {
                let (_, _, wide, alpha, grey) = packed_layout(pf)?;
                let colr = crate::api_registry::effective_colr(
                    &color,
                    crate::api::PixelFormat::Rgb24,
                    &self.opts,
                );
                let with_metadata =
                    crate::api_registry::effective_options(&metadata, colr.clone(), &self.opts);
                let (colr, opts, target) = if !wide && !grey {
                    // 8-bit BGR(A): the same lossless identity rule as
                    // RGB(A).
                    crate::api_registry::packed_coding(colr, with_metadata, alpha)?
                } else {
                    let target = crate::api_registry::packed_target(
                        &self.opts,
                        grey,
                        if wide { 16 } else { 8 },
                        alpha,
                    )?;
                    (colr, with_metadata, target)
                };
                let hf = packed_to_planar_for(vf, w, h, pf, &colr, target)?;
                encode_still_owned(hf, &opts)?
            }
        };
        // An untimed still: `pts` = the frame's (the item index of a
        // burst), duration 1 in a 1/1 time base — the shape the demuxer
        // emits, so a muxer sees the same packet either way.
        let pts = vf.pts.unwrap_or(0);
        let pkt = Packet::new(0, TimeBase::new(1, 1), bytes)
            .with_pts(pts)
            .with_dts(pts)
            .with_duration(1)
            .with_keyframe(true);
        self.queue.push_back(pkt);
        Ok(())
    }

    fn receive_packet(&mut self) -> CoreResult<Packet> {
        match self.queue.pop_front() {
            Some(p) => Ok(p),
            None if self.flushed => Err(CoreError::Eof),
            None => Err(CoreError::NeedMore),
        }
    }

    fn flush(&mut self) -> CoreResult<()> {
        self.flushed = true;
        Ok(())
    }
}

/// Direct factory endpoint for the `"heif"` encoder.
pub fn make_encoder(params: &CodecParameters) -> CoreResult<Box<dyn Encoder>> {
    Ok(Box::new(HeifEncoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_replicates_edges_and_conversion_neutralises_chroma() {
        let mut f = HeifFrame::zeroed(3, 2, HeifPixelFormat::new(Chroma::Mono, 10, false).unwrap())
            .unwrap();
        f.set_sample(0, 2, 1, 1020);
        let p = pad_frame(&f, 4, 4).unwrap();
        assert_eq!(p.sample(0, 3, 3), 1020);
        assert_eq!(p.sample(0, 0, 0), 0);
        let c = to_yuv420_8(&p).unwrap();
        assert_eq!(c.format.chroma, Chroma::Yuv420);
        assert_eq!(c.sample(0, 3, 3), 255);
        assert_eq!(c.sample(1, 1, 1), 128);
        // 4:4:4 → 4:2:0 averages 2x2 chroma blocks.
        let mut q = HeifFrame::zeroed(
            2,
            2,
            HeifPixelFormat::new(Chroma::Yuv444, 8, false).unwrap(),
        )
        .unwrap();
        q.set_sample(1, 0, 0, 100);
        q.set_sample(1, 1, 0, 200);
        q.set_sample(1, 0, 1, 100);
        q.set_sample(1, 1, 1, 200);
        let c = to_yuv420_8(&q).unwrap();
        assert_eq!(c.sample(1, 0, 0), 150);
    }

    #[test]
    fn profile_compat_bits() {
        assert_eq!(profile_compat_flags(1), 1 << 30);
        assert_eq!(profile_compat_flags(3), (1 << 28) | (1 << 30) | (1 << 29));
        assert_eq!(profile_compat_flags(2), (1 << 29) | (1 << 30));
    }
}
