//! The low-overhead image file format (ISO/IEC 23008-12:2025/Amd 2:2026
//! Annex O): a `mif3` file is `ftyp` + one `MinimizedImageBox` (`mini`)
//! — a bit-packed header followed by the raw chunks (codec
//! configurations, ICC profiles, gain-map metadata, alpha / gain-map /
//! main item data, Exif, XMP).
//!
//! O.4 is normative: "Readers shall treat a MinimizedImageBox as if it
//! were the equivalent MetaBox and MediaDataBox". This module parses
//! the box ([`MinimizedImage::parse`]), rebuilds the equivalent file —
//! `ftyp` with the O.2.1.2 implied brands, `meta` with the fixed item
//! ids (1 main, 2 alpha, 3 `tmap`, 4 gain map, 6 Exif / `dExf`, 7 XMP),
//! the 32-slot `ipco` (absent slots as `free` boxes), the `ipma` order
//! of O.4.6, `iloc` over the O.4.10 `mdat` order —
//! ([`MinimizedImage::equivalent_file`]) and serialises the box back
//! ([`MinimizedImage::to_file`]) for the writer direction.
//! [`crate::HeifFile`] expands a `mini` file on parse, so every reader
//! of this crate takes the regular `meta` paths.
//!
//! One slot of the expansion is not implementable from the staged
//! text: entry 30, the `AlphaInformationProperty` a codec with native
//! translucency (`alpha_flag` = 1 and `alpha_item_data_size` = 0)
//! expands to, is named but not defined by Amd 2 (neither its box type
//! nor its syntax); such files are refused with a typed
//! [`HeifError::Unsupported`].

use crate::boxes::write::{boxed, full_boxed};
use crate::boxes::FourCc;
use crate::error::{HeifError, Result};
use crate::ftyp::FileType;

/// `mif3` structural brand (Annex O.2.1).
pub const BRAND_MIF3: FourCc = crate::ftyp::BRAND_MIF3;
/// `vvi3` codec brand for VVC under `mif3` (L.4.3).
pub const BRAND_VVI3: FourCc = *b"vvi3";
/// MIME type of the low-overhead format (Annex P).
pub const MIME_TYPE: &str = "image/hif2";
/// File extension of the low-overhead format (Annex P).
pub const FILE_EXTENSION: &str = "hmg";

/// Fixed item ids of the O.4 expansion.
pub mod item_id {
    /// The main image.
    pub const MAIN: u32 = 1;
    /// The alpha auxiliary (hidden).
    pub const ALPHA: u32 = 2;
    /// The `tmap` derived image.
    pub const TMAP: u32 = 3;
    /// The gain map (hidden).
    pub const GAIN_MAP: u32 = 4;
    /// The `altr` group [tmap, main].
    pub const ALTR_GROUP: u32 = 5;
    /// Exif (`Exif`, or `dExf` when deflate-compressed).
    pub const EXIF: u32 = 6;
    /// XMP (`mime`, `application/rdf+xml`).
    pub const XMP: u32 = 7;
}

/// Pixel sample format of an image in the box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SampleFormat {
    /// Unsigned integer samples of `bits` (8..=16).
    Integer {
        /// `bits_per_channel`.
        bits: u8,
    },
    /// Floating-point samples of 16 / 32 / 64 bits
    /// (`bit_depth_log2_minus4` 0 / 1 / 2).
    Float {
        /// `bits_per_channel`.
        bits: u8,
    },
}

impl SampleFormat {
    /// `bits_per_channel` of the reconstructed image.
    pub fn bits(&self) -> u8 {
        match self {
            SampleFormat::Integer { bits } | SampleFormat::Float { bits } => *bits,
        }
    }

    /// `true` for the floating-point formats.
    pub fn is_float(&self) -> bool {
        matches!(self, SampleFormat::Float { .. })
    }
}

/// Chroma layout of an image in the box (Table O.4 plus the centring
/// bits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MiniChroma {
    /// `chroma_subsampling`: 0 = 4:0:0, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4.
    pub subsampling: u8,
    /// `chroma_is_horizontally_centered` (4:2:0 / 4:2:2 only).
    pub horizontally_centered: bool,
    /// `chroma_is_vertically_centered` (4:2:0 only).
    pub vertically_centered: bool,
}
impl MiniChroma {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(subsampling: u8, horizontally_centered: bool, vertically_centered: bool) -> Self {
        Self {
            subsampling,
            horizontally_centered,
            vertically_centered,
        }
    }
}

/// The HDR signalling blocks (`clli` / `mdcv` / `cclv` / `amve` /
/// `reve` / `ndwt` box *bodies*, O.3.3) attached to one image.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MiniHdrBoxes {
    /// `ContentLightLevelBox` body (4 bytes).
    pub clli: Option<Vec<u8>>,
    /// `MasteringDisplayColourVolumeBox` body (24 bytes).
    pub mdcv: Option<Vec<u8>>,
    /// `ContentColourVolumeBox` body (flag byte + present values).
    pub cclv: Option<Vec<u8>>,
    /// `AmbientViewingEnvironmentBox` body (8 bytes).
    pub amve: Option<Vec<u8>>,
    /// `ReferenceViewingEnvironmentBox` body (20 bytes, no FullBox
    /// header).
    pub reve: Option<Vec<u8>>,
    /// `NominalDiffuseWhiteBox` body (4 bytes, no FullBox header).
    pub ndwt: Option<Vec<u8>>,
}
impl MiniHdrBoxes {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(
        clli: Option<Vec<u8>>,
        mdcv: Option<Vec<u8>>,
        cclv: Option<Vec<u8>>,
        amve: Option<Vec<u8>>,
        reve: Option<Vec<u8>>,
        ndwt: Option<Vec<u8>>,
    ) -> Self {
        Self {
            clli,
            mdcv,
            cclv,
            amve,
            reve,
            ndwt,
        }
    }
}

impl MiniHdrBoxes {
    fn is_empty(&self) -> bool {
        self.clli.is_none()
            && self.mdcv.is_none()
            && self.cclv.is_none()
            && self.amve.is_none()
            && self.reve.is_none()
            && self.ndwt.is_none()
    }
}

/// The gain map of a `mini` box (`gainmap_flag` = 1).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MiniGainMap {
    /// `gainmap_width_minus1 + 1`.
    pub width: u32,
    /// `gainmap_height_minus1 + 1`.
    pub height: u32,
    /// `gainmap_matrix_coefficients`.
    pub matrix_coefficients: u8,
    /// `gainmap_full_range_flag`.
    pub full_range: bool,
    /// Chroma layout of the gain map.
    pub chroma: MiniChroma,
    /// Sample format of the gain map.
    pub format: SampleFormat,
    /// Tone-mapped image CICP `(primaries, transfer, matrix,
    /// full_range)` when `tmap_explicit_cicp_flag` (else sRGB, Table
    /// O.5).
    pub tmap_cicp: Option<(u8, u8, u8, bool)>,
    /// Tone-mapped image ICC profile (`tmap_icc_flag`).
    pub tmap_icc: Option<Vec<u8>>,
    /// HDR boxes of the tone-mapped image.
    pub tmap_hdr: MiniHdrBoxes,
    /// ISO 21496-1 `GainMapMetadata` (may be empty).
    pub metadata: Vec<u8>,
    /// Gain-map codec configuration; `None` = identical to the main
    /// item's (`gainmap_item_codec_config_size` 0).
    pub codec_config: Option<Vec<u8>>,
    /// Coded gain-map item data (empty = the reserved "no gain map
    /// item" case).
    pub data: Vec<u8>,
}
impl MiniGainMap {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: u32,
        height: u32,
        matrix_coefficients: u8,
        full_range: bool,
        chroma: MiniChroma,
        format: SampleFormat,
        tmap_cicp: Option<(u8, u8, u8, bool)>,
        tmap_icc: Option<Vec<u8>>,
        tmap_hdr: MiniHdrBoxes,
        metadata: Vec<u8>,
        codec_config: Option<Vec<u8>>,
        data: Vec<u8>,
    ) -> Self {
        Self {
            width,
            height,
            matrix_coefficients,
            full_range,
            chroma,
            format,
            tmap_cicp,
            tmap_icc,
            tmap_hdr,
            metadata,
            codec_config,
            data,
        }
    }
}

/// A parsed `MinimizedImageBox` (O.3).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MinimizedImage {
    /// `infe_type` + `codec_config_type` when
    /// `explicit_codec_types_flag` (else inferred from the brand in
    /// `ftyp.minor_version`).
    pub explicit_codec_types: Option<(FourCc, FourCc)>,
    /// Main / alpha sample format.
    pub format: SampleFormat,
    /// `full_range_flag` of the main image.
    pub full_range: bool,
    /// Main image chroma layout.
    pub chroma: MiniChroma,
    /// Exif orientation (`orientation_minus1 + 1`, 1..=8).
    pub orientation: u8,
    /// `width_minus1 + 1`.
    pub width: u32,
    /// `height_minus1 + 1`.
    pub height: u32,
    /// `(primaries, transfer, matrix)` when `explicit_cicp_flag`; else
    /// the Table O.3 defaults apply ([`MinimizedImage::cicp`]).
    pub explicit_cicp: Option<(u8, u8, u8)>,
    /// Main image ICC profile (`icc_flag`).
    pub icc: Option<Vec<u8>>,
    /// `alpha_flag`: the image has alpha.
    pub alpha: bool,
    /// `alpha_is_premultiplied`.
    pub alpha_premultiplied: bool,
    /// `hdr_flag`.
    pub hdr: bool,
    /// HDR boxes of the main image (only with `hdr`).
    pub hdr_boxes: MiniHdrBoxes,
    /// The gain map (only with `hdr`).
    pub gain_map: Option<MiniGainMap>,
    /// Main item codec configuration (may be empty).
    pub main_codec_config: Vec<u8>,
    /// Main item data.
    pub main_data: Vec<u8>,
    /// Alpha codec configuration; `None` = the main item's.
    pub alpha_codec_config: Option<Vec<u8>>,
    /// Alpha auxiliary item data (empty with `alpha` = native alpha).
    pub alpha_data: Vec<u8>,
    /// `exif_xmp_compressed_flag`: Exif / XMP are deflate streams.
    pub exif_xmp_compressed: bool,
    /// Exif chunk (an `ExifDataBlock`, A.2.1).
    pub exif: Option<Vec<u8>>,
    /// XMP chunk.
    pub xmp: Option<Vec<u8>>,
}
impl MinimizedImage {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        explicit_codec_types: Option<(FourCc, FourCc)>,
        format: SampleFormat,
        full_range: bool,
        chroma: MiniChroma,
        orientation: u8,
        width: u32,
        height: u32,
        explicit_cicp: Option<(u8, u8, u8)>,
        icc: Option<Vec<u8>>,
        alpha: bool,
        alpha_premultiplied: bool,
        hdr: bool,
        hdr_boxes: MiniHdrBoxes,
        gain_map: Option<MiniGainMap>,
        main_codec_config: Vec<u8>,
        main_data: Vec<u8>,
        alpha_codec_config: Option<Vec<u8>>,
        alpha_data: Vec<u8>,
        exif_xmp_compressed: bool,
        exif: Option<Vec<u8>>,
        xmp: Option<Vec<u8>>,
    ) -> Self {
        Self {
            explicit_codec_types,
            format,
            full_range,
            chroma,
            orientation,
            width,
            height,
            explicit_cicp,
            icc,
            alpha,
            alpha_premultiplied,
            hdr,
            hdr_boxes,
            gain_map,
            main_codec_config,
            main_data,
            alpha_codec_config,
            alpha_data,
            exif_xmp_compressed,
            exif,
            xmp,
        }
    }
}

/// MSB-first bit reader over the `mini` payload.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: u32, what: &str) -> Result<u32> {
        debug_assert!(n <= 32);
        if self.pos + n as usize > self.data.len() * 8 {
            return Err(HeifError::invalid(format!(
                "mini: truncated at {what} (bit {})",
                self.pos
            )));
        }
        let mut v = 0u64;
        for _ in 0..n {
            let byte = self.data[self.pos / 8];
            let bit = (byte >> (7 - (self.pos % 8))) & 1;
            v = (v << 1) | bit as u64;
            self.pos += 1;
        }
        Ok(v as u32)
    }

    fn flag(&mut self, what: &str) -> Result<bool> {
        Ok(self.read(1, what)? == 1)
    }

    fn bytes(&mut self, n: usize, what: &str) -> Result<Vec<u8>> {
        if self.pos % 8 == 0 {
            let start = self.pos / 8;
            let end = start
                .checked_add(n)
                .filter(|e| *e <= self.data.len())
                .ok_or_else(|| {
                    HeifError::invalid(format!(
                        "mini: {what} ({n} bytes) runs past the box ({} bytes)",
                        self.data.len()
                    ))
                })?;
            self.pos = end * 8;
            return Ok(self.data[start..end].to_vec());
        }
        if self.pos + n * 8 > self.data.len() * 8 {
            return Err(HeifError::invalid(format!(
                "mini: {what} ({n} bytes) runs past the box"
            )));
        }
        (0..n)
            .map(|_| self.read(8, what).map(|b| b as u8))
            .collect()
    }
}

/// MSB-first bit writer.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn put(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            if self.bits % 8 == 0 {
                self.out.push(0);
            }
            let bit = ((v >> i) & 1) as u8;
            let last = self.out.len() - 1;
            self.out[last] |= bit << (7 - (self.bits % 8));
            self.bits += 1;
        }
    }

    fn flag(&mut self, b: bool) {
        self.put(1, b as u32);
    }

    fn bytes(&mut self, b: &[u8]) {
        if self.bits % 8 == 0 {
            self.out.extend_from_slice(b);
            self.bits += b.len() * 8;
        } else {
            for v in b {
                self.put(8, *v as u32);
            }
        }
    }
}

fn read_format(b: &mut Bits<'_>, float: bool, what: &str) -> Result<SampleFormat> {
    if float {
        let l = b.read(2, what)?;
        if l > 2 {
            return Err(HeifError::invalid(format!(
                "mini: {what} bit_depth_log2_minus4 {l} is reserved"
            )));
        }
        Ok(SampleFormat::Float { bits: 1 << (l + 4) })
    } else if b.flag(what)? {
        Ok(SampleFormat::Integer {
            bits: b.read(3, what)? as u8 + 9,
        })
    } else {
        Ok(SampleFormat::Integer { bits: 8 })
    }
}

fn write_format(w: &mut BitWriter, f: SampleFormat) -> Result<()> {
    match f {
        SampleFormat::Float { bits } => {
            let l = match bits {
                16 => 0,
                32 => 1,
                64 => 2,
                _ => {
                    return Err(HeifError::invalid(format!(
                        "mini: float samples of {bits} bits"
                    )))
                }
            };
            w.put(2, l);
        }
        SampleFormat::Integer { bits: 8 } => w.flag(false),
        SampleFormat::Integer { bits } if (9..=16).contains(&bits) => {
            w.flag(true);
            w.put(3, bits as u32 - 9);
        }
        SampleFormat::Integer { bits } => {
            return Err(HeifError::invalid(format!(
                "mini: integer samples of {bits} bits (8..=16)"
            )))
        }
    }
    Ok(())
}

fn read_chroma(b: &mut Bits<'_>, subsampling: u8) -> Result<MiniChroma> {
    let horizontally_centered = if subsampling == 1 || subsampling == 2 {
        b.flag("chroma_is_horizontally_centered")?
    } else {
        false
    };
    let vertically_centered = if subsampling == 1 {
        b.flag("chroma_is_vertically_centered")?
    } else {
        false
    };
    Ok(MiniChroma {
        subsampling,
        horizontally_centered,
        vertically_centered,
    })
}

fn write_chroma_bits(w: &mut BitWriter, c: MiniChroma) {
    if c.subsampling == 1 || c.subsampling == 2 {
        w.flag(c.horizontally_centered);
    }
    if c.subsampling == 1 {
        w.flag(c.vertically_centered);
    }
}

/// Body length of an HDR box from its leading bytes, for the variable
/// `cclv`.
fn read_hdr_boxes(b: &mut Bits<'_>, prefix: &str) -> Result<MiniHdrBoxes> {
    let mut flags = [false; 6];
    for f in flags.iter_mut() {
        *f = b.flag(prefix)?;
    }
    let mut h = MiniHdrBoxes::default();
    if flags[0] {
        h.clli = Some(b.bytes(4, "clli")?);
    }
    if flags[1] {
        h.mdcv = Some(b.bytes(24, "mdcv")?);
    }
    if flags[2] {
        let first = b.bytes(1, "cclv flags")?;
        let len = crate::props::cclv_body_len(first[0]);
        let mut body = first;
        body.extend(b.bytes(len - 1, "cclv")?);
        h.cclv = Some(body);
    }
    if flags[3] {
        h.amve = Some(b.bytes(8, "amve")?);
    }
    if flags[4] {
        h.reve = Some(b.bytes(20, "reve")?);
    }
    if flags[5] {
        h.ndwt = Some(b.bytes(4, "ndwt")?);
    }
    Ok(h)
}

fn write_hdr_boxes(w: &mut BitWriter, h: &MiniHdrBoxes) -> Result<()> {
    let checks: [(&Option<Vec<u8>>, usize, &str); 6] = [
        (&h.clli, 4, "clli"),
        (&h.mdcv, 24, "mdcv"),
        (&h.cclv, 0, "cclv"),
        (&h.amve, 8, "amve"),
        (&h.reve, 20, "reve"),
        (&h.ndwt, 4, "ndwt"),
    ];
    for (b, len, what) in checks {
        if let Some(v) = b {
            let want = if len == 0 {
                v.first()
                    .map(|f| crate::props::cclv_body_len(*f))
                    .unwrap_or(1)
            } else {
                len
            };
            if v.len() != want {
                return Err(HeifError::invalid(format!(
                    "mini: {what} body is {} bytes, expected {want}",
                    v.len()
                )));
            }
        }
        w.flag(b.is_some());
    }
    for (b, _, _) in checks {
        if let Some(v) = b {
            w.bytes(v);
        }
    }
    Ok(())
}

/// Upper bound on the expanded file (every chunk size is bounded by
/// the box, which is bounded by the file; this caps the equivalent
/// `meta` + `mdat` rebuilt from it).
pub const MAX_EXPANDED_BYTES: usize = 1 << 30;

impl MinimizedImage {
    /// Parse a `mini` box payload (the bytes after the box header).
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut b = Bits {
            data: payload,
            pos: 0,
        };
        let version = b.read(2, "version")?;
        if version != 0 {
            return Err(HeifError::unsupported(format!(
                "MinimizedImageBox version {version} (Amd 2:2026 defines 0)"
            )));
        }
        let explicit_codec_types_flag = b.flag("explicit_codec_types_flag")?;
        let float_flag = b.flag("float_flag")?;
        let full_range = b.flag("full_range_flag")?;
        let alpha = b.flag("alpha_flag")?;
        let explicit_cicp_flag = b.flag("explicit_cicp_flag")?;
        let hdr = b.flag("hdr_flag")?;
        let icc_flag = b.flag("icc_flag")?;
        let exif_flag = b.flag("exif_flag")?;
        let xmp_flag = b.flag("xmp_flag")?;
        let subsampling = b.read(2, "chroma_subsampling")? as u8;
        let orientation = b.read(3, "orientation_minus1")? as u8 + 1;
        let large_dimensions = b.flag("large_dimensions_flag")?;
        let dim_bits = if large_dimensions { 15 } else { 7 };
        let width = b.read(dim_bits, "width_minus1")? + 1;
        let height = b.read(dim_bits, "height_minus1")? + 1;
        let chroma = read_chroma(&mut b, subsampling)?;
        let format = read_format(&mut b, float_flag, "main bit depth")?;
        let alpha_premultiplied = if alpha {
            b.flag("alpha_is_premultiplied")?
        } else {
            false
        };
        let explicit_cicp = if explicit_cicp_flag {
            Some((
                b.read(8, "colour_primaries")? as u8,
                b.read(8, "transfer_characteristics")? as u8,
                b.read(8, "matrix_coefficients")? as u8,
            ))
        } else {
            None
        };
        let explicit_codec_types = if explicit_codec_types_flag {
            let infe = b.read(32, "infe_type")?.to_be_bytes();
            let cfg = b.read(32, "codec_config_type")?.to_be_bytes();
            Some((infe, cfg))
        } else {
            None
        };
        let mut hdr_boxes = MiniHdrBoxes::default();
        let mut gain_map: Option<MiniGainMap> = None;
        let mut gainmap_flag = false;
        let mut tmap_icc_flag = false;
        if hdr {
            gainmap_flag = b.flag("gainmap_flag")?;
            if gainmap_flag {
                let same = b.flag("gainmap_dimension_same_as_main_item_flag")?;
                let (gw, gh) = if same {
                    (width, height)
                } else {
                    (
                        b.read(dim_bits, "gainmap_width_minus1")? + 1,
                        b.read(dim_bits, "gainmap_height_minus1")? + 1,
                    )
                };
                let matrix_coefficients = b.read(8, "gainmap_matrix_coefficients")? as u8;
                let gfull = b.flag("gainmap_full_range_flag")?;
                let gsub = b.read(2, "gainmap_chroma_subsampling")? as u8;
                let gchroma = read_chroma(&mut b, gsub)?;
                let gfloat = b.flag("gainmap_float_flag")?;
                let gformat = read_format(&mut b, gfloat, "gain map bit depth")?;
                tmap_icc_flag = b.flag("tmap_icc_flag")?;
                let tmap_explicit = b.flag("tmap_explicit_cicp_flag")?;
                let tmap_cicp = if tmap_explicit {
                    Some((
                        b.read(8, "tmap_colour_primaries")? as u8,
                        b.read(8, "tmap_transfer_characteristics")? as u8,
                        b.read(8, "tmap_matrix_coefficients")? as u8,
                        b.flag("tmap_full_range_flag")?,
                    ))
                } else {
                    None
                };
                gain_map = Some(MiniGainMap {
                    width: gw,
                    height: gh,
                    matrix_coefficients,
                    full_range: gfull,
                    chroma: gchroma,
                    format: gformat,
                    tmap_cicp,
                    tmap_icc: None,
                    tmap_hdr: MiniHdrBoxes::default(),
                    metadata: Vec::new(),
                    codec_config: None,
                    data: Vec::new(),
                });
            }
            hdr_boxes = read_hdr_boxes(&mut b, "main hdr flags")?;
            if let Some(g) = gain_map.as_mut() {
                g.tmap_hdr = read_hdr_boxes(&mut b, "tmap hdr flags")?;
            }
        }
        // Chunk sizes.
        let large_metadata = if icc_flag || exif_flag || xmp_flag || gainmap_flag {
            b.flag("large_metadata_flag")?
        } else {
            false
        };
        let large_codec_config = b.flag("large_codec_config_flag")?;
        let large_item_data = b.flag("large_item_data_flag")?;
        let md_bits = if large_metadata { 20 } else { 10 };
        let cc_bits = if large_codec_config { 12 } else { 3 };
        let id_bits = if large_item_data { 28 } else { 15 };
        let icc_size = if icc_flag {
            b.read(md_bits, "icc_data_size_minus1")? as usize + 1
        } else {
            0
        };
        let tmap_icc_size = if gainmap_flag && tmap_icc_flag {
            b.read(md_bits, "tmap_icc_data_size_minus1")? as usize + 1
        } else {
            0
        };
        let (gm_meta_size, gm_data_size) = if gainmap_flag {
            (
                b.read(md_bits, "gainmap_metadata_size")? as usize,
                b.read(id_bits, "gainmap_item_data_size")? as usize,
            )
        } else {
            (0, 0)
        };
        let gm_cc_size = if gainmap_flag && gm_data_size > 0 {
            b.read(cc_bits, "gainmap_item_codec_config_size")? as usize
        } else {
            0
        };
        let main_cc_size = b.read(cc_bits, "main_item_codec_config_size")? as usize;
        let main_data_size = b.read(id_bits, "main_item_data_size_minus1")? as usize + 1;
        let alpha_data_size = if alpha {
            b.read(id_bits, "alpha_item_data_size")? as usize
        } else {
            0
        };
        let alpha_cc_size = if alpha && alpha_data_size > 0 {
            b.read(cc_bits, "alpha_item_codec_config_size")? as usize
        } else {
            0
        };
        let exif_xmp_compressed = if exif_flag || xmp_flag {
            b.flag("exif_xmp_compressed_flag")?
        } else {
            false
        };
        let exif_size = if exif_flag {
            b.read(md_bits, "exif_data_size_minus1")? as usize + 1
        } else {
            0
        };
        let xmp_size = if xmp_flag {
            b.read(md_bits, "xmp_data_size_minus1")? as usize + 1
        } else {
            0
        };
        // trailing_bits(): byte alignment, shall be 0.
        let pad = (8 - b.pos % 8) % 8;
        if b.read(pad as u32, "trailing_bits")? != 0 {
            return Err(HeifError::invalid("mini: trailing_bits are not zero"));
        }
        // Chunks, in the O.3.2 order.
        let main_codec_config = b.bytes(main_cc_size, "main_item_codec_config")?;
        let alpha_codec_config = if alpha && alpha_data_size > 0 && alpha_cc_size > 0 {
            Some(b.bytes(alpha_cc_size, "alpha_item_codec_config")?)
        } else {
            None
        };
        if let Some(g) = gain_map.as_mut() {
            if gm_data_size > 0 && gm_cc_size > 0 {
                g.codec_config = Some(b.bytes(gm_cc_size, "gainmap_item_codec_config")?);
            }
        }
        let icc = if icc_flag {
            Some(b.bytes(icc_size, "icc_data")?)
        } else {
            None
        };
        if let Some(g) = gain_map.as_mut() {
            if tmap_icc_flag {
                g.tmap_icc = Some(b.bytes(tmap_icc_size, "tmap_icc_data")?);
            }
            g.metadata = b.bytes(gm_meta_size, "gainmap_metadata")?;
        }
        let alpha_data = b.bytes(alpha_data_size, "alpha_item_data")?;
        if let Some(g) = gain_map.as_mut() {
            g.data = b.bytes(gm_data_size, "gainmap_item_data")?;
        }
        let main_data = b.bytes(main_data_size, "main_item_data")?;
        let exif = if exif_flag {
            Some(b.bytes(exif_size, "exif_data")?)
        } else {
            None
        };
        let xmp = if xmp_flag {
            Some(b.bytes(xmp_size, "xmp_data")?)
        } else {
            None
        };
        Ok(Self {
            explicit_codec_types,
            format,
            full_range,
            chroma,
            orientation,
            width,
            height,
            explicit_cicp,
            icc,
            alpha,
            alpha_premultiplied,
            hdr,
            hdr_boxes,
            gain_map,
            main_codec_config,
            main_data,
            alpha_codec_config,
            alpha_data,
            exif_xmp_compressed,
            exif,
            xmp,
        })
    }

    /// The main image CICP `(primaries, transfer, matrix)`: explicit,
    /// else Table O.3 (primaries / transfer 1 / 13 — 2 / 2 with an ICC
    /// profile — and matrix 2 for 4:0:0, 6 otherwise).
    pub fn cicp(&self) -> (u8, u8, u8) {
        self.explicit_cicp.unwrap_or_else(|| {
            let (p, t) = if self.icc.is_some() { (2, 2) } else { (1, 13) };
            (p, t, if self.chroma.subsampling == 0 { 2 } else { 6 })
        })
    }

    /// The coded item type and codec configuration property type:
    /// explicit, else inferred from the codec brand in
    /// `ftyp.minor_version` (L.4.3: `vvi3` → `vvc1` / `vvcC`).
    pub fn codec_types(&self, minor_version: u32) -> Result<(FourCc, FourCc)> {
        if let Some(t) = self.explicit_codec_types {
            return Ok(t);
        }
        match &minor_version.to_be_bytes() {
            b"vvi3" => Ok((*b"vvc1", *b"vvcC")),
            other => Err(HeifError::unsupported(format!(
                "mini without explicit codec types under minor_version '{}' (no codec brand this crate knows infers them)",
                crate::boxes::fourcc_str(other)
            ))),
        }
    }

    fn has_native_alpha(&self) -> bool {
        self.alpha && self.alpha_data.is_empty()
    }

    /// The O.2.1.2 equivalent `ftyp`: `mif1` added, the equivalent
    /// major brand for a codec brand in `minor_version` (`vvi3` →
    /// `vvic`), `tmap` when a gain map is present.
    pub fn equivalent_file_type(&self, original: &FileType) -> FileType {
        let mut ft = original.clone();
        let minor = original.minor_version.to_be_bytes();
        if ft.major_brand == BRAND_MIF3 && original.minor_version != 0 {
            let equivalent = match &minor {
                b"vvi3" => *b"vvic",
                other => *other,
            };
            if !ft.compatible_brands.contains(&BRAND_MIF3) {
                ft.compatible_brands.push(BRAND_MIF3);
            }
            ft.major_brand = equivalent;
        }
        let mut add = |b: FourCc| {
            if !ft.compatible_brands.contains(&b) {
                ft.compatible_brands.push(b);
            }
        };
        add(crate::ftyp::BRAND_MIF1);
        if self.gain_map.is_some() {
            add(crate::ftyp::BRAND_TMAP);
        }
        ft.minor_version = 0;
        ft
    }

    /// Rebuild the equivalent file of O.4 — `ftyp` (O.2.1.2), `meta`
    /// (O.4.1–O.4.9), `mdat` (O.4.10) — so the regular readers apply.
    pub fn equivalent_file(&self, original: &FileType) -> Result<Vec<u8>> {
        if self.has_native_alpha() {
            return Err(HeifError::unsupported(
                "mini: alpha coded natively in the main item expands to an AlphaInformationProperty (O.4.6 entry 30), which ISO/IEC 23008-12 Amd 2:2026 does not define",
            ));
        }
        let (infe_type, cc_type) = self.codec_types(original.minor_version)?;
        let ftyp = self.equivalent_file_type(original).to_box();
        let gm = self.gain_map.as_ref();
        let tmap_data: Vec<u8> = match gm {
            Some(g) => {
                let mut v = vec![0u8];
                v.extend_from_slice(&g.metadata);
                v
            }
            None => Vec::new(),
        };
        let gm_data: &[u8] = gm.map(|g| g.data.as_slice()).unwrap_or(&[]);
        let has_alpha_item = !self.alpha_data.is_empty();
        let has_gm_item = !gm_data.is_empty();
        let exif_type: FourCc = if self.exif_xmp_compressed {
            *b"dExf"
        } else {
            *b"Exif"
        };
        // ── iinf
        let infe = |id: u32, hidden: bool, ty: &FourCc, mime: Option<(&str, &str)>| {
            let mut b = (id as u16).to_be_bytes().to_vec();
            b.extend_from_slice(&0u16.to_be_bytes()); // item_protection_index
            b.extend_from_slice(ty);
            b.push(0); // item_name ""
            if let Some((ct, ce)) = mime {
                b.extend_from_slice(ct.as_bytes());
                b.push(0);
                b.extend_from_slice(ce.as_bytes());
                b.push(0);
            }
            full_boxed(b"infe", 2, hidden as u32, &b)
        };
        let mut entries: Vec<Vec<u8>> = vec![infe(item_id::MAIN, false, &infe_type, None)];
        if has_alpha_item {
            entries.push(infe(item_id::ALPHA, true, &infe_type, None));
        }
        if gm.is_some() {
            entries.push(infe(item_id::TMAP, false, b"tmap", None));
        }
        if has_gm_item {
            entries.push(infe(item_id::GAIN_MAP, true, &infe_type, None));
        }
        if self.exif.is_some() {
            entries.push(infe(item_id::EXIF, true, &exif_type, None));
        }
        if self.xmp.is_some() {
            let ce = if self.exif_xmp_compressed {
                "deflate"
            } else {
                ""
            };
            entries.push(infe(
                item_id::XMP,
                true,
                b"mime",
                Some(("application/rdf+xml", ce)),
            ));
        }
        let mut iinf_body = (entries.len() as u16).to_be_bytes().to_vec();
        for e in &entries {
            iinf_body.extend_from_slice(e);
        }
        let iinf = full_boxed(b"iinf", 0, 0, &iinf_body);
        // ── iref
        let mut iref_body = Vec::new();
        let mut sitr = |ty: &FourCc, from: u32, to: &[u32]| {
            let mut b = (from as u16).to_be_bytes().to_vec();
            b.extend_from_slice(&(to.len() as u16).to_be_bytes());
            for t in to {
                b.extend_from_slice(&(*t as u16).to_be_bytes());
            }
            iref_body.extend(boxed(ty, &b));
        };
        if has_alpha_item {
            sitr(b"auxl", item_id::ALPHA, &[item_id::MAIN]);
            if self.alpha_premultiplied {
                sitr(b"prem", item_id::MAIN, &[item_id::ALPHA]);
            }
        }
        if gm.is_some() {
            if has_gm_item {
                sitr(b"dimg", item_id::TMAP, &[item_id::MAIN, item_id::GAIN_MAP]);
            } else {
                sitr(b"dimg", item_id::TMAP, &[item_id::MAIN]);
            }
        }
        if self.exif.is_some() {
            sitr(b"cdsc", item_id::EXIF, &[item_id::MAIN]);
        }
        if self.xmp.is_some() {
            sitr(b"cdsc", item_id::XMP, &[item_id::MAIN]);
        }
        let iref = if iref_body.is_empty() {
            Vec::new()
        } else {
            full_boxed(b"iref", 0, 0, &iref_body)
        };
        // ── grpl (O.4.5)
        let grpl = if gm.is_some() {
            let mut b = item_id::ALTR_GROUP.to_be_bytes().to_vec();
            b.extend_from_slice(&2u32.to_be_bytes());
            b.extend_from_slice(&item_id::TMAP.to_be_bytes());
            b.extend_from_slice(&item_id::MAIN.to_be_bytes());
            boxed(b"grpl", &full_boxed(b"altr", 0, 0, &b))
        } else {
            Vec::new()
        };
        // ── ipco: 32 slots (O.4.6), `free` where the condition fails.
        let (p, t, m) = self.cicp();
        let nclx = |p: u8, t: u8, m: u8, full: bool| {
            let mut b = b"nclx".to_vec();
            b.extend_from_slice(&(p as u16).to_be_bytes());
            b.extend_from_slice(&(t as u16).to_be_bytes());
            b.extend_from_slice(&(m as u16).to_be_bytes());
            b.push(if full { 0x80 } else { 0 });
            boxed(b"colr", &b)
        };
        let prof = |icc: &[u8]| {
            let mut b = b"prof".to_vec();
            b.extend_from_slice(icc);
            boxed(b"colr", &b)
        };
        let ispe = |w: u32, h: u32| {
            let mut b = w.to_be_bytes().to_vec();
            b.extend_from_slice(&h.to_be_bytes());
            full_boxed(b"ispe", 0, 0, &b)
        };
        let hdr = |h: &MiniHdrBoxes| -> [Option<Vec<u8>>; 6] {
            [
                h.clli.as_ref().map(|v| boxed(b"clli", v)),
                h.mdcv.as_ref().map(|v| boxed(b"mdcv", v)),
                h.cclv.as_ref().map(|v| boxed(b"cclv", v)),
                h.amve.as_ref().map(|v| boxed(b"amve", v)),
                h.reve.as_ref().map(|v| full_boxed(b"reve", 0, 0, v)),
                h.ndwt.as_ref().map(|v| full_boxed(b"ndwt", 0, 0, v)),
            ]
        };
        let main_components = if self.chroma.subsampling == 0 { 1 } else { 3 };
        let orient = self.orientation - 1;
        let mut slots: Vec<Option<Vec<u8>>> = vec![None; 32];
        if !self.main_codec_config.is_empty() {
            slots[0] = Some(boxed(&cc_type, &self.main_codec_config));
        }
        slots[1] = Some(ispe(self.width, self.height));
        slots[2] = Some(pixi_box(main_components, false, self.chroma, self.format));
        slots[3] = Some(nclx(p, t, m, self.full_range));
        if let Some(icc) = &self.icc {
            slots[4] = Some(prof(icc));
        }
        if has_alpha_item {
            let cfg = self
                .alpha_codec_config
                .as_ref()
                .unwrap_or(&self.main_codec_config);
            if !cfg.is_empty() {
                slots[5] = Some(boxed(&cc_type, cfg));
            }
            let mut aux = crate::props::AUX_URN_ALPHA.as_bytes().to_vec();
            aux.push(0);
            slots[6] = Some(full_boxed(b"auxC", 0, 0, &aux));
            slots[7] = Some(pixi_box(
                0,
                true,
                MiniChroma {
                    subsampling: 0,
                    horizontally_centered: false,
                    vertically_centered: false,
                },
                self.format,
            ));
        }
        let irot_angle = match orient {
            2 => Some(2u8),
            4 => Some(1),
            5 => Some(3),
            6 => Some(1),
            7 => Some(1),
            _ => None,
        };
        if let Some(a) = irot_angle {
            slots[8] = Some(boxed(b"irot", &[a]));
        }
        let imir_axis = match orient {
            1 => Some(1u8),
            3 => Some(0),
            4 => Some(0),
            6 => Some(1),
            _ => None,
        };
        if let Some(a) = imir_axis {
            slots[9] = Some(boxed(b"imir", &[a]));
        }
        for (i, b) in hdr(&self.hdr_boxes).into_iter().enumerate() {
            slots[10 + i] = b;
        }
        if let Some(g) = gm {
            if has_gm_item {
                let cfg = g.codec_config.as_ref().unwrap_or(&self.main_codec_config);
                if !cfg.is_empty() {
                    slots[16] = Some(boxed(&cc_type, cfg));
                }
                slots[17] = Some(ispe(g.width, g.height));
                let gcomp = if g.chroma.subsampling == 0 { 1 } else { 3 };
                slots[18] = Some(pixi_box(gcomp, false, g.chroma, g.format));
                slots[19] = Some(nclx(2, 2, g.matrix_coefficients, g.full_range));
            }
            slots[20] = Some(if orient <= 3 {
                ispe(self.width, self.height)
            } else {
                ispe(self.height, self.width)
            });
            if g.tmap_cicp.is_some() || g.tmap_icc.is_none() {
                let (tp, tt, tm, tf) = g.tmap_cicp.unwrap_or((1, 13, 6, true));
                slots[21] = Some(nclx(tp, tt, tm, tf));
            }
            if let Some(icc) = &g.tmap_icc {
                slots[22] = Some(prof(icc));
            }
            for (i, b) in hdr(&g.tmap_hdr).into_iter().enumerate() {
                slots[23 + i] = b;
            }
        }
        let mut ipco_body = Vec::new();
        for s in &slots {
            match s {
                Some(b) => ipco_body.extend_from_slice(b),
                None => ipco_body.extend(boxed(b"free", &[])),
            }
        }
        let ipco = boxed(b"ipco", &ipco_body);
        // ── ipma (O.4.6 association order), free slots dropped.
        let mut rows: Vec<(u32, Vec<(usize, bool)>)> = Vec::new();
        let mut main = vec![(1, true), (2, false), (3, false), (4, true), (5, true)];
        if self.hdr {
            main.extend([11, 12, 13, 14, 15, 16].map(|i| (i, false)));
        }
        main.extend([(9, true), (10, true)]);
        rows.push((item_id::MAIN, main));
        if has_alpha_item {
            rows.push((
                item_id::ALPHA,
                vec![
                    (6, true),
                    (2, false),
                    (7, true),
                    (8, false),
                    (9, true),
                    (10, true),
                ],
            ));
        }
        if gm.is_some() {
            let mut r = vec![(21, false), (22, true), (23, true)];
            r.extend([24, 25, 26, 27, 28, 29].map(|i| (i, false)));
            rows.push((item_id::TMAP, r));
        }
        if has_gm_item {
            rows.push((
                item_id::GAIN_MAP,
                vec![
                    (17, true),
                    (18, false),
                    (19, false),
                    (20, true),
                    (9, true),
                    (10, true),
                ],
            ));
        }
        let mut ipma_body = (rows.len() as u32).to_be_bytes().to_vec();
        for (id, assoc) in &rows {
            let kept: Vec<_> = assoc
                .iter()
                .filter(|(i, _)| slots[i - 1].is_some())
                .collect();
            ipma_body.extend_from_slice(&(*id as u16).to_be_bytes());
            ipma_body.push(kept.len() as u8);
            for (i, ess) in kept {
                ipma_body.push(((*ess as u8) << 7) | *i as u8);
            }
        }
        let ipma = full_boxed(b"ipma", 0, 0, &ipma_body);
        let mut iprp_body = ipco;
        iprp_body.extend(ipma);
        let iprp = boxed(b"iprp", &iprp_body);
        // ── mdat payload order (O.4.10) and iloc (O.4.9).
        let chunks: [(u32, &[u8]); 6] = [
            (item_id::ALPHA, &self.alpha_data),
            (item_id::TMAP, &tmap_data),
            (item_id::GAIN_MAP, gm_data),
            (item_id::MAIN, &self.main_data),
            (item_id::EXIF, self.exif.as_deref().unwrap_or(&[])),
            (item_id::XMP, self.xmp.as_deref().unwrap_or(&[])),
        ];
        let mdat_len: usize = chunks.iter().map(|(_, c)| c.len()).sum();
        if mdat_len > MAX_EXPANDED_BYTES {
            return Err(HeifError::exhausted("mini: expanded mdat too large"));
        }
        let wide = mdat_len as u64 + 4096 > u32::MAX as u64 / 2;
        let build_meta = |mdat_payload_offset: u64| -> Vec<u8> {
            // iloc v1: offset_size / length_size 4 (8 when large),
            // base_offset_size 0, index_size 0.
            let sz: u8 = if wide { 8 } else { 4 };
            let mut b = vec![(sz << 4) | sz, 0];
            let present: Vec<(u32, u64, u64)> = {
                let mut off = mdat_payload_offset;
                let mut v = Vec::new();
                for (id, c) in &chunks {
                    if !c.is_empty() {
                        v.push((*id, off, c.len() as u64));
                    }
                    off += c.len() as u64;
                }
                v.sort_by_key(|(id, _, _)| *id);
                v
            };
            b.extend_from_slice(&(present.len() as u16).to_be_bytes());
            for (id, off, len) in present {
                b.extend_from_slice(&(id as u16).to_be_bytes());
                b.extend_from_slice(&0u16.to_be_bytes()); // construction_method 0
                b.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
                b.extend_from_slice(&1u16.to_be_bytes()); // extent_count
                if wide {
                    b.extend_from_slice(&off.to_be_bytes());
                    b.extend_from_slice(&len.to_be_bytes());
                } else {
                    b.extend_from_slice(&(off as u32).to_be_bytes());
                    b.extend_from_slice(&(len as u32).to_be_bytes());
                }
            }
            let iloc = full_boxed(b"iloc", 1, 0, &b);
            let mut hdlr = vec![0u8; 4];
            hdlr.extend_from_slice(b"pict");
            hdlr.extend_from_slice(&[0u8; 12]);
            hdlr.push(0);
            let mut body = full_boxed(b"hdlr", 0, 0, &hdlr);
            body.extend(full_boxed(
                b"pitm",
                0,
                0,
                &(item_id::MAIN as u16).to_be_bytes(),
            ));
            body.extend_from_slice(&iinf);
            body.extend_from_slice(&iref);
            body.extend_from_slice(&iprp);
            body.extend_from_slice(&grpl);
            body.extend(iloc);
            full_boxed(b"meta", 0, 0, &body)
        };
        let probe = build_meta(0);
        let mdat_header = if mdat_len as u64 + 8 > u32::MAX as u64 {
            16
        } else {
            8
        };
        let mdat_payload_offset = (ftyp.len() + probe.len() + mdat_header) as u64;
        let meta = build_meta(mdat_payload_offset);
        debug_assert_eq!(meta.len(), probe.len());
        let mut out = Vec::with_capacity(ftyp.len() + meta.len() + mdat_header + mdat_len);
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&meta);
        if mdat_header == 16 {
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(b"mdat");
            out.extend_from_slice(&(mdat_len as u64 + 16).to_be_bytes());
        } else {
            out.extend_from_slice(&(mdat_len as u32 + 8).to_be_bytes());
            out.extend_from_slice(b"mdat");
        }
        for (_, c) in &chunks {
            out.extend_from_slice(c);
        }
        Ok(out)
    }

    /// Serialise as a `mini` box (the writer direction). Chooses the
    /// compact field widths when the values fit; refuses values the
    /// syntax cannot carry (dimensions above 32768, chunks above the
    /// 28-bit / 20-bit / 12-bit size fields, native alpha).
    pub fn to_box(&self) -> Result<Vec<u8>> {
        let mut w = BitWriter::default();
        let gm = self.gain_map.as_ref();
        let icc_flag = self.icc.is_some();
        let exif_flag = self.exif.is_some();
        let xmp_flag = self.xmp.is_some();
        let hdr = self.hdr || gm.is_some() || !self.hdr_boxes.is_empty();
        w.put(2, 0); // version
        w.flag(self.explicit_codec_types.is_some());
        w.flag(self.format.is_float());
        w.flag(self.full_range);
        w.flag(self.alpha);
        w.flag(self.explicit_cicp.is_some());
        w.flag(hdr);
        w.flag(icc_flag);
        w.flag(exif_flag);
        w.flag(xmp_flag);
        if self.chroma.subsampling > 3 {
            return Err(HeifError::invalid("mini: chroma_subsampling > 3"));
        }
        w.put(2, self.chroma.subsampling as u32);
        if !(1..=8).contains(&self.orientation) {
            return Err(HeifError::invalid("mini: orientation outside 1..=8"));
        }
        w.put(3, self.orientation as u32 - 1);
        let max_dim = [
            self.width,
            self.height,
            gm.map(|g| g.width).unwrap_or(1),
            gm.map(|g| g.height).unwrap_or(1),
        ]
        .into_iter()
        .max()
        .unwrap_or(1);
        if max_dim == 0 || max_dim > 1 << 15 || self.width == 0 || self.height == 0 {
            return Err(HeifError::invalid(format!(
                "mini: dimensions {}x{} outside 1..=32768",
                self.width, self.height
            )));
        }
        let large_dim = max_dim > 128;
        let dim_bits = if large_dim { 15 } else { 7 };
        w.flag(large_dim);
        w.put(dim_bits, self.width - 1);
        w.put(dim_bits, self.height - 1);
        write_chroma_bits(&mut w, self.chroma);
        write_format(&mut w, self.format)?;
        if self.alpha {
            w.flag(self.alpha_premultiplied);
        }
        if let Some((p, t, m)) = self.explicit_cicp {
            w.put(8, p as u32);
            w.put(8, t as u32);
            w.put(8, m as u32);
        }
        if let Some((i, c)) = self.explicit_codec_types {
            w.put(32, u32::from_be_bytes(i));
            w.put(32, u32::from_be_bytes(c));
        }
        if hdr {
            w.flag(gm.is_some());
            if let Some(g) = gm {
                let same = g.width == self.width && g.height == self.height;
                w.flag(same);
                if !same {
                    if g.width == 0 || g.height == 0 {
                        return Err(HeifError::invalid("mini: empty gain map"));
                    }
                    w.put(dim_bits, g.width - 1);
                    w.put(dim_bits, g.height - 1);
                }
                w.put(8, g.matrix_coefficients as u32);
                w.flag(g.full_range);
                if g.chroma.subsampling > 3 {
                    return Err(HeifError::invalid("mini: gain map chroma_subsampling > 3"));
                }
                w.put(2, g.chroma.subsampling as u32);
                write_chroma_bits(&mut w, g.chroma);
                w.flag(g.format.is_float());
                write_format(&mut w, g.format)?;
                w.flag(g.tmap_icc.is_some());
                w.flag(g.tmap_cicp.is_some());
                if let Some((p, t, m, f)) = g.tmap_cicp {
                    w.put(8, p as u32);
                    w.put(8, t as u32);
                    w.put(8, m as u32);
                    w.flag(f);
                }
            }
            write_hdr_boxes(&mut w, &self.hdr_boxes)?;
            if let Some(g) = gm {
                write_hdr_boxes(&mut w, &g.tmap_hdr)?;
            }
        }
        // Chunk sizes.
        let md_sizes = [
            self.icc.as_ref().map(|v| v.len().saturating_sub(1)),
            gm.and_then(|g| g.tmap_icc.as_ref())
                .map(|v| v.len().saturating_sub(1)),
            gm.map(|g| g.metadata.len()),
            self.exif.as_ref().map(|v| v.len().saturating_sub(1)),
            self.xmp.as_ref().map(|v| v.len().saturating_sub(1)),
        ];
        for (v, what) in [(&self.icc, "icc"), (&self.exif, "exif"), (&self.xmp, "xmp")] {
            if v.as_ref().map(|b| b.is_empty()).unwrap_or(false) {
                return Err(HeifError::invalid(format!("mini: empty {what} chunk")));
            }
        }
        let alpha_cc = if self.alpha && !self.alpha_data.is_empty() {
            Some(
                self.alpha_codec_config
                    .as_ref()
                    .map(|c| c.len())
                    .unwrap_or(0),
            )
        } else {
            None
        };
        let gm_cc = gm
            .filter(|g| !g.data.is_empty())
            .map(|g| g.codec_config.as_ref().map(|c| c.len()).unwrap_or(0));
        let cc_sizes = [Some(self.main_codec_config.len()), alpha_cc, gm_cc];
        if self.main_data.is_empty() {
            return Err(HeifError::invalid("mini: empty main item data"));
        }
        let id_sizes = [
            Some(self.main_data.len() - 1),
            self.alpha.then_some(self.alpha_data.len()),
            gm.map(|g| g.data.len()),
        ];
        let fits = |v: &[Option<usize>], bits: u32| v.iter().flatten().all(|x| *x < 1 << bits);
        let large_md = !fits(&md_sizes, 10);
        let large_cc = !fits(&cc_sizes, 3);
        let large_id = !fits(&id_sizes, 15);
        if !fits(&md_sizes, 20) || !fits(&cc_sizes, 12) || !fits(&id_sizes, 28) {
            return Err(HeifError::invalid(
                "mini: a chunk exceeds its size field (metadata 2^20, codec configuration 2^12, item data 2^28 bytes)",
            ));
        }
        let md_bits = if large_md { 20 } else { 10 };
        let cc_bits = if large_cc { 12 } else { 3 };
        let id_bits = if large_id { 28 } else { 15 };
        if icc_flag || exif_flag || xmp_flag || gm.is_some() {
            w.flag(large_md);
        }
        w.flag(large_cc);
        w.flag(large_id);
        if let Some(v) = md_sizes[0] {
            w.put(md_bits, v as u32);
        }
        if let Some(v) = md_sizes[1] {
            w.put(md_bits, v as u32);
        }
        if let Some(g) = gm {
            w.put(md_bits, g.metadata.len() as u32);
            w.put(id_bits, g.data.len() as u32);
            if let Some(c) = gm_cc {
                w.put(cc_bits, c as u32);
            }
        }
        w.put(cc_bits, self.main_codec_config.len() as u32);
        w.put(id_bits, (self.main_data.len() - 1) as u32);
        if self.alpha {
            w.put(id_bits, self.alpha_data.len() as u32);
            if let Some(c) = alpha_cc {
                w.put(cc_bits, c as u32);
            }
        }
        if exif_flag || xmp_flag {
            w.flag(self.exif_xmp_compressed);
        }
        if let Some(v) = md_sizes[3] {
            w.put(md_bits, v as u32);
        }
        if let Some(v) = md_sizes[4] {
            w.put(md_bits, v as u32);
        }
        let pad = (8 - w.bits % 8) % 8;
        w.put(pad as u32, 0);
        w.bytes(&self.main_codec_config);
        if let (Some(_), Some(c)) = (alpha_cc, &self.alpha_codec_config) {
            w.bytes(c);
        }
        if let (Some(_), Some(g)) = (gm_cc, gm) {
            if let Some(c) = &g.codec_config {
                w.bytes(c);
            }
        }
        if let Some(v) = &self.icc {
            w.bytes(v);
        }
        if let Some(g) = gm {
            if let Some(v) = &g.tmap_icc {
                w.bytes(v);
            }
            w.bytes(&g.metadata);
        }
        w.bytes(&self.alpha_data);
        if let Some(g) = gm {
            w.bytes(&g.data);
        }
        w.bytes(&self.main_data);
        if let Some(v) = &self.exif {
            w.bytes(v);
        }
        if let Some(v) = &self.xmp {
            w.bytes(v);
        }
        Ok(boxed(b"mini", &w.out))
    }

    /// A complete `mif3` file: `ftyp` (major `mif3`, minor 0 — the
    /// codec types are explicit — compatible `mif3`) + the `mini` box.
    pub fn to_file(&self) -> Result<Vec<u8>> {
        self.to_file_with_minor(0)
    }

    /// A complete `mif3` file whose `ftyp.minor_version` carries a
    /// brand: O.2.1.1 allows "a brand to which the file conforms after
    /// the equivalent MetaBox and MediaDataBox have been transformed"
    /// (`heic` for an HEVC item, `avif` for an AV1 one — the equivalent
    /// major brand, O.2.1.2), or a codec brand that infers the codec
    /// types (`vvi3`). Explicit codec types are required unless the
    /// brand infers them (O.3.3).
    pub fn to_file_with_minor(&self, minor_version: u32) -> Result<Vec<u8>> {
        if self.explicit_codec_types.is_none() {
            self.codec_types(minor_version).map_err(|_| {
                HeifError::invalid(
                    "mini: explicit codec types are required unless minor_version names a codec brand that infers them (O.3.3)",
                )
            })?;
        }
        let mut out = FileType {
            box_type: *b"ftyp",
            major_brand: BRAND_MIF3,
            minor_version,
            compatible_brands: vec![BRAND_MIF3],
        }
        .to_box();
        out.extend(self.to_box()?);
        Ok(out)
    }
}

/// A `pixi` reconstructed per O.4.7.1 (`px_flags` = 1).
fn pixi_box(main_components: u8, alpha: bool, chroma: MiniChroma, format: SampleFormat) -> Vec<u8> {
    let n = main_components + alpha as u8;
    let bits = format.bits();
    let cf = format.is_float() as u8;
    let mut b = vec![n];
    b.extend(std::iter::repeat(bits).take(n as usize));
    // channel_idc(3) reserved(1) component_format(2) subsampling_flag(1)
    // channel_label_flag(1), then subsampling_type(4) location(4).
    let rec = |idc: u8, st: u8, sl: u8, out: &mut Vec<u8>| {
        out.push((idc << 5) | (cf << 2) | 0x02);
        out.push((st << 4) | sl);
    };
    if alpha {
        rec(5, 0, 0, &mut b);
    }
    if main_components > 0 {
        rec(2, 0, 0, &mut b);
    }
    if main_components > 1 {
        let st = match chroma.subsampling {
            1 => 2,
            2 => 1,
            _ => 0,
        };
        let sl = match (chroma.horizontally_centered, chroma.vertically_centered) {
            (true, true) => 1,
            (true, false) => 3,
            (false, true) => 0,
            (false, false) => 2,
        };
        rec(3, st, sl, &mut b);
        rec(4, st, sl, &mut b);
    }
    full_boxed(b"pixi", 0, 1, &b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `av1C` record: 8-bit 4:2:0 (subsampling_x = y = 1).
    const AV1C: [u8; 4] = [0x81, 0x00, 0x0c, 0x00];
    /// A monochrome `av1C` record.
    const AV1C_MONO: [u8; 4] = [0x81, 0x00, 0x1c, 0x00];

    fn sample() -> MinimizedImage {
        MinimizedImage {
            explicit_codec_types: Some((*b"av01", *b"av1C")),
            format: SampleFormat::Integer { bits: 8 },
            full_range: true,
            chroma: MiniChroma {
                subsampling: 1,
                horizontally_centered: false,
                vertically_centered: true,
            },
            orientation: 1,
            width: 64,
            height: 48,
            explicit_cicp: None,
            icc: None,
            alpha: false,
            alpha_premultiplied: false,
            hdr: false,
            hdr_boxes: MiniHdrBoxes::default(),
            gain_map: None,
            main_codec_config: AV1C.to_vec(),
            main_data: vec![9; 40],
            alpha_codec_config: None,
            alpha_data: Vec::new(),
            exif_xmp_compressed: false,
            exif: None,
            xmp: None,
        }
    }

    fn round_trip(m: &MinimizedImage) {
        let b = m.to_box().unwrap();
        let back = MinimizedImage::parse(&b[8..]).unwrap();
        assert_eq!(&back, m);
    }

    #[test]
    fn minimal_box_round_trips_and_expands() {
        let m = sample();
        round_trip(&m);
        let file = m.to_file().unwrap();
        // ftyp (20) + mini: header 8 + 2 flag bytes + ...
        assert_eq!(&file[4..8], b"ftyp");
        let ft = FileType::parse(*b"ftyp", &file[8..20]).unwrap();
        let eq = m.equivalent_file(&ft).unwrap();
        let f = crate::HeifFile::parse(&eq).unwrap();
        let meta = f.meta().unwrap();
        assert_eq!(meta.primary_item_id, Some(1));
        assert_eq!(meta.properties.len(), 32);
        assert_eq!(f.item_data(1).unwrap().as_ref(), &m.main_data[..]);
        let props = crate::props::ItemProperties::resolve(meta, 1).unwrap();
        assert_eq!(props.ispe().map(|i| (i.width, i.height)), Some((64, 48)));
        assert_eq!(
            props.nclx(),
            Some(&crate::props::Colr::Nclx {
                primaries: 1,
                transfer: 13,
                matrix: 6,
                full_range: true
            })
        );
        let ch = props.pixi_channels().unwrap();
        assert_eq!(ch.len(), 3);
        assert_eq!(ch[1].subsampling, Some((2, 0)));
        assert!(f.file_type.has_brand(b"mif1"));
    }

    #[test]
    fn full_featured_box_round_trips_and_expands() {
        let mut m = sample();
        m.width = 300;
        m.height = 200;
        m.orientation = 6;
        m.format = SampleFormat::Integer { bits: 10 };
        m.explicit_cicp = Some((9, 16, 9));
        m.icc = Some(vec![7; 20]);
        m.alpha = true;
        m.alpha_premultiplied = true;
        m.alpha_data = vec![5; 30];
        m.alpha_codec_config = Some(AV1C_MONO.to_vec());
        m.hdr = true;
        m.hdr_boxes.clli = Some(vec![0, 100, 0, 50]);
        m.hdr_boxes.cclv = Some(vec![
            0x30, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0, 6, 0, 0, 0,
            7,
        ]);
        m.hdr_boxes.ndwt = Some(vec![0, 0, 1, 0]);
        m.gain_map = Some(MiniGainMap {
            width: 150,
            height: 100,
            matrix_coefficients: 6,
            full_range: false,
            chroma: MiniChroma {
                subsampling: 0,
                horizontally_centered: false,
                vertically_centered: false,
            },
            format: SampleFormat::Integer { bits: 8 },
            tmap_cicp: Some((9, 16, 9, true)),
            tmap_icc: None,
            tmap_hdr: MiniHdrBoxes {
                reve: Some(vec![1; 20]),
                ..MiniHdrBoxes::default()
            },
            metadata: vec![0, 0, 0, 0, 0],
            codec_config: None,
            data: vec![3; 12],
        });
        m.exif = Some(vec![0, 0, 0, 0, b'I', b'I']);
        m.xmp = Some(b"<x/>".to_vec());
        round_trip(&m);
        let ft = FileType {
            box_type: *b"ftyp",
            major_brand: BRAND_MIF3,
            minor_version: 0,
            compatible_brands: vec![],
        };
        let eq = m.equivalent_file(&ft).unwrap();
        let f = crate::HeifFile::parse(&eq).unwrap();
        assert!(f.file_type.has_brand(b"tmap"));
        let meta = f.meta().unwrap();
        assert_eq!(meta.derivation_inputs(3), vec![1, 4]);
        assert_eq!(meta.auxiliaries_of(1), vec![2]);
        assert!(meta.is_premultiplied(1, 2));
        assert_eq!(meta.entity_groups[0].entity_ids, vec![3, 1]);
        assert_eq!(f.item_data(2).unwrap().as_ref(), &m.alpha_data[..]);
        assert_eq!(f.item_data(4).unwrap().as_ref(), &[3; 12][..]);
        assert_eq!(f.item_data(3).unwrap().as_ref(), &[0, 0, 0, 0, 0, 0][..]);
        assert_eq!(
            f.item_data(6).unwrap().as_ref(),
            &m.exif.clone().unwrap()[..]
        );
        assert_eq!(f.item_data(7).unwrap().as_ref(), b"<x/>");
        let p1 = crate::props::ItemProperties::resolve(meta, 1).unwrap();
        // Exif orientation 6 → irot angle 3, no mirror (O.4.6 slots 9 / 10).
        assert_eq!(p1.irot().map(|r| r.angle), Some(3));
        assert!(p1.imir().is_none());
        assert_eq!(p1.clli().map(|c| c.max_content_light_level), Some(100));
        assert_eq!(p1.ndwt().map(|n| n.diffuse_white_luminance), Some(256));
        assert_eq!(p1.icc_profile(), Some(&[7u8; 20][..]));
        let p3 = crate::props::ItemProperties::resolve(meta, 3).unwrap();
        // Orientation 6 (90° turn): the tmap ispe is transposed.
        assert_eq!(p3.ispe().map(|i| (i.width, i.height)), Some((200, 300)));
        assert!(p3.reve().is_some());
        let p4 = crate::props::ItemProperties::resolve(meta, 4).unwrap();
        assert_eq!(
            p4.nclx(),
            Some(&crate::props::Colr::Nclx {
                primaries: 2,
                transfer: 2,
                matrix: 6,
                full_range: false
            })
        );
        // The gain map shares the main codec configuration.
        assert_eq!(
            meta.property_of(4, b"av1C").map(|p| p.body.clone()),
            Some(AV1C.to_vec())
        );
        assert_eq!(
            meta.property_of(2, b"av1C").map(|p| p.body.clone()),
            Some(AV1C_MONO.to_vec())
        );
    }

    #[test]
    fn native_alpha_and_unknown_codec_brands_are_refused() {
        let mut m = sample();
        m.alpha = true;
        let ft = FileType {
            box_type: *b"ftyp",
            major_brand: BRAND_MIF3,
            minor_version: 0,
            compatible_brands: vec![],
        };
        assert!(matches!(
            m.equivalent_file(&ft),
            Err(HeifError::Unsupported(_))
        ));
        let mut n = sample();
        n.explicit_codec_types = None;
        assert!(n.equivalent_file(&ft).is_err());
        let vvc = FileType {
            minor_version: u32::from_be_bytes(*b"vvi3"),
            ..ft
        };
        let eq = n.equivalent_file(&vvc).unwrap();
        let f = crate::HeifFile::parse(&eq).unwrap();
        assert_eq!(f.file_type.major_brand, *b"vvic");
        assert_eq!(f.primary_item().unwrap().item_type, *b"vvc1");
    }

    #[test]
    fn truncations_are_errors() {
        let b = sample().to_box().unwrap();
        for cut in 8..b.len() {
            assert!(MinimizedImage::parse(&b[8..cut]).is_err(), "cut {cut}");
        }
    }
}
