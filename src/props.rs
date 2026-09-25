//! Typed item properties (ISO/IEC 23008-12 §6.5, Annex B.2.3, AVIF
//! §4, ISO/IEC 14496-12 §12.1.4 / §12.1.5) over the raw `ipco` boxes.
//!
//! Property semantics that matter for decoding:
//!
//! * **descriptive** properties describe the reconstructed image
//!   *before* transformations (§6.5.1); readers ignore descriptive
//!   properties that follow the first transformative or unrecognized
//!   property in the association order.
//! * **transformative** properties (`clap`, `irot`, `imir`, `iscl`)
//!   apply in association order to the reconstructed image (§6.3);
//!   MIAF (§7.3.9) additionally requires them to be essential and
//!   ordered clean aperture → rotation → mirror.
//! * an **unrecognized essential** property means the item shall not be
//!   processed (§9.3.1 of the 2017 edition).

use crate::av1c::Av1Config;
use crate::avcc::AvcConfig;
use crate::boxes::{fourcc_str, parse_full_box, FourCc, Reader};
use crate::error::{HeifError, Result};
use crate::hvcc::HevcConfig;
use crate::lhvc::{LhevcConfig, OperatingPoints};
use crate::meta::{Meta, RawProperty};

/// Alpha plane URN, codec-independent (HEIF §6.9.1, MIAF §7.3.5.1).
pub const AUX_URN_ALPHA: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha";
/// Depth map URN, codec-independent (HEIF §6.9.2).
pub const AUX_URN_DEPTH: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:depth";
/// HEVC alpha plane URN (HEIF Annex B.2.4).
pub const AUX_URN_ALPHA_HEVC: &str = "urn:mpeg:hevc:2015:auxid:1";
/// HEVC depth map URN (HEIF Annex B.2.4).
pub const AUX_URN_DEPTH_HEVC: &str = "urn:mpeg:hevc:2015:auxid:2";

/// `ispe` (§6.5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ispe {
    /// `image_width` of the reconstructed image.
    pub width: u32,
    /// `image_height` of the reconstructed image.
    pub height: u32,
}

/// `pixi` (§6.5.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pixi {
    /// `bits_per_channel[]`.
    pub bits_per_channel: Vec<u8>,
}

impl Pixi {
    /// `num_channels`.
    pub fn num_channels(&self) -> usize {
        self.bits_per_channel.len()
    }

    /// Largest channel depth (0 for an empty list).
    pub fn max_bit_depth(&self) -> u8 {
        self.bits_per_channel.iter().copied().max().unwrap_or(0)
    }
}

/// `colr` (§6.5.5; ISO/IEC 14496-12 §12.1.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Colr {
    /// `nclx`: CICP code points (ISO/IEC 23091-2 / H.273).
    Nclx {
        /// `colour_primaries`.
        primaries: u16,
        /// `transfer_characteristics`.
        transfer: u16,
        /// `matrix_coefficients`.
        matrix: u16,
        /// `full_range_flag`.
        full_range: bool,
    },
    /// `rICC` / `prof`: an ICC profile.
    Icc {
        /// `true` for `rICC` (restricted), `false` for `prof`.
        restricted: bool,
        /// The ICC profile bytes.
        profile: Vec<u8>,
    },
    /// Any other `colour_type` (kept raw).
    Other {
        /// The `colour_type` tag.
        colour_type: FourCc,
        /// The bytes after the tag.
        payload: Vec<u8>,
    },
}

impl Colr {
    /// MIAF §7.3.6.4 default when a coded image has no CICP property:
    /// BT.709 primaries, sRGB transfer, BT.601 matrix, full range.
    pub const MIAF_DEFAULT: Colr = Colr::Nclx {
        primaries: 1,
        transfer: 13,
        matrix: 6,
        full_range: true,
    };

    /// `true` for the ICC variants.
    pub fn is_icc(&self) -> bool {
        matches!(self, Colr::Icc { .. })
    }
}

/// `pasp` (§6.5.4; ISO/IEC 14496-12 §12.1.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pasp {
    /// `hSpacing`.
    pub h_spacing: u32,
    /// `vSpacing`.
    pub v_spacing: u32,
}

impl Pasp {
    /// `true` for square pixels (the MIAF-mandated value).
    pub fn is_square(&self) -> bool {
        self.h_spacing == self.v_spacing && self.h_spacing != 0
    }
}

/// `clap` (§6.5.9; ISO/IEC 14496-12 §12.1.4). Offsets are the raw
/// 32-bit fields interpreted as two's complement (a centre offset can
/// be negative).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clap {
    /// `cleanApertureWidthN`.
    pub width_n: u32,
    /// `cleanApertureWidthD`.
    pub width_d: u32,
    /// `cleanApertureHeightN`.
    pub height_n: u32,
    /// `cleanApertureHeightD`.
    pub height_d: u32,
    /// `horizOffN`.
    pub horiz_off_n: i32,
    /// `horizOffD`.
    pub horiz_off_d: u32,
    /// `vertOffN`.
    pub vert_off_n: i32,
    /// `vertOffD`.
    pub vert_off_d: u32,
}

/// A resolved clean-aperture rectangle in pixels of the input image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CropRect {
    /// Left-most column.
    pub x: u32,
    /// Top-most row.
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

impl Clap {
    /// Resolve the aperture against an input of `width × height`.
    ///
    /// The clean aperture centre is at `((width − 1) / 2 + horizOff,
    /// (height − 1) / 2 + vertOff)`; the left edge is therefore
    /// `(width − cleanApertureWidth) / 2 + horizOff`. MIAF §7.3.6.7
    /// requires integer apertures; a fractional size or a non-integer
    /// edge is rejected, as is an aperture outside the input.
    pub fn resolve(&self, width: u32, height: u32) -> Result<CropRect> {
        let axis = |name: &str,
                    n: u32,
                    d: u32,
                    off_n: i32,
                    off_d: u32,
                    input: u32|
         -> Result<(u32, u32)> {
            if d == 0 || off_d == 0 {
                return Err(HeifError::invalid(format!("clap: zero {name} denominator")));
            }
            if n % d != 0 {
                return Err(HeifError::unsupported(format!(
                    "clap: fractional clean aperture {name} {n}/{d}"
                )));
            }
            let size = n / d;
            if size == 0 || size > input {
                return Err(HeifError::invalid(format!(
                    "clap: clean aperture {name} {size} outside the {input}-pixel input"
                )));
            }
            // edge = (input - size) / 2 + off_n / off_d, exactly.
            let num = (input as i128 - size as i128) * off_d as i128 + 2 * off_n as i128;
            let den = 2 * off_d as i128;
            if num % den != 0 {
                return Err(HeifError::unsupported(format!(
                    "clap: non-integer {name} edge ({num}/{den})"
                )));
            }
            let edge = num / den;
            if edge < 0 || edge + size as i128 > input as i128 {
                return Err(HeifError::invalid(format!(
                    "clap: {name} aperture [{edge}, {}) outside the {input}-pixel input",
                    edge + size as i128
                )));
            }
            Ok((edge as u32, size))
        };
        let (x, w) = axis(
            "width",
            self.width_n,
            self.width_d,
            self.horiz_off_n,
            self.horiz_off_d,
            width,
        )?;
        let (y, h) = axis(
            "height",
            self.height_n,
            self.height_d,
            self.vert_off_n,
            self.vert_off_d,
            height,
        )?;
        Ok(CropRect {
            x,
            y,
            width: w,
            height: h,
        })
    }

    /// Build a centred (or offset) integer clean aperture.
    pub fn integer(width: u32, height: u32, horiz_off: i32, vert_off: i32) -> Self {
        Self {
            width_n: width,
            width_d: 1,
            height_n: height,
            height_d: 1,
            horiz_off_n: horiz_off,
            horiz_off_d: 1,
            vert_off_n: vert_off,
            vert_off_d: 1,
        }
    }

    /// Build the aperture selecting `rect` out of a `width × height`
    /// input; the offsets are expressed in halves so any integer
    /// rectangle is representable exactly.
    pub fn for_rect(width: u32, height: u32, rect: CropRect) -> Self {
        // edge = (input - size)/2 + off  ⇒  off = edge - (input - size)/2
        // ⇒ off_n/off_d = (2*edge - (input - size)) / 2
        let off = |edge: u32, size: u32, input: u32| -> i32 {
            2 * edge as i32 - (input as i32 - size as i32)
        };
        Self {
            width_n: rect.width,
            width_d: 1,
            height_n: rect.height,
            height_d: 1,
            horiz_off_n: off(rect.x, rect.width, width),
            horiz_off_d: 2,
            vert_off_n: off(rect.y, rect.height, height),
            vert_off_d: 2,
        }
    }
}

/// `irot` (§6.5.10): `angle × 90°` anti-clockwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Irot {
    /// `angle` in `0..=3`.
    pub angle: u8,
}

/// `imir` (§6.5.12): `axis` 0 = vertical mirror (top/bottom exchanged),
/// 1 = horizontal mirror (left/right exchanged).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Imir {
    /// `axis`.
    pub axis: u8,
}

/// `iscl` (§6.5.13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Iscl {
    /// `target_width_numerator`.
    pub width_num: u16,
    /// `target_width_denominator`.
    pub width_den: u16,
    /// `target_height_numerator`.
    pub height_num: u16,
    /// `target_height_denominator`.
    pub height_den: u16,
}

impl Iscl {
    /// Output size for an input of `width × height` (ceil of the exact
    /// ratio, §6.5.13.1).
    pub fn output_size(&self, width: u32, height: u32) -> Result<(u32, u32)> {
        if self.width_num == 0
            || self.width_den == 0
            || self.height_num == 0
            || self.height_den == 0
        {
            return Err(HeifError::invalid("iscl: zero numerator or denominator"));
        }
        let w = (width as u64 * self.width_num as u64).div_ceil(self.width_den as u64);
        let h = (height as u64 * self.height_num as u64).div_ceil(self.height_den as u64);
        if w > u32::MAX as u64 || h > u32::MAX as u64 {
            return Err(HeifError::invalid("iscl: scaled size overflows"));
        }
        Ok((w as u32, h as u32))
    }
}

/// Kind of an auxiliary image, from its `auxC` URN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuxKind {
    /// Alpha plane (either URN family).
    Alpha,
    /// Depth map (either URN family).
    Depth,
    /// Anything else.
    Other,
}

/// `auxC` (§6.5.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuxC {
    /// `aux_type` URN.
    pub aux_type: String,
    /// `aux_subtype` bytes (HEVC: `HEVCAuxConfigSubType`, Annex B.2.4).
    pub aux_subtype: Vec<u8>,
}

impl AuxC {
    /// Classify the URN.
    pub fn kind(&self) -> AuxKind {
        match self.aux_type.as_str() {
            AUX_URN_ALPHA | AUX_URN_ALPHA_HEVC => AuxKind::Alpha,
            AUX_URN_DEPTH | AUX_URN_DEPTH_HEVC => AuxKind::Depth,
            _ => AuxKind::Other,
        }
    }
}

/// `clli` — content light level (ISO/IEC 14496-12 §12.1.6, a plain
/// 4-byte Box; H.265 D.3.35 semantics, CTA-861-G zero = unknown).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clli {
    /// `max_content_light_level` (MaxCLL, cd/m²).
    pub max_content_light_level: u16,
    /// `max_pic_average_light_level` (MaxFALL, cd/m²).
    pub max_pic_average_light_level: u16,
}

/// `mdcv` — mastering display colour volume (ISO/IEC 14496-12 §12.1.7:
/// three interleaved `(display_primaries_x, display_primaries_y)`
/// pairs, chromaticity in 0.00002 steps, luminance in 0.0001 cd/m²;
/// semantics of the H.265 D.3.28 SEI, primaries in G, B, R order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mdcv {
    /// `(display_primaries_x, display_primaries_y)[c]`, `c` = 0..3 in
    /// file order (G, B, R).
    pub display_primaries: [(u16, u16); 3],
    /// `white_point_x/y`.
    pub white_point: (u16, u16),
    /// `max_display_mastering_luminance`.
    pub max_luminance: u32,
    /// `min_display_mastering_luminance`.
    pub min_luminance: u32,
}

/// `cclv` — content colour volume (ISO/IEC 14496-12 §12.1.8: one flag
/// byte — `ccv_cancel_flag`, `ccv_persistence_flag`, three presence
/// flags, 2 reserved bits — then interleaved signed 32-bit primaries
/// and the present luminance values; semantics of the H.265 D.3.40
/// SEI, where in a sample entry the two leading flags are 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cclv {
    /// `ccv_cancel_flag`.
    pub cancel: bool,
    /// `ccv_persistence_flag`.
    pub persistence: bool,
    /// `ccv_primaries_x/y[c]` when present (G, B, R).
    pub primaries: Option<[(i32, i32); 3]>,
    /// `ccv_min_luminance_value` when present.
    pub min_luminance: Option<u32>,
    /// `ccv_max_luminance_value` when present.
    pub max_luminance: Option<u32>,
    /// `ccv_avg_luminance_value` when present.
    pub avg_luminance: Option<u32>,
}

/// `amve` — ambient viewing environment (HEIF §6.5.36 / ISO/IEC
/// 14496-12 §12.1.9: a plain 8-byte Box; H.265 D.3.39 semantics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Amve {
    /// `ambient_illuminance` (0.0001 lux).
    pub ambient_illuminance: u32,
    /// `ambient_light_x` (× 50000).
    pub ambient_light_x: u16,
    /// `ambient_light_y` (× 50000).
    pub ambient_light_y: u16,
}

/// `rloc` (§6.5.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rloc {
    /// `horizontal_offset`.
    pub horizontal_offset: u32,
    /// `vertical_offset`.
    pub vertical_offset: u32,
}

/// `lsel` (§6.5.11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lsel {
    /// `layer_id`.
    pub layer_id: u16,
}

/// `a1op` — AV1 operating point selector (AVIF §4.3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct A1op {
    /// `op_index`.
    pub op_index: u8,
}

/// `a1lx` — AV1 layered image indexing (AVIF §4.3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct A1lx {
    /// `large_size` flag (32-bit sizes).
    pub large_size: bool,
    /// `layer_size[3]` in bytes (0 = layer absent / last).
    pub layer_size: [u32; 3],
}

/// `rref` (§6.5.17).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rref {
    /// `reference_type[]`.
    pub reference_types: Vec<FourCc>,
}

/// `crtt` / `mdft` (§6.5.18 / §6.5.19): a time in microseconds since
/// 1904-01-01T00:00:00Z when `version == 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeInfo {
    /// The 64-bit timestamp field.
    pub time: u64,
}

/// `udes` (§6.5.20).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Udes {
    /// `lang`.
    pub lang: String,
    /// `name`.
    pub name: String,
    /// `description`.
    pub description: String,
    /// `tags` (comma separated per the spec).
    pub tags: String,
}

/// `altt` (§6.5.21).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Altt {
    /// `alt_text`.
    pub alt_text: String,
    /// `alt_lang`.
    pub alt_lang: String,
}

/// One typed property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Property {
    /// `ispe`.
    Ispe(Ispe),
    /// `pixi`.
    Pixi(Pixi),
    /// `colr`.
    Colr(Colr),
    /// `pasp`.
    Pasp(Pasp),
    /// `clap` (transformative).
    Clap(Clap),
    /// `irot` (transformative).
    Irot(Irot),
    /// `imir` (transformative).
    Imir(Imir),
    /// `iscl` (transformative).
    Iscl(Iscl),
    /// `auxC`.
    AuxC(AuxC),
    /// `hvcC` — HEVC decoder configuration (Annex B.2.3.1).
    HvcC(HevcConfig),
    /// `av1C` — AV1 codec configuration (AVIF §4.2).
    Av1C(Av1Config),
    /// `lhvC` — layered HEVC configuration (HEIF B.2.3.2).
    LhvC(LhevcConfig),
    /// `avcC` — AVC configuration (HEIF E.2.3).
    AvcC(AvcConfig),
    /// `oinf` — operating points information (HEIF B.2.3.3).
    Oinf(OperatingPoints),
    /// `tols` — target output layer set (HEIF §6.5.29).
    Tols(u16),
    /// `clli`.
    Clli(Clli),
    /// `mdcv`.
    Mdcv(Mdcv),
    /// `cclv`.
    Cclv(Cclv),
    /// `amve`.
    Amve(Amve),
    /// `rloc`.
    Rloc(Rloc),
    /// `lsel`.
    Lsel(Lsel),
    /// `a1op`.
    A1op(A1op),
    /// `a1lx`.
    A1lx(A1lx),
    /// `rref`.
    Rref(Rref),
    /// `crtt`.
    Crtt(TimeInfo),
    /// `mdft`.
    Mdft(TimeInfo),
    /// `udes`.
    Udes(Udes),
    /// `altt`.
    Altt(Altt),
    /// A property type this crate does not model (raw box kept).
    Unknown(RawProperty),
}

impl Property {
    /// The property's box type.
    pub fn box_type(&self) -> FourCc {
        match self {
            Property::Ispe(_) => *b"ispe",
            Property::Pixi(_) => *b"pixi",
            Property::Colr(_) => *b"colr",
            Property::Pasp(_) => *b"pasp",
            Property::Clap(_) => *b"clap",
            Property::Irot(_) => *b"irot",
            Property::Imir(_) => *b"imir",
            Property::Iscl(_) => *b"iscl",
            Property::AuxC(_) => *b"auxC",
            Property::HvcC(_) => *b"hvcC",
            Property::Av1C(_) => *b"av1C",
            Property::LhvC(_) => *b"lhvC",
            Property::AvcC(_) => *b"avcC",
            Property::Oinf(_) => *b"oinf",
            Property::Tols(_) => *b"tols",
            Property::Clli(_) => *b"clli",
            Property::Mdcv(_) => *b"mdcv",
            Property::Cclv(_) => *b"cclv",
            Property::Amve(_) => *b"amve",
            Property::Rloc(_) => *b"rloc",
            Property::Lsel(_) => *b"lsel",
            Property::A1op(_) => *b"a1op",
            Property::A1lx(_) => *b"a1lx",
            Property::Rref(_) => *b"rref",
            Property::Crtt(_) => *b"crtt",
            Property::Mdft(_) => *b"mdft",
            Property::Udes(_) => *b"udes",
            Property::Altt(_) => *b"altt",
            Property::Unknown(r) => r.box_type,
        }
    }

    /// `true` for the transformative property types (§6.5.9–§6.5.13).
    pub fn is_transformative(&self) -> bool {
        is_transformative_type(&self.box_type())
    }

    /// `true` for decoder configuration properties (§6.5.2).
    pub fn is_decoder_config(&self) -> bool {
        matches!(
            self,
            Property::HvcC(_) | Property::Av1C(_) | Property::LhvC(_) | Property::AvcC(_)
        )
    }

    /// Parse one raw `ipco` box into its typed form. Unknown box types
    /// become [`Property::Unknown`]; a malformed known type is an error.
    pub fn parse(raw: &RawProperty) -> Result<Property> {
        let b = raw.body.as_slice();
        let p = match &raw.box_type {
            b"ispe" => Property::Ispe(parse_ispe(b)?),
            b"pixi" => Property::Pixi(parse_pixi(b)?),
            b"colr" => Property::Colr(parse_colr(b)?),
            b"pasp" => Property::Pasp(parse_pasp(b)?),
            b"clap" => Property::Clap(parse_clap(b)?),
            b"irot" => Property::Irot(Irot {
                angle: first_byte(b, "irot")? & 0x03,
            }),
            b"imir" => Property::Imir(Imir {
                axis: first_byte(b, "imir")? & 0x01,
            }),
            b"iscl" => Property::Iscl(parse_iscl(b)?),
            b"auxC" => Property::AuxC(parse_auxc(b)?),
            b"hvcC" => Property::HvcC(HevcConfig::parse(b)?),
            b"av1C" => Property::Av1C(Av1Config::parse(b)?),
            b"lhvC" => Property::LhvC(LhevcConfig::parse(b)?),
            b"avcC" => Property::AvcC(AvcConfig::parse(b)?),
            b"oinf" => {
                let (v, _f, body) = parse_full_box(b)?;
                if v != 0 {
                    return Err(HeifError::invalid(format!("oinf version {v}")));
                }
                Property::Oinf(OperatingPoints::parse(body)?)
            }
            b"tols" => {
                let (v, _f, body) = parse_full_box(b)?;
                if v != 0 {
                    return Err(HeifError::invalid(format!("tols version {v}")));
                }
                Property::Tols(Reader::new(body).u16("tols target_ols_idx")?)
            }
            b"clli" => Property::Clli(parse_clli(b)?),
            b"mdcv" => Property::Mdcv(parse_mdcv(b)?),
            b"cclv" => Property::Cclv(parse_cclv(b)?),
            b"amve" => Property::Amve(parse_amve(b)?),
            b"rloc" => Property::Rloc(parse_rloc(b)?),
            b"lsel" => Property::Lsel(parse_lsel(b)?),
            b"a1op" => Property::A1op(A1op {
                op_index: first_byte(b, "a1op")?,
            }),
            b"a1lx" => Property::A1lx(parse_a1lx(b)?),
            b"rref" => Property::Rref(parse_rref(b)?),
            b"crtt" => Property::Crtt(parse_time(b, "crtt")?),
            b"mdft" => Property::Mdft(parse_time(b, "mdft")?),
            b"udes" => Property::Udes(parse_udes(b)?),
            b"altt" => Property::Altt(parse_altt(b)?),
            _ => Property::Unknown(raw.clone()),
        };
        Ok(p)
    }
}

/// `true` for `clap` / `irot` / `imir` / `iscl`.
pub fn is_transformative_type(t: &FourCc) -> bool {
    matches!(t, b"clap" | b"irot" | b"imir" | b"iscl")
}

/// One resolved association: the property, its 1-based `ipco` index and
/// its `essential` flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyEntry {
    /// 1-based index into `ipco`.
    pub index: u16,
    /// `essential`.
    pub essential: bool,
    /// The typed property.
    pub property: Property,
}

/// The typed property list of one item, in association order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemProperties {
    /// Entries in `ipma` order.
    pub entries: Vec<PropertyEntry>,
}

impl ItemProperties {
    /// Resolve and parse the properties of `item_id`.
    pub fn resolve(meta: &Meta, item_id: u32) -> Result<Self> {
        let mut entries = Vec::new();
        for (index, raw, essential) in meta.properties_of(item_id) {
            let property = Property::parse(raw).map_err(|e| {
                HeifError::invalid(format!(
                    "item {item_id} property #{index} ('{}'): {e}",
                    fourcc_str(&raw.box_type)
                ))
            })?;
            entries.push(PropertyEntry {
                index,
                essential,
                property,
            });
        }
        Ok(Self { entries })
    }

    /// Every entry.
    pub fn iter(&self) -> impl Iterator<Item = &PropertyEntry> {
        self.entries.iter()
    }

    /// Descriptive properties a reader honours: those *before* the first
    /// transformative or unrecognized property (§6.5.1).
    pub fn descriptive(&self) -> impl Iterator<Item = &PropertyEntry> {
        self.entries
            .iter()
            .take_while(|e| {
                !e.property.is_transformative() && !matches!(e.property, Property::Unknown(_))
            })
            .filter(|e| !e.property.is_transformative())
    }

    /// Transformative properties in association order (§6.3).
    pub fn transformative(&self) -> impl Iterator<Item = &PropertyEntry> {
        self.entries
            .iter()
            .filter(|e| e.property.is_transformative())
    }

    /// Unrecognized properties marked essential — an item carrying one
    /// shall not be processed.
    pub fn unsupported_essential(&self) -> Vec<FourCc> {
        self.entries
            .iter()
            .filter(|e| e.essential && matches!(e.property, Property::Unknown(_)))
            .map(|e| e.property.box_type())
            .collect()
    }

    fn find<T>(&self, f: impl Fn(&Property) -> Option<T>) -> Option<T> {
        self.descriptive().find_map(|e| f(&e.property))
    }

    /// `ispe`.
    pub fn ispe(&self) -> Option<Ispe> {
        self.find(|p| match p {
            Property::Ispe(v) => Some(*v),
            _ => None,
        })
    }

    /// `pixi`.
    pub fn pixi(&self) -> Option<&Pixi> {
        self.descriptive().find_map(|e| match &e.property {
            Property::Pixi(v) => Some(v),
            _ => None,
        })
    }

    /// Every `colr` (an item may carry one ICC and one `nclx`).
    pub fn colrs(&self) -> Vec<&Colr> {
        self.descriptive()
            .filter_map(|e| match &e.property {
                Property::Colr(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    /// The `nclx` colour information, when present.
    pub fn nclx(&self) -> Option<&Colr> {
        self.colrs()
            .into_iter()
            .find(|c| matches!(c, Colr::Nclx { .. }))
    }

    /// The ICC profile bytes, when present.
    pub fn icc_profile(&self) -> Option<&[u8]> {
        self.colrs().into_iter().find_map(|c| match c {
            Colr::Icc { profile, .. } => Some(profile.as_slice()),
            _ => None,
        })
    }

    /// `pasp`.
    pub fn pasp(&self) -> Option<Pasp> {
        self.find(|p| match p {
            Property::Pasp(v) => Some(*v),
            _ => None,
        })
    }

    /// `auxC`.
    pub fn auxc(&self) -> Option<&AuxC> {
        self.descriptive().find_map(|e| match &e.property {
            Property::AuxC(v) => Some(v),
            _ => None,
        })
    }

    /// `hvcC`.
    pub fn hvcc(&self) -> Option<&HevcConfig> {
        self.descriptive().find_map(|e| match &e.property {
            Property::HvcC(v) => Some(v),
            _ => None,
        })
    }

    /// `avcC`.
    pub fn avcc(&self) -> Option<&AvcConfig> {
        self.descriptive().find_map(|e| match &e.property {
            Property::AvcC(v) => Some(v),
            _ => None,
        })
    }

    /// `lhvC`.
    pub fn lhvc(&self) -> Option<&LhevcConfig> {
        self.descriptive().find_map(|e| match &e.property {
            Property::LhvC(v) => Some(v),
            _ => None,
        })
    }

    /// `oinf`.
    pub fn oinf(&self) -> Option<&OperatingPoints> {
        self.descriptive().find_map(|e| match &e.property {
            Property::Oinf(v) => Some(v),
            _ => None,
        })
    }

    /// `tols` `target_ols_idx`.
    pub fn tols(&self) -> Option<u16> {
        self.descriptive().find_map(|e| match &e.property {
            Property::Tols(v) => Some(*v),
            _ => None,
        })
    }

    /// `av1C`.
    pub fn av1c(&self) -> Option<&Av1Config> {
        self.descriptive().find_map(|e| match &e.property {
            Property::Av1C(v) => Some(v),
            _ => None,
        })
    }

    /// `clli`.
    pub fn clli(&self) -> Option<Clli> {
        self.find(|p| match p {
            Property::Clli(v) => Some(*v),
            _ => None,
        })
    }

    /// `mdcv`.
    pub fn mdcv(&self) -> Option<Mdcv> {
        self.find(|p| match p {
            Property::Mdcv(v) => Some(*v),
            _ => None,
        })
    }

    /// `cclv`.
    pub fn cclv(&self) -> Option<Cclv> {
        self.find(|p| match p {
            Property::Cclv(v) => Some(*v),
            _ => None,
        })
    }

    /// `rloc`.
    pub fn rloc(&self) -> Option<Rloc> {
        self.find(|p| match p {
            Property::Rloc(v) => Some(*v),
            _ => None,
        })
    }

    /// `lsel`.
    pub fn lsel(&self) -> Option<Lsel> {
        self.find(|p| match p {
            Property::Lsel(v) => Some(*v),
            _ => None,
        })
    }

    /// `a1op`.
    pub fn a1op(&self) -> Option<A1op> {
        self.find(|p| match p {
            Property::A1op(v) => Some(*v),
            _ => None,
        })
    }

    /// `a1lx`.
    pub fn a1lx(&self) -> Option<A1lx> {
        self.find(|p| match p {
            Property::A1lx(v) => Some(*v),
            _ => None,
        })
    }

    /// `rref`.
    pub fn rref(&self) -> Option<&Rref> {
        self.descriptive().find_map(|e| match &e.property {
            Property::Rref(v) => Some(v),
            _ => None,
        })
    }

    /// `clap` (from the transformative chain).
    pub fn clap(&self) -> Option<Clap> {
        self.transformative().find_map(|e| match &e.property {
            Property::Clap(v) => Some(*v),
            _ => None,
        })
    }

    /// `irot` (from the transformative chain).
    pub fn irot(&self) -> Option<Irot> {
        self.transformative().find_map(|e| match &e.property {
            Property::Irot(v) => Some(*v),
            _ => None,
        })
    }

    /// `imir` (from the transformative chain).
    pub fn imir(&self) -> Option<Imir> {
        self.transformative().find_map(|e| match &e.property {
            Property::Imir(v) => Some(*v),
            _ => None,
        })
    }

    /// Output size after the transformative chain, starting from `ispe`
    /// (or the given reconstructed size). Follows §6.3: each transform
    /// applies to the output of the previous one; `essential_only`
    /// skips non-essential transforms (a permitted reader choice).
    pub fn output_size(
        &self,
        reconstructed: (u32, u32),
        essential_only: bool,
    ) -> Result<(u32, u32)> {
        let (mut w, mut h) = reconstructed;
        for e in self.transformative() {
            if essential_only && !e.essential {
                continue;
            }
            match &e.property {
                Property::Clap(c) => {
                    let r = c.resolve(w, h)?;
                    w = r.width;
                    h = r.height;
                }
                Property::Irot(r) => {
                    if r.angle % 2 == 1 {
                        std::mem::swap(&mut w, &mut h);
                    }
                }
                Property::Imir(_) => {}
                Property::Iscl(s) => {
                    let (nw, nh) = s.output_size(w, h)?;
                    w = nw;
                    h = nh;
                }
                _ => {}
            }
        }
        Ok((w, h))
    }
}

fn first_byte(b: &[u8], what: &str) -> Result<u8> {
    b.first()
        .copied()
        .ok_or_else(|| HeifError::invalid(format!("{what}: empty body")))
}

fn parse_ispe(b: &[u8]) -> Result<Ispe> {
    let (v, _f, body) = parse_full_box(b)?;
    if v != 0 {
        return Err(HeifError::invalid(format!("ispe version {v}")));
    }
    let mut r = Reader::new(body);
    Ok(Ispe {
        width: r.u32("ispe image_width")?,
        height: r.u32("ispe image_height")?,
    })
}

fn parse_pixi(b: &[u8]) -> Result<Pixi> {
    let (v, _f, body) = parse_full_box(b)?;
    if v != 0 {
        return Err(HeifError::invalid(format!("pixi version {v}")));
    }
    let mut r = Reader::new(body);
    let n = r.u8("pixi num_channels")? as usize;
    Ok(Pixi {
        bits_per_channel: r.bytes(n, "pixi bits_per_channel")?.to_vec(),
    })
}

fn parse_colr(b: &[u8]) -> Result<Colr> {
    let mut r = Reader::new(b);
    let colour_type = r.fourcc("colr colour_type")?;
    match &colour_type {
        b"nclx" => {
            let primaries = r.u16("colr colour_primaries")?;
            let transfer = r.u16("colr transfer_characteristics")?;
            let matrix = r.u16("colr matrix_coefficients")?;
            let full_range = r.u8("colr full_range_flag")? & 0x80 != 0;
            Ok(Colr::Nclx {
                primaries,
                transfer,
                matrix,
                full_range,
            })
        }
        b"rICC" => Ok(Colr::Icc {
            restricted: true,
            profile: r.rest().to_vec(),
        }),
        b"prof" => Ok(Colr::Icc {
            restricted: false,
            profile: r.rest().to_vec(),
        }),
        _ => Ok(Colr::Other {
            colour_type,
            payload: r.rest().to_vec(),
        }),
    }
}

fn parse_pasp(b: &[u8]) -> Result<Pasp> {
    let mut r = Reader::new(b);
    Ok(Pasp {
        h_spacing: r.u32("pasp hSpacing")?,
        v_spacing: r.u32("pasp vSpacing")?,
    })
}

fn parse_clap(b: &[u8]) -> Result<Clap> {
    let mut r = Reader::new(b);
    Ok(Clap {
        width_n: r.u32("clap cleanApertureWidthN")?,
        width_d: r.u32("clap cleanApertureWidthD")?,
        height_n: r.u32("clap cleanApertureHeightN")?,
        height_d: r.u32("clap cleanApertureHeightD")?,
        horiz_off_n: r.i32("clap horizOffN")?,
        horiz_off_d: r.u32("clap horizOffD")?,
        vert_off_n: r.i32("clap vertOffN")?,
        vert_off_d: r.u32("clap vertOffD")?,
    })
}

fn parse_iscl(b: &[u8]) -> Result<Iscl> {
    let (v, _f, body) = parse_full_box(b)?;
    if v != 0 {
        return Err(HeifError::invalid(format!("iscl version {v}")));
    }
    let mut r = Reader::new(body);
    Ok(Iscl {
        width_num: r.u16("iscl target_width_numerator")?,
        width_den: r.u16("iscl target_width_denominator")?,
        height_num: r.u16("iscl target_height_numerator")?,
        height_den: r.u16("iscl target_height_denominator")?,
    })
}

fn parse_auxc(b: &[u8]) -> Result<AuxC> {
    let (v, _f, body) = parse_full_box(b)?;
    if v != 0 {
        return Err(HeifError::invalid(format!("auxC version {v}")));
    }
    let mut r = Reader::new(body);
    let aux_type = r.cstr("auxC aux_type")?;
    Ok(AuxC {
        aux_type,
        aux_subtype: r.rest().to_vec(),
    })
}

/// `clli` / `mdcv` / `cclv` / `amve` are plain boxes in ISO/IEC 14496-12
/// §12.1.6–9 ("This is a Box, not a FullBox"); some writers emit them
/// with a FullBox prefix. Accept both shapes by length.
fn plain_or_full<'a>(b: &'a [u8], plain_len: usize, what: &str) -> Result<&'a [u8]> {
    if b.len() == plain_len {
        Ok(b)
    } else if b.len() == plain_len + 4 {
        Ok(&b[4..])
    } else {
        Err(HeifError::invalid(format!(
            "{what}: body is {} bytes, expected {plain_len}",
            b.len()
        )))
    }
}

fn parse_clli(b: &[u8]) -> Result<Clli> {
    let mut r = Reader::new(plain_or_full(b, 4, "clli")?);
    Ok(Clli {
        max_content_light_level: r.u16("clli max_content_light_level")?,
        max_pic_average_light_level: r.u16("clli max_pic_average_light_level")?,
    })
}

fn parse_mdcv(b: &[u8]) -> Result<Mdcv> {
    let mut r = Reader::new(plain_or_full(b, 24, "mdcv")?);
    // ISO/IEC 14496-12 §12.1.7: `for (c = 0; c < 3; c++) { x; y }` —
    // the chromaticities are interleaved per primary.
    let mut display_primaries = [(0u16, 0u16); 3];
    for p in display_primaries.iter_mut() {
        p.0 = r.u16("mdcv display_primaries_x")?;
        p.1 = r.u16("mdcv display_primaries_y")?;
    }
    let white_point = (r.u16("mdcv white_point_x")?, r.u16("mdcv white_point_y")?);
    let max_luminance = r.u32("mdcv max_display_mastering_luminance")?;
    let min_luminance = r.u32("mdcv min_display_mastering_luminance")?;
    Ok(Mdcv {
        display_primaries,
        white_point,
        max_luminance,
        min_luminance,
    })
}

/// Body length of a `cclv` (§12.1.8) from its flag byte: 1 + 24 with
/// the primaries + 4 per present luminance value.
fn cclv_body_len(flags: u8) -> usize {
    1 + if flags & 0x20 != 0 { 24 } else { 0 }
        + 4 * ((flags >> 4) & 1) as usize
        + 4 * ((flags >> 3) & 1) as usize
        + 4 * ((flags >> 2) & 1) as usize
}

fn parse_cclv(b: &[u8]) -> Result<Cclv> {
    // Plain Box (the specification's shape); a FullBox prefix is
    // accepted when the plain reading does not fit the length.
    let body = match b.first() {
        Some(f) if cclv_body_len(*f) == b.len() => b,
        _ if b.len() >= 5 && cclv_body_len(b[4]) + 4 == b.len() => &b[4..],
        _ => {
            return Err(HeifError::invalid(format!(
                "cclv: body is {} bytes, inconsistent with its flags",
                b.len()
            )))
        }
    };
    let mut r = Reader::new(body);
    let flags = r.u8("cclv flags")?;
    let cancel = flags & 0x80 != 0;
    let persistence = flags & 0x40 != 0;
    let primaries = if flags & 0x20 != 0 {
        // §12.1.8: `for (c) { ccv_primaries_x[c]; ccv_primaries_y[c] }`.
        let mut p = [(0i32, 0i32); 3];
        for e in p.iter_mut() {
            e.0 = r.i32("cclv ccv_primaries_x")?;
            e.1 = r.i32("cclv ccv_primaries_y")?;
        }
        Some(p)
    } else {
        None
    };
    let min_luminance = if flags & 0x10 != 0 {
        Some(r.u32("cclv ccv_min_luminance_value")?)
    } else {
        None
    };
    let max_luminance = if flags & 0x08 != 0 {
        Some(r.u32("cclv ccv_max_luminance_value")?)
    } else {
        None
    };
    let avg_luminance = if flags & 0x04 != 0 {
        Some(r.u32("cclv ccv_avg_luminance_value")?)
    } else {
        None
    };
    Ok(Cclv {
        cancel,
        persistence,
        primaries,
        min_luminance,
        max_luminance,
        avg_luminance,
    })
}

fn parse_amve(b: &[u8]) -> Result<Amve> {
    let mut r = Reader::new(plain_or_full(b, 8, "amve")?);
    Ok(Amve {
        ambient_illuminance: r.u32("amve ambient_illuminance")?,
        ambient_light_x: r.u16("amve ambient_light_x")?,
        ambient_light_y: r.u16("amve ambient_light_y")?,
    })
}

fn parse_rloc(b: &[u8]) -> Result<Rloc> {
    let (v, _f, body) = parse_full_box(b)?;
    if v != 0 {
        return Err(HeifError::invalid(format!("rloc version {v}")));
    }
    let mut r = Reader::new(body);
    Ok(Rloc {
        horizontal_offset: r.u32("rloc horizontal_offset")?,
        vertical_offset: r.u32("rloc vertical_offset")?,
    })
}

fn parse_lsel(b: &[u8]) -> Result<Lsel> {
    let mut r = Reader::new(plain_or_full(b, 2, "lsel")?);
    Ok(Lsel {
        layer_id: r.u16("lsel layer_id")?,
    })
}

fn parse_a1lx(b: &[u8]) -> Result<A1lx> {
    let mut r = Reader::new(b);
    let large_size = r.u8("a1lx flags")? & 1 == 1;
    let mut layer_size = [0u32; 3];
    for s in layer_size.iter_mut() {
        *s = if large_size {
            r.u32("a1lx layer_size")?
        } else {
            r.u16("a1lx layer_size")? as u32
        };
    }
    Ok(A1lx {
        large_size,
        layer_size,
    })
}

fn parse_rref(b: &[u8]) -> Result<Rref> {
    let (v, _f, body) = parse_full_box(b)?;
    if v != 0 {
        return Err(HeifError::invalid(format!("rref version {v}")));
    }
    let mut r = Reader::new(body);
    let n = r.u8("rref reference_type_count")? as usize;
    let mut reference_types = Vec::with_capacity(n);
    for _ in 0..n {
        reference_types.push(r.fourcc("rref reference_type")?);
    }
    Ok(Rref { reference_types })
}

fn parse_time(b: &[u8], what: &str) -> Result<TimeInfo> {
    let (_v, _f, body) = parse_full_box(b)?;
    let mut r = Reader::new(body);
    Ok(TimeInfo { time: r.u64(what)? })
}

fn parse_udes(b: &[u8]) -> Result<Udes> {
    let (_v, _f, body) = parse_full_box(b)?;
    let mut r = Reader::new(body);
    let lang = r.cstr("udes lang")?;
    let name = r.cstr("udes name")?;
    let description = r.cstr("udes description")?;
    let tags = if r.is_empty() {
        String::new()
    } else {
        r.cstr("udes tags")?
    };
    Ok(Udes {
        lang,
        name,
        description,
        tags,
    })
}

fn parse_altt(b: &[u8]) -> Result<Altt> {
    let (_v, _f, body) = parse_full_box(b)?;
    let mut r = Reader::new(body);
    let alt_text = r.cstr("altt alt_text")?;
    let alt_lang = if r.is_empty() {
        String::new()
    } else {
        r.cstr("altt alt_lang")?
    };
    Ok(Altt { alt_text, alt_lang })
}

/// Serialization of the typed properties back into `ipco` box bytes
/// (used by the writer).
pub mod write {
    use super::*;
    use crate::boxes::write::{boxed, full_boxed};

    /// Serialize a typed property as a complete box.
    pub fn property_box(p: &Property) -> Vec<u8> {
        match p {
            Property::Ispe(v) => {
                let mut b = v.width.to_be_bytes().to_vec();
                b.extend_from_slice(&v.height.to_be_bytes());
                full_boxed(b"ispe", 0, 0, &b)
            }
            Property::Pixi(v) => {
                let mut b = vec![v.bits_per_channel.len() as u8];
                b.extend_from_slice(&v.bits_per_channel);
                full_boxed(b"pixi", 0, 0, &b)
            }
            Property::Colr(c) => boxed(b"colr", &colr_body(c)),
            Property::Pasp(v) => {
                let mut b = v.h_spacing.to_be_bytes().to_vec();
                b.extend_from_slice(&v.v_spacing.to_be_bytes());
                boxed(b"pasp", &b)
            }
            Property::Clap(c) => {
                let mut b = Vec::with_capacity(32);
                for v in [c.width_n, c.width_d, c.height_n, c.height_d] {
                    b.extend_from_slice(&v.to_be_bytes());
                }
                b.extend_from_slice(&c.horiz_off_n.to_be_bytes());
                b.extend_from_slice(&c.horiz_off_d.to_be_bytes());
                b.extend_from_slice(&c.vert_off_n.to_be_bytes());
                b.extend_from_slice(&c.vert_off_d.to_be_bytes());
                boxed(b"clap", &b)
            }
            Property::Irot(v) => boxed(b"irot", &[v.angle & 3]),
            Property::Imir(v) => boxed(b"imir", &[v.axis & 1]),
            Property::Iscl(v) => {
                let mut b = Vec::with_capacity(8);
                for x in [v.width_num, v.width_den, v.height_num, v.height_den] {
                    b.extend_from_slice(&x.to_be_bytes());
                }
                full_boxed(b"iscl", 0, 0, &b)
            }
            Property::AuxC(a) => {
                let mut b = a.aux_type.as_bytes().to_vec();
                b.push(0);
                b.extend_from_slice(&a.aux_subtype);
                full_boxed(b"auxC", 0, 0, &b)
            }
            Property::HvcC(h) => boxed(b"hvcC", &h.to_bytes()),
            Property::Av1C(a) => boxed(b"av1C", &a.to_bytes()),
            Property::LhvC(c) => boxed(b"lhvC", &c.serialize()),
            Property::AvcC(c) => boxed(b"avcC", &c.serialize()),
            Property::Oinf(o) => full_boxed(b"oinf", 0, 0, &o.serialize()),
            Property::Tols(t) => full_boxed(b"tols", 0, 0, &t.to_be_bytes()),
            Property::Clli(c) => {
                let mut b = c.max_content_light_level.to_be_bytes().to_vec();
                b.extend_from_slice(&c.max_pic_average_light_level.to_be_bytes());
                boxed(b"clli", &b)
            }
            Property::Mdcv(m) => {
                let mut b = Vec::with_capacity(24);
                for (x, y) in m.display_primaries {
                    b.extend_from_slice(&x.to_be_bytes());
                    b.extend_from_slice(&y.to_be_bytes());
                }
                b.extend_from_slice(&m.white_point.0.to_be_bytes());
                b.extend_from_slice(&m.white_point.1.to_be_bytes());
                b.extend_from_slice(&m.max_luminance.to_be_bytes());
                b.extend_from_slice(&m.min_luminance.to_be_bytes());
                boxed(b"mdcv", &b)
            }
            Property::Cclv(c) => {
                let mut flags = 0u8;
                if c.cancel {
                    flags |= 0x80;
                }
                if c.persistence {
                    flags |= 0x40;
                }
                if c.primaries.is_some() {
                    flags |= 0x20;
                }
                if c.min_luminance.is_some() {
                    flags |= 0x10;
                }
                if c.max_luminance.is_some() {
                    flags |= 0x08;
                }
                if c.avg_luminance.is_some() {
                    flags |= 0x04;
                }
                let mut b = vec![flags];
                if let Some(p) = c.primaries {
                    for (x, y) in p {
                        b.extend_from_slice(&x.to_be_bytes());
                        b.extend_from_slice(&y.to_be_bytes());
                    }
                }
                for v in [c.min_luminance, c.max_luminance, c.avg_luminance]
                    .into_iter()
                    .flatten()
                {
                    b.extend_from_slice(&v.to_be_bytes());
                }
                // §12.1.8: a Box, not a FullBox.
                boxed(b"cclv", &b)
            }
            Property::Amve(a) => {
                let mut b = a.ambient_illuminance.to_be_bytes().to_vec();
                b.extend_from_slice(&a.ambient_light_x.to_be_bytes());
                b.extend_from_slice(&a.ambient_light_y.to_be_bytes());
                // §12.1.9: a Box, not a FullBox.
                boxed(b"amve", &b)
            }
            Property::Rloc(r) => {
                let mut b = r.horizontal_offset.to_be_bytes().to_vec();
                b.extend_from_slice(&r.vertical_offset.to_be_bytes());
                full_boxed(b"rloc", 0, 0, &b)
            }
            Property::Lsel(l) => boxed(b"lsel", &l.layer_id.to_be_bytes()),
            Property::A1op(a) => boxed(b"a1op", &[a.op_index]),
            Property::A1lx(a) => {
                let mut b = vec![a.large_size as u8];
                for s in a.layer_size {
                    if a.large_size {
                        b.extend_from_slice(&s.to_be_bytes());
                    } else {
                        b.extend_from_slice(&(s as u16).to_be_bytes());
                    }
                }
                boxed(b"a1lx", &b)
            }
            Property::Rref(r) => {
                let mut b = vec![r.reference_types.len() as u8];
                for t in &r.reference_types {
                    b.extend_from_slice(t);
                }
                full_boxed(b"rref", 0, 0, &b)
            }
            Property::Crtt(t) => full_boxed(b"crtt", 0, 0, &t.time.to_be_bytes()),
            Property::Mdft(t) => full_boxed(b"mdft", 0, 0, &t.time.to_be_bytes()),
            Property::Udes(u) => {
                let mut b = Vec::new();
                for s in [&u.lang, &u.name, &u.description, &u.tags] {
                    b.extend_from_slice(s.as_bytes());
                    b.push(0);
                }
                full_boxed(b"udes", 0, 0, &b)
            }
            Property::Altt(a) => {
                let mut b = a.alt_text.as_bytes().to_vec();
                b.push(0);
                b.extend_from_slice(a.alt_lang.as_bytes());
                b.push(0);
                full_boxed(b"altt", 0, 0, &b)
            }
            Property::Unknown(raw) => {
                if let Some(ut) = raw.user_type {
                    let mut b = ut.to_vec();
                    b.extend_from_slice(&raw.body);
                    boxed(b"uuid", &b)
                } else {
                    boxed(&raw.box_type, &raw.body)
                }
            }
        }
    }

    /// The `colr` box body for a [`Colr`].
    pub fn colr_body(c: &Colr) -> Vec<u8> {
        match c {
            Colr::Nclx {
                primaries,
                transfer,
                matrix,
                full_range,
            } => {
                let mut b = b"nclx".to_vec();
                b.extend_from_slice(&primaries.to_be_bytes());
                b.extend_from_slice(&transfer.to_be_bytes());
                b.extend_from_slice(&matrix.to_be_bytes());
                b.push(if *full_range { 0x80 } else { 0 });
                b
            }
            Colr::Icc {
                restricted,
                profile,
            } => {
                let mut b = if *restricted {
                    b"rICC".to_vec()
                } else {
                    b"prof".to_vec()
                };
                b.extend_from_slice(profile);
                b
            }
            Colr::Other {
                colour_type,
                payload,
            } => {
                let mut b = colour_type.to_vec();
                b.extend_from_slice(payload);
                b
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::{parse_box_header, payload};

    fn raw_of(bytes: &[u8]) -> RawProperty {
        let h = parse_box_header(bytes, 0).unwrap();
        RawProperty {
            box_type: h.box_type,
            user_type: h.user_type,
            body: payload(bytes, &h).to_vec(),
            box_size: h.total_len(),
        }
    }

    fn round_trip(p: Property) {
        let bytes = write::property_box(&p);
        let back = Property::parse(&raw_of(&bytes)).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn every_typed_property_round_trips() {
        round_trip(Property::Ispe(Ispe {
            width: 640,
            height: 480,
        }));
        round_trip(Property::Pixi(Pixi {
            bits_per_channel: vec![8, 8, 8],
        }));
        round_trip(Property::Colr(Colr::MIAF_DEFAULT));
        round_trip(Property::Colr(Colr::Icc {
            restricted: false,
            profile: vec![1, 2, 3],
        }));
        round_trip(Property::Colr(Colr::Icc {
            restricted: true,
            profile: vec![],
        }));
        round_trip(Property::Colr(Colr::Other {
            colour_type: *b"nclc",
            payload: vec![0; 6],
        }));
        round_trip(Property::Pasp(Pasp {
            h_spacing: 1,
            v_spacing: 1,
        }));
        round_trip(Property::Clap(Clap::integer(1, 1, -31, -31)));
        round_trip(Property::Irot(Irot { angle: 3 }));
        round_trip(Property::Imir(Imir { axis: 1 }));
        round_trip(Property::Iscl(Iscl {
            width_num: 1,
            width_den: 2,
            height_num: 3,
            height_den: 4,
        }));
        round_trip(Property::AuxC(AuxC {
            aux_type: AUX_URN_ALPHA.into(),
            aux_subtype: vec![0, 0, 0, 0],
        }));
        round_trip(Property::LhvC(LhevcConfig {
            configuration_version: 1,
            min_spatial_segmentation_idc: 0,
            parallelism_type: 0,
            num_temporal_layers: 1,
            temporal_id_nested: true,
            length_size: 4,
            arrays: vec![],
            raw: vec![1, 0xf0, 0, 0xfc, 0x0f, 0],
        }));
        round_trip(Property::AvcC(AvcConfig {
            configuration_version: 1,
            profile_idc: 66,
            profile_compatibility: 0xc0,
            level_idc: 30,
            length_size: 4,
            sps: vec![vec![0x67, 0x42, 0xc0, 0x1e]],
            pps: vec![vec![0x68, 0xce]],
            chroma_format: None,
            bit_depth_luma_minus8: None,
            bit_depth_chroma_minus8: None,
            sps_ext: vec![],
            raw: vec![
                1, 66, 0xc0, 30, 0xff, 0xe1, 0, 4, 0x67, 0x42, 0xc0, 0x1e, 1, 0, 2, 0x68, 0xce,
            ],
        }));
        round_trip(Property::Tols(3));
        round_trip(Property::Oinf(OperatingPoints {
            scalability_mask: 0,
            ptls: vec![],
            operating_points: vec![],
            layers: vec![],
            raw: vec![0, 0, 0, 0, 0, 0],
        }));
        round_trip(Property::Clli(Clli {
            max_content_light_level: 1000,
            max_pic_average_light_level: 400,
        }));
        round_trip(Property::Mdcv(Mdcv {
            display_primaries: [(13250, 34500), (7500, 3000), (34000, 16000)],
            white_point: (15635, 16450),
            max_luminance: 10_000_000,
            min_luminance: 50,
        }));
        round_trip(Property::Cclv(Cclv {
            cancel: false,
            persistence: true,
            primaries: Some([(1, -2), (3, 4), (5, 6)]),
            min_luminance: Some(1),
            max_luminance: None,
            avg_luminance: Some(3),
        }));
        round_trip(Property::Amve(Amve {
            ambient_illuminance: 314159,
            ambient_light_x: 15635,
            ambient_light_y: 16450,
        }));
    }

    /// ISO/IEC 14496-12 §12.1.7: the primaries are interleaved
    /// `(x, y)` pairs — a symmetric parse / write pair would hide a
    /// planar (x x x y y y) reading, so pin the bytes.
    #[test]
    fn mdcv_bytes_are_interleaved_per_primary() {
        let m = Mdcv {
            display_primaries: [(13250, 34500), (7500, 3000), (34000, 16000)],
            white_point: (15635, 16450),
            max_luminance: 10_000_000,
            min_luminance: 50,
        };
        let bytes = write::property_box(&Property::Mdcv(m));
        let body = &bytes[8..];
        assert_eq!(body.len(), 24, "plain Box, no version / flags");
        let u16_at = |i: usize| u16::from_be_bytes([body[i], body[i + 1]]);
        assert_eq!(
            [
                u16_at(0),
                u16_at(2),
                u16_at(4),
                u16_at(6),
                u16_at(8),
                u16_at(10)
            ],
            [13250, 34500, 7500, 3000, 34000, 16000]
        );
        assert_eq!((u16_at(12), u16_at(14)), (15635, 16450));
        assert_eq!(Property::parse(&raw_of(&bytes)).unwrap(), Property::Mdcv(m));
        // §12.1.8 `cclv` primaries likewise.
        let c = Cclv {
            cancel: false,
            persistence: false,
            primaries: Some([(1, -2), (3, 4), (5, 6)]),
            min_luminance: None,
            max_luminance: None,
            avg_luminance: None,
        };
        let bytes = write::property_box(&Property::Cclv(c));
        let body = &bytes[bytes.len() - 25..];
        let i32_at =
            |i: usize| i32::from_be_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
        assert_eq!(
            [
                i32_at(1),
                i32_at(5),
                i32_at(9),
                i32_at(13),
                i32_at(17),
                i32_at(21)
            ],
            [1, -2, 3, 4, 5, 6]
        );
        round_trip(Property::Rloc(Rloc {
            horizontal_offset: 64,
            vertical_offset: 128,
        }));
        round_trip(Property::Lsel(Lsel { layer_id: 2 }));
        round_trip(Property::A1op(A1op { op_index: 1 }));
        round_trip(Property::A1lx(A1lx {
            large_size: true,
            layer_size: [70000, 1, 0],
        }));
        round_trip(Property::A1lx(A1lx {
            large_size: false,
            layer_size: [7, 1, 0],
        }));
        round_trip(Property::Rref(Rref {
            reference_types: vec![*b"pred"],
        }));
        round_trip(Property::Crtt(TimeInfo { time: 1 << 40 }));
        round_trip(Property::Mdft(TimeInfo { time: 7 }));
        round_trip(Property::Udes(Udes {
            lang: "en".into(),
            name: "n".into(),
            description: "d".into(),
            tags: "a,b".into(),
        }));
        round_trip(Property::Altt(Altt {
            alt_text: "text".into(),
            alt_lang: "en".into(),
        }));
        round_trip(Property::Unknown(RawProperty {
            box_type: *b"zzzz",
            user_type: None,
            body: vec![1, 2, 3],
            box_size: 11,
        }));
    }

    #[test]
    fn plain_or_full_shapes_accepted() {
        let clli_full = crate::boxes::write::full_boxed(b"clli", 0, 0, &[0, 1, 0, 2]);
        match Property::parse(&raw_of(&clli_full)).unwrap() {
            Property::Clli(c) => assert_eq!(c.max_content_light_level, 1),
            other => panic!("{other:?}"),
        }
        let bad = crate::boxes::write::boxed(b"clli", &[0, 1, 0]);
        assert!(Property::parse(&raw_of(&bad)).is_err());
        // cclv / amve: the FullBox-prefixed shape is read too.
        let c = Cclv {
            cancel: false,
            persistence: false,
            primaries: None,
            min_luminance: Some(5),
            max_luminance: Some(6),
            avg_luminance: None,
        };
        let plain = write::property_box(&Property::Cclv(c));
        let full = crate::boxes::write::full_boxed(b"cclv", 0, 0, &plain[8..]);
        assert_eq!(Property::parse(&raw_of(&full)).unwrap(), Property::Cclv(c));
        let a = Amve {
            ambient_illuminance: 1,
            ambient_light_x: 2,
            ambient_light_y: 3,
        };
        let full = crate::boxes::write::full_boxed(
            b"amve",
            0,
            0,
            &write::property_box(&Property::Amve(a))[8..],
        );
        assert_eq!(Property::parse(&raw_of(&full)).unwrap(), Property::Amve(a));
        let short = crate::boxes::write::boxed(b"cclv", &[0x38, 0, 0]);
        assert!(Property::parse(&raw_of(&short)).is_err());
    }

    /// ISO/IEC 14496-12 §12.1.6–9 on the wire: plain Boxes (no
    /// version / flags), the field widths and order of each clause.
    #[test]
    fn hdr_boxes_have_the_isobmff_wire_shape() {
        let clli = write::property_box(&Property::Clli(Clli {
            max_content_light_level: 0x1234,
            max_pic_average_light_level: 0x0056,
        }));
        assert_eq!(&clli[4..8], b"clli");
        assert_eq!(&clli[8..], &[0x12, 0x34, 0x00, 0x56]);
        let mdcv = write::property_box(&Property::Mdcv(Mdcv {
            display_primaries: [(1, 2), (3, 4), (5, 6)],
            white_point: (7, 8),
            max_luminance: 9,
            min_luminance: 10,
        }));
        assert_eq!(mdcv.len(), 8 + 24);
        assert_eq!(
            &mdcv[8..],
            &[0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0, 8, 0, 0, 0, 9, 0, 0, 0, 10]
        );
        let cclv = write::property_box(&Property::Cclv(Cclv {
            cancel: false,
            persistence: false,
            primaries: Some([(1, -1), (2, -2), (3, -3)]),
            min_luminance: None,
            max_luminance: Some(0x0102_0304),
            avg_luminance: Some(7),
        }));
        // flags: primaries (0x20) + max (0x08) + avg (0x04).
        assert_eq!(cclv.len(), 8 + 1 + 24 + 8);
        assert_eq!(cclv[8], 0x2c);
        assert_eq!(&cclv[9..17], &[0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(&cclv[33..41], &[1, 2, 3, 4, 0, 0, 0, 7]);
        let amve = write::property_box(&Property::Amve(Amve {
            ambient_illuminance: 0x0001_0203,
            ambient_light_x: 0x0405,
            ambient_light_y: 0x0607,
        }));
        assert_eq!(amve.len(), 8 + 8);
        assert_eq!(&amve[8..], &[0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn clap_resolution() {
        // 64x64 coded, 1x1 clean aperture centred at (0,0): the aperture
        // centre is (31.5 + off); with off = -31.5 the left edge is 0.
        let c = Clap {
            width_n: 1,
            width_d: 1,
            height_n: 1,
            height_d: 1,
            horiz_off_n: -63,
            horiz_off_d: 2,
            vert_off_n: -63,
            vert_off_d: 2,
        };
        assert_eq!(
            c.resolve(64, 64).unwrap(),
            CropRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            }
        );
        // Centred crop.
        let c = Clap::integer(32, 16, 0, 0);
        assert_eq!(
            c.resolve(64, 64).unwrap(),
            CropRect {
                x: 16,
                y: 24,
                width: 32,
                height: 16
            }
        );
        // for_rect inverts resolve.
        let r = CropRect {
            x: 3,
            y: 5,
            width: 7,
            height: 9,
        };
        assert_eq!(Clap::for_rect(20, 30, r).resolve(20, 30).unwrap(), r);
        // Non-integer edge (centred 31 in 64 → 16.5) is refused.
        assert!(Clap::integer(31, 64, 0, 0).resolve(64, 64).is_err());
        // Out of range.
        assert!(Clap::integer(65, 1, 0, 0).resolve(64, 64).is_err());
        assert!(Clap::integer(1, 1, 100, 0).resolve(64, 64).is_err());
        assert!(Clap {
            width_d: 0,
            ..Clap::integer(1, 1, 0, 0)
        }
        .resolve(4, 4)
        .is_err());
    }

    #[test]
    fn iscl_output_size_uses_ceil() {
        let s = Iscl {
            width_num: 1,
            width_den: 3,
            height_num: 2,
            height_den: 3,
        };
        assert_eq!(s.output_size(100, 100).unwrap(), (34, 67));
        assert!(Iscl { width_num: 0, ..s }.output_size(1, 1).is_err());
    }

    #[test]
    fn descriptive_stops_at_first_transformative_or_unknown() {
        let mk = |p: Property, essential: bool| PropertyEntry {
            index: 1,
            essential,
            property: p,
        };
        let props = ItemProperties {
            entries: vec![
                mk(
                    Property::Ispe(Ispe {
                        width: 4,
                        height: 2,
                    }),
                    false,
                ),
                mk(Property::Irot(Irot { angle: 1 }), true),
                mk(
                    Property::Pixi(Pixi {
                        bits_per_channel: vec![8],
                    }),
                    false,
                ),
                mk(
                    Property::Unknown(RawProperty {
                        box_type: *b"zzzz",
                        user_type: None,
                        body: vec![],
                        box_size: 8,
                    }),
                    true,
                ),
                mk(Property::Imir(Imir { axis: 0 }), true),
            ],
        };
        assert_eq!(props.ispe().unwrap().width, 4);
        assert!(props.pixi().is_none(), "pixi after irot is ignored");
        assert_eq!(props.transformative().count(), 2);
        assert_eq!(props.unsupported_essential(), vec![*b"zzzz"]);
        assert_eq!(props.output_size((4, 2), false).unwrap(), (2, 4));
        assert_eq!(props.output_size((4, 2), true).unwrap(), (2, 4));
        assert_eq!(props.irot().unwrap().angle, 1);
        assert_eq!(props.imir().unwrap().axis, 0);
    }

    #[test]
    fn aux_kind_from_urn() {
        let a = |s: &str| AuxC {
            aux_type: s.into(),
            aux_subtype: vec![],
        };
        assert_eq!(a(AUX_URN_ALPHA).kind(), AuxKind::Alpha);
        assert_eq!(a(AUX_URN_ALPHA_HEVC).kind(), AuxKind::Alpha);
        assert_eq!(a(AUX_URN_DEPTH).kind(), AuxKind::Depth);
        assert_eq!(a(AUX_URN_DEPTH_HEVC).kind(), AuxKind::Depth);
        assert_eq!(a("urn:com:example:x").kind(), AuxKind::Other);
    }
}
