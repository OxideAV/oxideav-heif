//! The workspace image-crate contract: the small vocabulary every
//! `oxideav-<format>` image crate exposes at its root.
//!
//! This module holds the **standalone** half — the types, [`probe`] and
//! [`info`], and the pixel conversions of [`HeifImage`] — which build
//! with `default-features = false`. HEIF is a container whose pixels
//! come from the HEVC / AV1 / AVC codec crates, and those are
//! framework-only, so [`decode()`](crate::decode()) / [`encode()`](crate::encode())
//! and their one-call `rgb8` / `rgba8` variants live behind the
//! default-on `registry` feature (see [`crate::api_registry`]). Standalone callers still get the container
//! model ([`HeifFile`]), the header ([`info`]) and composition over
//! planes they decoded themselves ([`compose`](crate::compose)).

use std::time::Duration;

use crate::derived::{build_primary_graph, ImageKind, ImageNode};
use crate::error::{HeifError, Result};
use crate::file::HeifFile;
use crate::image::{Chroma, HeifFrame, HeifPixelFormat, Plane};
use crate::layout::{entry_layout, predict_output};
use crate::meta::{ITEM_TYPE_DEXF, ITEM_TYPE_EXIF, ITEM_TYPE_TMAP};
use crate::props::Colr;
use crate::sequence::{parse_movie, Movie, Track};

/// `true` when `bytes` start with an `ftyp` box declaring a HEIF-family
/// brand (`heic`, `heix`, `mif1`, `msf1`, `miaf`, `avif`, `mif3`, …).
/// No allocation, no panic; `false` on short input and on files with
/// only generic ISOBMFF / QuickTime brands.
pub fn probe(bytes: &[u8]) -> bool {
    crate::ftyp::probe_score(bytes) > 0
}

/// The header of a HEIF file: the primary image's geometry and native
/// layout, the number of images, alpha / colour / metadata presence —
/// read from the container alone (no codec runs). Works standalone.
pub fn info(bytes: &[u8]) -> Result<ImageInfo> {
    let file = HeifFile::parse_borrowed(bytes)?;
    info_of(&file)
}

/// [`info`] over an already parsed file.
pub fn info_of<D: AsRef<[u8]>>(file: &HeifFile<D>) -> Result<ImageInfo> {
    let movie = parse_movie(file)?;
    let sequence_frames = sequence_frame_count(movie.as_ref());
    match build_primary_graph(file) {
        Ok(node) => {
            let (layout, (width, height)) = predict_output(&node)?;
            let colour_node = colour_node_of(&node);
            let (color, explicit) = match colour_node.properties.nclx() {
                Some(c) => (ColorInfo::from_colr(c), true),
                None => (ColorInfo::default(), false),
            };
            let format = PixelFormat::from_layout_promoting(layout, explicit && color.matrix == 0);
            let meta = file.meta()?;
            let has_gain_map = matches!(node.kind, ImageKind::ToneMap(_))
                || meta.items.iter().any(|it| {
                    it.item_type == ITEM_TYPE_TMAP
                        && meta.derivation_inputs(it.id).first() == Some(&node.item.id)
                });
            let items = meta.display_order().len().max(1) as u64;
            Ok(ImageInfo {
                width,
                height,
                format,
                frames: (items + sequence_frames).min(u32::MAX as u64) as u32,
                has_alpha: layout.has_alpha,
                color,
                has_icc: colour_node.properties.icc_profile().is_some(),
                has_exif: node
                    .metadata
                    .iter()
                    .any(|m| m.item_type == ITEM_TYPE_EXIF || m.item_type == ITEM_TYPE_DEXF),
                has_xmp: node.metadata.iter().any(|m| m.is_xmp()),
                primary_item_id: Some(node.item.id),
                has_gain_map,
            })
        }
        Err(e) => {
            // No decodable primary item: an image sequence (`msf1`) file
            // describes itself through its first visual track.
            let Some(mv) = movie.as_ref() else {
                return Err(e);
            };
            let Some(track) = sequence_tracks(mv).next() else {
                return Err(e);
            };
            let entry = track
                .primary_entry()
                .ok_or_else(|| HeifError::invalid("visual track without a sample entry"))?;
            let layout = entry_layout(entry).ok_or_else(|| {
                HeifError::unsupported("sample entry without a known decoder configuration")
            })?;
            let has_alpha = mv.alpha_track_of(track.track_id).is_some();
            let layout = if has_alpha {
                layout.with_alpha()
            } else {
                layout
            };
            let color = entry
                .colr
                .iter()
                .find(|c| matches!(c, Colr::Nclx { .. }))
                .map(ColorInfo::from_colr)
                .unwrap_or_default();
            Ok(ImageInfo {
                width: entry.width as u32,
                height: entry.height as u32,
                format: PixelFormat::from_layout_promoting(layout, false),
                frames: sequence_frames.min(u32::MAX as u64) as u32,
                has_alpha,
                color,
                has_icc: entry.colr.iter().any(Colr::is_icc),
                has_exif: false,
                has_xmp: false,
                primary_item_id: None,
                has_gain_map: false,
            })
        }
    }
}

/// The node whose colour information describes what a decode of
/// `node` yields: a `tmap` decoded to its base rendition carries the
/// base's (HEIF Amd 1 §6.6.2.4).
pub(crate) fn colour_node_of(node: &ImageNode) -> &ImageNode {
    match (&node.kind, node.inputs.first()) {
        (ImageKind::ToneMap(_), Some(base)) => base,
        _ => node,
    }
}

/// The visual tracks [`decode_all`](crate::decode_all) yields frames
/// from: image-sequence / video tracks that are not auxiliaries (an
/// alpha track is composed into its master's frames).
pub(crate) fn sequence_tracks(movie: &Movie) -> impl Iterator<Item = &Track> {
    movie
        .tracks
        .iter()
        .filter(|t| t.is_visual() && &t.handler != b"auxv" && t.aux_kind().is_none())
}

fn sequence_frame_count(movie: Option<&Movie>) -> u64 {
    movie
        .map(|mv| sequence_tracks(mv).map(|t| t.samples.len() as u64).sum())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Pixel format
// ---------------------------------------------------------------------

/// The native layout of a [`HeifImage`]. Variant names mirror
/// `oxideav_core::PixelFormat` one to one; only the layouts this crate
/// decodes to or encodes from are present. Planar variants hold one
/// [`Plane`] per component (Y, Cb, Cr [, A] — or G, B, R [, A] for the
/// `Gbrp*` family an identity-matrix item decodes to), samples one
/// byte each at 8 bits and one little-endian 16-bit word above; the
/// two packed variants (`Rgb24`, `Rgba`) hold a single interleaved
/// plane. Range (full / limited) is not part of the layout: it lives
/// in [`ColorInfo::range`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PixelFormat {
    /// 8-bit monochrome.
    Gray8,
    /// 10-bit monochrome.
    Gray10Le,
    /// 12-bit monochrome.
    Gray12Le,
    /// 16-bit monochrome.
    Gray16Le,
    /// 8-bit YCbCr 4:2:0.
    Yuv420P,
    /// 10-bit YCbCr 4:2:0.
    Yuv420P10Le,
    /// 12-bit YCbCr 4:2:0.
    Yuv420P12Le,
    /// 16-bit YCbCr 4:2:0.
    Yuv420P16Le,
    /// 8-bit YCbCr 4:2:0 + alpha.
    Yuva420P,
    /// 10-bit YCbCr 4:2:0 + alpha.
    Yuva420P10Le,
    /// 8-bit YCbCr 4:2:2.
    Yuv422P,
    /// 10-bit YCbCr 4:2:2.
    Yuv422P10Le,
    /// 12-bit YCbCr 4:2:2.
    Yuv422P12Le,
    /// 16-bit YCbCr 4:2:2.
    Yuv422P16Le,
    /// 8-bit YCbCr 4:2:2 + alpha.
    Yuva422P,
    /// 10-bit YCbCr 4:2:2 + alpha.
    Yuva422P10Le,
    /// 12-bit YCbCr 4:2:2 + alpha.
    Yuva422P12Le,
    /// 16-bit YCbCr 4:2:2 + alpha.
    Yuva422P16Le,
    /// 8-bit YCbCr 4:4:4.
    Yuv444P,
    /// 10-bit YCbCr 4:4:4.
    Yuv444P10Le,
    /// 12-bit YCbCr 4:4:4.
    Yuv444P12Le,
    /// 16-bit YCbCr 4:4:4.
    Yuv444P16Le,
    /// 8-bit YCbCr 4:4:4 + alpha.
    Yuva444P,
    /// 10-bit YCbCr 4:4:4 + alpha.
    Yuva444P10Le,
    /// 12-bit YCbCr 4:4:4 + alpha.
    Yuva444P12Le,
    /// 16-bit YCbCr 4:4:4 + alpha.
    Yuva444P16Le,
    /// 8-bit planar RGB, planes G, B, R (an item coded with
    /// `matrix_coefficients = 0`).
    Gbrp8,
    /// 8-bit planar RGB + alpha.
    Gbrap8,
    /// 10-bit planar RGB.
    Gbrp10Le,
    /// 10-bit planar RGB + alpha.
    Gbrap10Le,
    /// 12-bit planar RGB.
    Gbrp12Le,
    /// 12-bit planar RGB + alpha.
    Gbrap12Le,
    /// 14-bit planar RGB.
    Gbrp14Le,
    /// 14-bit planar RGB + alpha.
    Gbrap14Le,
    /// 16-bit planar RGB.
    Gbrp16Le,
    /// 16-bit planar RGB + alpha.
    Gbrap16Le,
    /// Packed 8-bit RGB, 3 bytes per pixel (an input layout: what
    /// [`HeifImage::from_rgb8`] builds and [`encode()`](crate::encode())
    /// codes as 4:2:0 YCbCr).
    Rgb24,
    /// Packed 8-bit RGBA, 4 bytes per pixel ([`HeifImage::from_rgba8`]).
    Rgba,
}

impl PixelFormat {
    /// `(chroma, bit depth, alpha, planar RGB)` of a planar variant;
    /// `None` for the packed ones.
    fn planar_parts(self) -> Option<(Chroma, u8, bool, bool)> {
        use Chroma::*;
        use PixelFormat::*;
        Some(match self {
            Gray8 => (Mono, 8, false, false),
            Gray10Le => (Mono, 10, false, false),
            Gray12Le => (Mono, 12, false, false),
            Gray16Le => (Mono, 16, false, false),
            Yuv420P => (Yuv420, 8, false, false),
            Yuv420P10Le => (Yuv420, 10, false, false),
            Yuv420P12Le => (Yuv420, 12, false, false),
            Yuv420P16Le => (Yuv420, 16, false, false),
            Yuva420P => (Yuv420, 8, true, false),
            Yuva420P10Le => (Yuv420, 10, true, false),
            Yuv422P => (Yuv422, 8, false, false),
            Yuv422P10Le => (Yuv422, 10, false, false),
            Yuv422P12Le => (Yuv422, 12, false, false),
            Yuv422P16Le => (Yuv422, 16, false, false),
            Yuva422P => (Yuv422, 8, true, false),
            Yuva422P10Le => (Yuv422, 10, true, false),
            Yuva422P12Le => (Yuv422, 12, true, false),
            Yuva422P16Le => (Yuv422, 16, true, false),
            Yuv444P => (Yuv444, 8, false, false),
            Yuv444P10Le => (Yuv444, 10, false, false),
            Yuv444P12Le => (Yuv444, 12, false, false),
            Yuv444P16Le => (Yuv444, 16, false, false),
            Yuva444P => (Yuv444, 8, true, false),
            Yuva444P10Le => (Yuv444, 10, true, false),
            Yuva444P12Le => (Yuv444, 12, true, false),
            Yuva444P16Le => (Yuv444, 16, true, false),
            Gbrp8 => (Yuv444, 8, false, true),
            Gbrap8 => (Yuv444, 8, true, true),
            Gbrp10Le => (Yuv444, 10, false, true),
            Gbrap10Le => (Yuv444, 10, true, true),
            Gbrp12Le => (Yuv444, 12, false, true),
            Gbrap12Le => (Yuv444, 12, true, true),
            Gbrp14Le => (Yuv444, 14, false, true),
            Gbrap14Le => (Yuv444, 14, true, true),
            Gbrp16Le => (Yuv444, 16, false, true),
            Gbrap16Le => (Yuv444, 16, true, true),
            Rgb24 | Rgba => return None,
        })
    }

    /// The variant for a planar sample layout: the `Gbrp*` / `Gbrap*`
    /// family when `planar_rgb` is set and the layout is 4:4:4 at a
    /// depth that family has (8 / 10 / 12 / 14 / 16), else the YCbCr
    /// / grey variant. `None` when no variant describes the layout
    /// (odd depths, grey + alpha — the framework's `Ya8` is packed —
    /// 4:2:0 12/16-bit + alpha); see [`PixelFormat::from_layout_promoting`].
    pub fn from_layout(layout: HeifPixelFormat, planar_rgb: bool) -> Option<Self> {
        use Chroma::*;
        use PixelFormat::*;
        let (c, d, a) = (layout.chroma, layout.bit_depth, layout.has_alpha);
        if planar_rgb && c == Yuv444 {
            if let Some(f) = match (d, a) {
                (8, false) => Some(Gbrp8),
                (8, true) => Some(Gbrap8),
                (10, false) => Some(Gbrp10Le),
                (10, true) => Some(Gbrap10Le),
                (12, false) => Some(Gbrp12Le),
                (12, true) => Some(Gbrap12Le),
                (14, false) => Some(Gbrp14Le),
                (14, true) => Some(Gbrap14Le),
                (16, false) => Some(Gbrp16Le),
                (16, true) => Some(Gbrap16Le),
                _ => None,
            } {
                return Some(f);
            }
        }
        Some(match (c, d, a) {
            (Mono, 8, false) => Gray8,
            (Mono, 10, false) => Gray10Le,
            (Mono, 12, false) => Gray12Le,
            (Mono, 16, false) => Gray16Le,
            (Yuv420, 8, false) => Yuv420P,
            (Yuv420, 10, false) => Yuv420P10Le,
            (Yuv420, 12, false) => Yuv420P12Le,
            (Yuv420, 16, false) => Yuv420P16Le,
            (Yuv420, 8, true) => Yuva420P,
            (Yuv420, 10, true) => Yuva420P10Le,
            (Yuv422, 8, false) => Yuv422P,
            (Yuv422, 10, false) => Yuv422P10Le,
            (Yuv422, 12, false) => Yuv422P12Le,
            (Yuv422, 16, false) => Yuv422P16Le,
            (Yuv422, 8, true) => Yuva422P,
            (Yuv422, 10, true) => Yuva422P10Le,
            (Yuv422, 12, true) => Yuva422P12Le,
            (Yuv422, 16, true) => Yuva422P16Le,
            (Yuv444, 8, false) => Yuv444P,
            (Yuv444, 10, false) => Yuv444P10Le,
            (Yuv444, 12, false) => Yuv444P12Le,
            (Yuv444, 16, false) => Yuv444P16Le,
            (Yuv444, 8, true) => Yuva444P,
            (Yuv444, 10, true) => Yuva444P10Le,
            (Yuv444, 12, true) => Yuva444P12Le,
            (Yuv444, 16, true) => Yuva444P16Le,
            _ => return None,
        })
    }

    /// [`PixelFormat::from_layout`], falling back to the 4:4:4 variant
    /// of the same depth / alpha for layouts without a variant of their
    /// own (the promotion [`HeifImage::from_frame`] performs). Depths
    /// with no 4:4:4 variant either (9 / 11 / 13 / 14 / 15 bits as
    /// YCbCr) land on the nearest wider 4:4:4 variant.
    pub fn from_layout_promoting(layout: HeifPixelFormat, planar_rgb: bool) -> Self {
        if let Some(f) = Self::from_layout(layout, planar_rgb) {
            return f;
        }
        let d = layout.bit_depth;
        let promoted = HeifPixelFormat {
            chroma: Chroma::Yuv444,
            bit_depth: if d <= 8 {
                8
            } else if d <= 10 {
                10
            } else if d <= 12 {
                12
            } else {
                16
            },
            has_alpha: layout.has_alpha,
        };
        Self::from_layout(promoted, false).unwrap_or(PixelFormat::Yuva444P16Le)
    }

    /// The planar sample layout of this variant (`Gbrp*` is a 4:4:4
    /// layout whose planes are G, B, R); `None` for the packed ones.
    pub fn layout(self) -> Option<HeifPixelFormat> {
        self.planar_parts().map(|(c, d, a, _)| HeifPixelFormat {
            chroma: c,
            bit_depth: d,
            has_alpha: a,
        })
    }

    /// Bits per sample.
    pub fn bit_depth(self) -> u8 {
        self.planar_parts().map(|p| p.1).unwrap_or(8)
    }

    /// `true` when the layout carries an alpha component.
    pub fn has_alpha(self) -> bool {
        match self.planar_parts() {
            Some((_, _, a, _)) => a,
            None => self == PixelFormat::Rgba,
        }
    }

    /// `true` for the interleaved variants (`Rgb24`, `Rgba`).
    pub fn is_packed(self) -> bool {
        self.planar_parts().is_none()
    }

    /// `true` for the `Gbrp*` / `Gbrap*` family.
    pub fn is_planar_rgb(self) -> bool {
        matches!(self.planar_parts(), Some((_, _, _, true)))
    }

    /// `true` for the monochrome variants.
    pub fn is_gray(self) -> bool {
        matches!(self.planar_parts(), Some((Chroma::Mono, _, _, _)))
    }

    /// Number of planes: 1 for packed layouts, components otherwise.
    pub fn plane_count(self) -> usize {
        self.layout().map(|l| l.plane_count()).unwrap_or(1)
    }

    /// Bytes per pixel of a packed variant (3 / 4); `None` for planar.
    pub fn packed_bytes_per_pixel(self) -> Option<usize> {
        match self {
            PixelFormat::Rgb24 => Some(3),
            PixelFormat::Rgba => Some(4),
            _ => None,
        }
    }

    /// Size of plane `plane` in samples for a `width × height` picture.
    pub fn plane_dims(self, plane: usize, width: u32, height: u32) -> (u32, u32) {
        match self.layout() {
            Some(l) => l.plane_dims(plane, width, height),
            None => (width, height),
        }
    }

    /// Bytes per row of plane `plane` for a `width`-pixel picture.
    pub fn plane_row_bytes(self, plane: usize, width: u32) -> usize {
        match self.layout() {
            Some(l) => l.plane_dims(plane, width, 1).0 as usize * l.bytes_per_sample(),
            None => width as usize * self.packed_bytes_per_pixel().unwrap_or(3),
        }
    }
}

impl TryFrom<HeifPixelFormat> for PixelFormat {
    type Error = HeifError;

    /// The YCbCr / grey variant of a sample layout (a 4:4:4 layout
    /// holding G, B, R planes wants [`PixelFormat::from_layout`] with
    /// `planar_rgb`).
    fn try_from(layout: HeifPixelFormat) -> Result<Self> {
        Self::from_layout(layout, false).ok_or_else(|| {
            HeifError::unsupported(format!(
                "no pixel-format variant for {:?} {}-bit{}",
                layout.chroma,
                layout.bit_depth,
                if layout.has_alpha { " + alpha" } else { "" }
            ))
        })
    }
}

impl TryFrom<PixelFormat> for HeifPixelFormat {
    type Error = HeifError;

    /// The planar sample layout of a variant; packed variants have none.
    fn try_from(f: PixelFormat) -> Result<Self> {
        f.layout()
            .ok_or_else(|| HeifError::unsupported(format!("{f:?} is a packed layout")))
    }
}

// ---------------------------------------------------------------------
// Colour, metadata, palette
// ---------------------------------------------------------------------

/// Nominal sample range (H.273 `VideoFullRangeFlag`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColorRange {
    /// No range was signalled.
    #[default]
    Unspecified,
    /// Limited (video) range: `VideoFullRangeFlag == 0`.
    Limited,
    /// Full range: `VideoFullRangeFlag == 1`.
    Full,
}

/// Colour description of a [`HeifImage`]: the H.273 code points and
/// range the file signals (`colr` `nclx`), or the MIAF §7.3.6.4 default
/// when it signals none — BT.709 primaries (1), sRGB transfer (13),
/// BT.601 matrix (6), full range — which is also [`Default`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ColorInfo {
    /// Sample range.
    pub range: ColorRange,
    /// H.273 `ColourPrimaries`.
    pub primaries: u8,
    /// H.273 `TransferCharacteristics`.
    pub transfer: u8,
    /// H.273 `MatrixCoefficients` (0 = identity: RGB planes).
    pub matrix: u8,
}

impl Default for ColorInfo {
    fn default() -> Self {
        Self::from_colr(&Colr::MIAF_DEFAULT)
    }
}

impl ColorInfo {
    /// Every field as a positional argument, in declaration order.
    pub fn new(range: ColorRange, primaries: u8, transfer: u8, matrix: u8) -> Self {
        Self {
            range,
            primaries,
            transfer,
            matrix,
        }
    }

    /// The description for packed / planar RGB pixels: sRGB primaries
    /// and transfer, identity matrix, full range.
    pub fn srgb() -> Self {
        Self::new(ColorRange::Full, 1, 13, 0)
    }

    /// Nothing signalled: every code point 2 ("unspecified"), range
    /// unspecified. What [`HeifImage::new`] / `From<HeifFrame>` leave
    /// in place so that [`EncodeOptions::colr`](crate::EncodeOptions)
    /// decides on encode; the conversions read it as the MIAF default.
    pub fn unspecified() -> Self {
        Self::new(ColorRange::Unspecified, 2, 2, 2)
    }

    /// `true` when nothing is signalled ([`ColorInfo::unspecified`]).
    pub fn is_unspecified(&self) -> bool {
        *self == Self::unspecified()
    }

    /// From a `colr` property: the `nclx` code points (clamped to a
    /// byte), or the MIAF default for an ICC / unknown `colr`.
    pub fn from_colr(colr: &Colr) -> Self {
        match colr {
            Colr::Nclx {
                primaries,
                transfer,
                matrix,
                full_range,
            } => Self {
                range: if *full_range {
                    ColorRange::Full
                } else {
                    ColorRange::Limited
                },
                primaries: (*primaries).min(255) as u8,
                transfer: (*transfer).min(255) as u8,
                matrix: (*matrix).min(255) as u8,
            },
            _ => Self::new(ColorRange::Full, 1, 13, 6),
        }
    }

    /// As an `nclx` `colr` property (an unspecified range is written
    /// full, the MIAF default).
    pub fn to_colr(&self) -> Colr {
        Colr::Nclx {
            primaries: self.primaries as u16,
            transfer: self.transfer as u16,
            matrix: self.matrix as u16,
            full_range: self.range != ColorRange::Limited,
        }
    }

    /// `true` unless the range is [`ColorRange::Limited`].
    pub fn is_full_range(&self) -> bool {
        self.range != ColorRange::Limited
    }
}

/// Embedded metadata of a [`HeifImage`].
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Metadata {
    /// ICC profile (`colr` `prof` / `rICC`).
    pub icc: Option<Vec<u8>>,
    /// Exif payload from the TIFF header on (the HEIF offset word
    /// resolved).
    pub exif: Option<Vec<u8>>,
    /// XMP packet (UTF-8 bytes).
    pub xmp: Option<Vec<u8>>,
    /// Display gamma, when the format carries one (HEIF never does;
    /// the transfer characteristic is in [`ColorInfo`]).
    pub gamma: Option<f32>,
}

impl Metadata {
    /// Every field as a positional argument, in declaration order.
    pub fn new(
        icc: Option<Vec<u8>>,
        exif: Option<Vec<u8>>,
        xmp: Option<Vec<u8>>,
        gamma: Option<f32>,
    ) -> Self {
        Self {
            icc,
            exif,
            xmp,
            gamma,
        }
    }

    /// `true` when no field is set.
    pub fn is_empty(&self) -> bool {
        self.icc.is_none() && self.exif.is_none() && self.xmp.is_none() && self.gamma.is_none()
    }
}

/// A colour table (indexed formats). HEIF has no indexed layout:
/// [`HeifImage::palette`] is always `None`; the type exists for shape
/// parity with the other image crates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Palette {
    /// RGBA8 entries, index order.
    pub entries: Vec<[u8; 4]>,
}

impl Palette {
    /// Every field as a positional argument, in declaration order.
    pub fn new(entries: Vec<[u8; 4]>) -> Self {
        Self { entries }
    }
}

// ---------------------------------------------------------------------
// The image type
// ---------------------------------------------------------------------

/// A decoded (or to-be-encoded) picture in its native layout: the
/// contract's image type. Planar layouts hold one [`Plane`] per
/// component; the packed ones exactly one plane. `color` and `metadata`
/// come from the file on decode and drive the `colr` / metadata items
/// on encode.
///
/// Conversions: [`HeifImage::to_rgb8`] / [`HeifImage::to_rgba8`] are
/// the raw byte paths (exact H.273 kernels at the signalled matrix and
/// range, deep sources scaled to 8 bits). Everything else — 16-bit
/// output, re-layouts, resizing — belongs to `oxideav-image` /
/// `oxideav-pixfmt` on the framework side.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct HeifImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Native layout.
    pub format: PixelFormat,
    /// The planes (`format.plane_count()` of them).
    pub planes: Vec<Plane>,
    /// Colour description.
    pub color: ColorInfo,
    /// Embedded metadata.
    pub metadata: Metadata,
    /// Colour table; always `None` for HEIF.
    pub palette: Option<Palette>,
}

impl HeifImage {
    /// Build from planes in `format`'s layout, no metadata; the plane
    /// geometry is validated ([`HeifImage::validate`]) so an invalid
    /// image cannot be built here. The colour is [`ColorInfo::srgb`]
    /// for packed / planar RGB layouts and [`ColorInfo::unspecified`]
    /// otherwise (so `EncodeOptions::colr` governs an encode, as for a
    /// bare frame); [`decode()`](crate::decode()) fills it from the file.
    pub fn new(width: u32, height: u32, format: PixelFormat, planes: Vec<Plane>) -> Result<Self> {
        let img = Self::unchecked(width, height, format, planes);
        img.validate()?;
        Ok(img)
    }

    /// [`HeifImage::new`] without the geometry check (the colour and
    /// metadata defaults are the same).
    pub(crate) fn unchecked(
        width: u32,
        height: u32,
        format: PixelFormat,
        planes: Vec<Plane>,
    ) -> Self {
        Self {
            width,
            height,
            format,
            planes,
            color: if format.is_packed() || format.is_planar_rgb() {
                ColorInfo::srgb()
            } else {
                ColorInfo::unspecified()
            },
            metadata: Metadata::default(),
            palette: None,
        }
    }

    /// Set the colour description.
    pub fn with_color(mut self, color: ColorInfo) -> Self {
        self.color = color;
        self
    }

    /// Set the metadata.
    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Set the palette (kept for shape parity; HEIF ignores it).
    pub fn with_palette(mut self, palette: Option<Palette>) -> Self {
        self.palette = palette;
        self
    }

    /// A packed `Rgb24` image over `data` (`3 × width × height` bytes,
    /// row-major, stride `3 × width`), validated like
    /// [`HeifImage::new`]: a zero dimension or a short buffer is
    /// `InvalidData`.
    pub fn from_rgb8(width: u32, height: u32, data: Vec<u8>) -> Result<Self> {
        Self::new(
            width,
            height,
            PixelFormat::Rgb24,
            vec![Plane {
                stride: width as usize * 3,
                data,
            }],
        )
    }

    /// A packed `Rgba` image over `data` (`4 × width × height` bytes),
    /// validated like [`HeifImage::new`].
    pub fn from_rgba8(width: u32, height: u32, data: Vec<u8>) -> Result<Self> {
        Self::new(
            width,
            height,
            PixelFormat::Rgba,
            vec![Plane {
                stride: width as usize * 4,
                data,
            }],
        )
    }

    /// From a composed planar frame plus its colour / metadata. A
    /// layout without a [`PixelFormat`] variant (grey + alpha, 4:2:0
    /// 12/16-bit + alpha, odd depths) is promoted to 4:4:4
    /// with neutral chroma so no sample is lost; a 4:4:4 frame whose
    /// `color.matrix` is 0 (identity) is labelled `Gbrp*` (its planes
    /// are G, B, R) when its depth has such a variant.
    pub fn from_frame(frame: HeifFrame, color: ColorInfo, metadata: Metadata) -> Self {
        let identity = color.matrix == 0;
        let gbr = identity
            .then(|| PixelFormat::from_layout(frame.format, true))
            .flatten()
            .filter(|f| f.is_planar_rgb());
        let (format, frame) = match gbr.or_else(|| PixelFormat::from_layout(frame.format, false)) {
            Some(f) => (f, frame),
            None => {
                let promoted = match frame.promote_to_444() {
                    Ok(p) => p,
                    Err(_) => frame,
                };
                (
                    PixelFormat::from_layout_promoting(promoted.format, false),
                    promoted,
                )
            }
        };
        Self {
            width: frame.width,
            height: frame.height,
            format,
            planes: frame.planes,
            color,
            metadata,
            palette: None,
        }
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Native layout.
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// The pixel bytes of a packed layout (the single plane); `None`
    /// for planar layouts — use [`HeifImage::into_raw`] or the planes.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        if self.format.is_packed() {
            self.planes.first().map(|p| p.data.as_slice())
        } else {
            None
        }
    }

    /// The raw bytes: the plane of a packed layout, or every plane
    /// concatenated in order (strides as reported).
    pub fn into_raw(self) -> Vec<u8> {
        let mut planes = self.planes.into_iter();
        let Some(first) = planes.next() else {
            return Vec::new();
        };
        let mut out = first.data;
        for p in planes {
            out.extend_from_slice(&p.data);
        }
        out
    }

    /// Check plane count and sizes against `width × height` / `format`.
    pub fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(HeifError::invalid("zero-sized image"));
        }
        if self.width as u64 * self.height as u64 > crate::image::MAX_FRAME_PIXELS {
            return Err(HeifError::limit(format!(
                "image {}x{} exceeds {} pixels",
                self.width,
                self.height,
                crate::image::MAX_FRAME_PIXELS
            )));
        }
        match self.format.layout() {
            Some(layout) => HeifFrame {
                width: self.width,
                height: self.height,
                format: layout,
                planes: self.planes.clone(),
            }
            .validate(),
            None => {
                let bpp = self.format.packed_bytes_per_pixel().unwrap_or(3);
                let Some(p) = self.planes.first() else {
                    return Err(HeifError::invalid("packed image without a plane"));
                };
                if self.planes.len() != 1 {
                    return Err(HeifError::invalid(format!(
                        "packed image has {} planes",
                        self.planes.len()
                    )));
                }
                let row = self.width as usize * bpp;
                if p.stride < row || p.data.len() < p.stride * (self.height as usize - 1) + row {
                    return Err(HeifError::invalid(format!(
                        "packed plane: {} bytes at stride {} for {}x{}x{bpp}",
                        p.data.len(),
                        p.stride,
                        self.width,
                        self.height
                    )));
                }
                Ok(())
            }
        }
    }

    /// The planar frame of a planar image (a copy of the planes);
    /// `Unsupported` for a packed layout.
    pub fn to_frame(&self) -> Result<HeifFrame> {
        self.clone().into_frame()
    }

    /// The planar frame of a planar image, moving the planes;
    /// `Unsupported` for a packed layout.
    pub fn into_frame(self) -> Result<HeifFrame> {
        let format = HeifPixelFormat::try_from(self.format)?;
        let f = HeifFrame {
            width: self.width,
            height: self.height,
            format,
            planes: self.planes,
        };
        f.validate()?;
        Ok(f)
    }

    /// Tightly packed RGB, 3 bytes per pixel, `3 × width` bytes per
    /// row: exact H.273 conversion at the image's matrix and range,
    /// chroma replicated to its co-sited luma samples, deeper samples
    /// scaled to 8 bits (round to nearest), alpha dropped. Infallible
    /// on every image this crate decodes; a caller-assembled image with
    /// bad geometry yields a black (or padded) picture — see
    /// [`HeifImage::try_to_rgb8`].
    pub fn to_rgb8(&self) -> Vec<u8> {
        self.rgb8(false)
            .unwrap_or_else(|_| self.fallback_rgb8(false))
    }

    /// Tightly packed RGBA, 4 bytes per pixel; alpha opaque (255) when
    /// the layout has none.
    pub fn to_rgba8(&self) -> Vec<u8> {
        self.rgb8(true).unwrap_or_else(|_| self.fallback_rgb8(true))
    }

    /// [`HeifImage::to_rgb8`] reporting a bad plane geometry instead
    /// of substituting.
    pub fn try_to_rgb8(&self) -> Result<Vec<u8>> {
        self.rgb8(false)
    }

    /// [`HeifImage::to_rgba8`] reporting a bad plane geometry instead
    /// of substituting.
    pub fn try_to_rgba8(&self) -> Result<Vec<u8>> {
        self.rgb8(true)
    }

    fn fallback_rgb8(&self, alpha: bool) -> Vec<u8> {
        let n = self.width as usize * self.height as usize;
        let out_bpp = if alpha { 4 } else { 3 };
        let mut v = vec![0u8; n * out_bpp];
        if alpha {
            v.iter_mut().skip(3).step_by(4).for_each(|a| *a = 255);
        }
        v
    }

    fn rgb8(&self, alpha: bool) -> Result<Vec<u8>> {
        let (w, h) = (self.width as usize, self.height as usize);
        let out_bpp = if alpha { 4 } else { 3 };
        let first = self
            .planes
            .first()
            .ok_or_else(|| HeifError::invalid("image without planes"))?;
        match self.format {
            PixelFormat::Rgb24 | PixelFormat::Rgba => {
                let bpp = self.format.packed_bytes_per_pixel().unwrap_or(3);
                let row_bytes = w * bpp;
                let mut out = Vec::with_capacity(w * h * out_bpp);
                for y in 0..h {
                    let Some(row) = first
                        .data
                        .get(y * first.stride..)
                        .and_then(|r| r.get(..row_bytes))
                    else {
                        break;
                    };
                    for px in row.chunks_exact(bpp) {
                        out.extend_from_slice(&px[..3]);
                        if alpha {
                            out.push(if bpp == 4 { px[3] } else { 255 });
                        }
                    }
                }
                if out.len() != w * h * out_bpp {
                    return Err(HeifError::invalid("packed plane shorter than its geometry"));
                }
                Ok(out)
            }
            _ => {
                let layout = HeifPixelFormat::try_from(self.format)?;
                let colr = if self.format.is_planar_rgb() {
                    Colr::Nclx {
                        primaries: self.color.primaries as u16,
                        transfer: self.color.transfer as u16,
                        matrix: 0,
                        full_range: self.color.is_full_range(),
                    }
                } else {
                    self.color.to_colr()
                };
                crate::rgb::planes_to_rgb8(
                    self.width,
                    self.height,
                    layout,
                    &self.planes,
                    Some(&colr),
                    alpha,
                )
            }
        }
    }
}

impl From<HeifFrame> for HeifImage {
    /// A composed frame with an unspecified colour description and no
    /// metadata ([`HeifImage::from_frame`] takes the file's).
    fn from(frame: HeifFrame) -> Self {
        Self::from_frame(frame, ColorInfo::unspecified(), Metadata::default())
    }
}

// ---------------------------------------------------------------------
// Raw RGB images
// ---------------------------------------------------------------------

/// Tightly packed 8-bit RGB, 3 bytes per pixel, row-major: the one-call
/// output of [`decode_rgb8`](crate::decode_rgb8). Same definition in
/// every image crate.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RgbImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `3 × width × height` bytes.
    pub data: Vec<u8>,
}

impl RgbImage {
    /// Every field as a positional argument, in declaration order.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
        }
    }

    /// The pixel bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// The pixel bytes, moved out.
    pub fn into_raw(self) -> Vec<u8> {
        self.data
    }
}

/// Tightly packed 8-bit RGBA, 4 bytes per pixel, row-major: the
/// one-call output of [`decode_rgba8`](crate::decode_rgba8).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RgbaImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `4 × width × height` bytes.
    pub data: Vec<u8>,
}

impl RgbaImage {
    /// Every field as a positional argument, in declaration order.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
        }
    }

    /// The pixel bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// The pixel bytes, moved out.
    pub fn into_raw(self) -> Vec<u8> {
        self.data
    }
}

// ---------------------------------------------------------------------
// Header, options, frames
// ---------------------------------------------------------------------

/// What [`info`] reads from the container without decoding.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImageInfo {
    /// Primary image width after its transformative properties.
    pub width: u32,
    /// Primary image height after its transformative properties.
    pub height: u32,
    /// The layout [`decode()`](crate::decode()) yields.
    pub format: PixelFormat,
    /// Number of images [`decode_all`](crate::decode_all) yields: the
    /// displayable image items (bursts; `altr` alternatives count once)
    /// plus every sample of the image-sequence tracks.
    pub frames: u32,
    /// The primary image carries an alpha auxiliary.
    pub has_alpha: bool,
    /// Colour description (the MIAF default when none is signalled).
    pub color: ColorInfo,
    /// An ICC profile is attached.
    pub has_icc: bool,
    /// An Exif item is attached.
    pub has_exif: bool,
    /// An XMP item is attached.
    pub has_xmp: bool,
    /// The primary item (`pitm`), `None` for a sequence-only file.
    pub primary_item_id: Option<u32>,
    /// An ISO 21496-1 gain map (`tmap`) accompanies the primary image.
    pub has_gain_map: bool,
}

impl ImageInfo {
    /// Every field as a positional argument, in declaration order.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: u32,
        height: u32,
        format: PixelFormat,
        frames: u32,
        has_alpha: bool,
        color: ColorInfo,
        has_icc: bool,
        has_exif: bool,
        has_xmp: bool,
        primary_item_id: Option<u32>,
        has_gain_map: bool,
    ) -> Self {
        Self {
            width,
            height,
            format,
            frames,
            has_alpha,
            color,
            has_icc,
            has_exif,
            has_xmp,
            primary_item_id,
            has_gain_map,
        }
    }
}

/// Decoding options: limits (enforced before any decode or allocation),
/// strictness, and the HEIF-specific selections.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// Largest accepted output width (default 2²⁰; `None` = unlimited).
    pub max_width: Option<u32>,
    /// Largest accepted output height (default 2²⁰; `None` = unlimited).
    pub max_height: Option<u32>,
    /// Largest accepted output pixel count (default
    /// [`MAX_CANVAS_PIXELS`](crate::derived::MAX_CANVAS_PIXELS), 2³⁰;
    /// `None` = unlimited).
    pub max_pixels: Option<u64>,
    /// Largest accepted input size in bytes (default 4 GiB; `None` =
    /// unlimited).
    pub max_bytes: Option<u64>,
    /// Refuse files that do not declare a HEIF-family brand, and
    /// MIAF-branded files that violate the MIAF constraints
    /// ([`miaf::check`](crate::miaf::check)). Off, the reader is
    /// lenient: any ISOBMFF file with a `pict` item tree decodes.
    pub strict: bool,
    /// Decode this item instead of the primary (`pitm`).
    pub item_id: Option<u32>,
    /// Apply the ISO 21496-1 gain map (the `tmap` reconstruction, HEIF
    /// Amd 1 §6.6.2.4.1) instead of returning the base rendition.
    /// Off by default: SDR pipelines get the SDR picture, and the
    /// decoded gain map stays available through
    /// [`decode_primary`](crate::decode_primary).
    pub tone_mapped: bool,
    /// Decode the base layer of a layered (`lhv1`) item whose `tols`
    /// asks for enhancement layers, instead of refusing.
    pub base_layer_fallback: bool,
    /// Thread budget for independent coded items (grid tiles);
    /// `None` / `Some(1)` = serial. Output is identical for every budget.
    pub threads: Option<usize>,
    /// HDR reference white (cd/m²) a PQ-coded tone-mapped
    /// reconstruction is anchored to.
    pub reference_white_nits: Option<f64>,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            max_width: Some(1 << 20),
            max_height: Some(1 << 20),
            max_pixels: Some(crate::derived::MAX_CANVAS_PIXELS),
            max_bytes: Some(1 << 32),
            strict: false,
            item_id: None,
            tone_mapped: false,
            base_layer_fallback: false,
            threads: None,
            reference_white_nits: None,
        }
    }
}

impl DecodeOptions {
    /// Every field as a positional argument, in declaration order.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        max_width: Option<u32>,
        max_height: Option<u32>,
        max_pixels: Option<u64>,
        max_bytes: Option<u64>,
        strict: bool,
        item_id: Option<u32>,
        tone_mapped: bool,
        base_layer_fallback: bool,
        threads: Option<usize>,
        reference_white_nits: Option<f64>,
    ) -> Self {
        Self {
            max_width,
            max_height,
            max_pixels,
            max_bytes,
            strict,
            item_id,
            tone_mapped,
            base_layer_fallback,
            threads,
            reference_white_nits,
        }
    }

    /// Set [`DecodeOptions::max_width`].
    pub fn with_max_width(mut self, max_width: Option<u32>) -> Self {
        self.max_width = max_width;
        self
    }

    /// Set [`DecodeOptions::max_height`].
    pub fn with_max_height(mut self, max_height: Option<u32>) -> Self {
        self.max_height = max_height;
        self
    }

    /// Set [`DecodeOptions::max_pixels`].
    pub fn with_max_pixels(mut self, max_pixels: Option<u64>) -> Self {
        self.max_pixels = max_pixels;
        self
    }

    /// Set [`DecodeOptions::max_bytes`].
    pub fn with_max_bytes(mut self, max_bytes: Option<u64>) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Set [`DecodeOptions::strict`].
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Set [`DecodeOptions::item_id`].
    pub fn with_item_id(mut self, item_id: Option<u32>) -> Self {
        self.item_id = item_id;
        self
    }

    /// Set [`DecodeOptions::tone_mapped`].
    pub fn with_tone_mapped(mut self, tone_mapped: bool) -> Self {
        self.tone_mapped = tone_mapped;
        self
    }

    /// Set [`DecodeOptions::base_layer_fallback`].
    pub fn with_base_layer_fallback(mut self, base_layer_fallback: bool) -> Self {
        self.base_layer_fallback = base_layer_fallback;
        self
    }

    /// Set [`DecodeOptions::threads`].
    pub fn with_threads(mut self, threads: Option<usize>) -> Self {
        self.threads = threads;
        self
    }

    /// Set [`DecodeOptions::reference_white_nits`].
    pub fn with_reference_white_nits(mut self, reference_white_nits: Option<f64>) -> Self {
        self.reference_white_nits = reference_white_nits;
        self
    }

    /// Check an input size against [`DecodeOptions::max_bytes`].
    pub fn check_input(&self, len: usize) -> Result<()> {
        if let Some(max) = self.max_bytes {
            if len as u64 > max {
                return Err(HeifError::limit(format!(
                    "{len}-byte input exceeds max_bytes {max}"
                )));
            }
        }
        Ok(())
    }

    /// Check an output geometry against the dimension / pixel limits.
    pub fn check_dims(&self, width: u32, height: u32) -> Result<()> {
        if self.max_width.is_some_and(|m| width > m) || self.max_height.is_some_and(|m| height > m)
        {
            return Err(HeifError::limit(format!(
                "{width}x{height} exceeds max_width {:?} / max_height {:?}",
                self.max_width, self.max_height
            )));
        }
        if let Some(max) = self.max_pixels {
            if width as u64 * height as u64 > max {
                return Err(HeifError::limit(format!(
                    "{width}x{height} exceeds max_pixels {max}"
                )));
            }
        }
        Ok(())
    }
}

/// One image of a multi-image file ([`decode_all`](crate::decode_all)).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Frame {
    /// The picture.
    pub image: HeifImage,
    /// Display duration of a sequence sample (from the track timing);
    /// `None` for image items.
    pub delay: Option<Duration>,
    /// The image item this frame is, for burst / item frames.
    pub item_id: Option<u32>,
    /// The track this frame is a sample of, for sequence frames.
    pub track_id: Option<u32>,
}

impl Frame {
    /// Every field as a positional argument, in declaration order.
    pub fn new(
        image: HeifImage,
        delay: Option<Duration>,
        item_id: Option<u32>,
        track_id: Option<u32>,
    ) -> Self {
        Self {
            image,
            delay,
            item_id,
            track_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(c: Chroma, d: u8, a: bool) -> HeifPixelFormat {
        HeifPixelFormat::new(c, d, a).unwrap()
    }

    #[test]
    fn probe_sniffs_heif_brands_only() {
        assert!(!probe(b""));
        assert!(!probe(b"\0\0\0\x10ftypisom"));
        let ft = crate::ftyp::FileType {
            box_type: *b"ftyp",
            major_brand: *b"heic",
            minor_version: 0,
            compatible_brands: vec![*b"mif1", *b"heic"],
        }
        .to_box();
        assert!(probe(&ft));
        let generic = crate::ftyp::FileType {
            box_type: *b"ftyp",
            major_brand: *b"isom",
            minor_version: 0,
            compatible_brands: vec![*b"mp42"],
        }
        .to_box();
        assert!(!probe(&generic));
        assert!(info(&ft).is_err(), "a bare ftyp has no image");
    }

    #[test]
    fn pixel_format_round_trips_every_planar_variant() {
        use PixelFormat::*;
        let all = [
            Gray8,
            Gray10Le,
            Gray12Le,
            Gray16Le,
            Yuv420P,
            Yuv420P10Le,
            Yuv420P12Le,
            Yuv420P16Le,
            Yuva420P,
            Yuva420P10Le,
            Yuv422P,
            Yuv422P10Le,
            Yuv422P12Le,
            Yuv422P16Le,
            Yuva422P,
            Yuva422P10Le,
            Yuva422P12Le,
            Yuva422P16Le,
            Yuv444P,
            Yuv444P10Le,
            Yuv444P12Le,
            Yuv444P16Le,
            Yuva444P,
            Yuva444P10Le,
            Yuva444P12Le,
            Yuva444P16Le,
            Gbrp8,
            Gbrap8,
            Gbrp10Le,
            Gbrap10Le,
            Gbrp12Le,
            Gbrap12Le,
            Gbrp14Le,
            Gbrap14Le,
            Gbrp16Le,
            Gbrap16Le,
        ];
        for f in all {
            let l = f.layout().unwrap();
            assert_eq!(
                PixelFormat::from_layout(l, f.is_planar_rgb()),
                Some(f),
                "{f:?}"
            );
            assert_eq!(HeifPixelFormat::try_from(f).unwrap(), l);
            assert_eq!(f.bit_depth(), l.bit_depth);
            assert_eq!(f.has_alpha(), l.has_alpha);
            assert_eq!(f.plane_count(), l.plane_count());
            assert!(!f.is_packed());
        }
        assert!(Rgb24.is_packed() && !Rgb24.has_alpha() && Rgba.has_alpha());
        assert!(HeifPixelFormat::try_from(Rgba).is_err());
        assert!(PixelFormat::try_from(layout(Chroma::Mono, 10, true)).is_err());
        assert!(PixelFormat::try_from(layout(Chroma::Mono, 8, true)).is_err());
        assert_eq!(
            PixelFormat::from_layout_promoting(layout(Chroma::Mono, 8, true), false),
            Yuva444P
        );
        // Promotion: grey + alpha at 10 bits → 4:4:4 10-bit + alpha.
        assert_eq!(
            PixelFormat::from_layout_promoting(layout(Chroma::Mono, 10, true), false),
            Yuva444P10Le
        );
        assert_eq!(
            PixelFormat::from_layout_promoting(layout(Chroma::Yuv420, 9, false), false),
            Yuv444P10Le
        );
        // Identity matrix at an odd depth keeps a YCbCr label.
        assert_eq!(
            PixelFormat::from_layout(layout(Chroma::Yuv444, 9, false), true),
            None
        );
        assert_eq!(
            PixelFormat::from_layout(layout(Chroma::Yuv444, 12, true), true),
            Some(Gbrap12Le)
        );
        assert_eq!(Rgb24.plane_row_bytes(0, 5), 15);
        assert_eq!(Yuv420P10Le.plane_row_bytes(1, 5), 6);
    }

    #[test]
    fn color_info_defaults_to_miaf_and_round_trips_colr() {
        let d = ColorInfo::default();
        assert_eq!(d, ColorInfo::new(ColorRange::Full, 1, 13, 6));
        assert_eq!(d.to_colr(), Colr::MIAF_DEFAULT);
        let limited = Colr::Nclx {
            primaries: 9,
            transfer: 16,
            matrix: 9,
            full_range: false,
        };
        let c = ColorInfo::from_colr(&limited);
        assert_eq!(c.range, ColorRange::Limited);
        assert_eq!(c.to_colr(), limited);
        assert!(!c.is_full_range());
        let icc = Colr::Icc {
            restricted: false,
            profile: vec![0; 4],
        };
        assert_eq!(ColorInfo::from_colr(&icc), d);
        assert!(Metadata::default().is_empty());
    }

    #[test]
    fn packed_images_convert_and_validate() {
        let rgb = HeifImage::from_rgb8(2, 1, vec![1, 2, 3, 4, 5, 6]).unwrap();
        rgb.validate().unwrap();
        assert_eq!(rgb.as_bytes(), Some(&[1u8, 2, 3, 4, 5, 6][..]));
        assert_eq!(rgb.to_rgb8(), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(rgb.to_rgba8(), vec![1, 2, 3, 255, 4, 5, 6, 255]);
        assert_eq!(rgb.color, ColorInfo::srgb());
        assert!(
            rgb.to_frame().is_err(),
            "packed layouts have no planar frame"
        );
        let rgba = HeifImage::from_rgba8(1, 2, vec![1, 2, 3, 9, 4, 5, 6, 8]).unwrap();
        assert_eq!(rgba.to_rgb8(), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(rgba.clone().into_raw(), vec![1, 2, 3, 9, 4, 5, 6, 8]);
        // A short buffer cannot be built; the kernels stay defensive for
        // an image assembled inside the crate.
        assert!(matches!(
            HeifImage::from_rgba8(2, 2, vec![0; 3]),
            Err(HeifError::InvalidData(_))
        ));
        let short = HeifImage::unchecked(
            2,
            2,
            PixelFormat::Rgba,
            vec![Plane {
                stride: 8,
                data: vec![0; 3],
            }],
        );
        assert!(short.validate().is_err());
        assert!(short.try_to_rgba8().is_err());
        assert_eq!(
            short.to_rgba8(),
            vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]
        );
        assert!(
            HeifImage::new(2, 2, PixelFormat::Yuv420P, vec![Plane::new(2, vec![0; 4])]).is_err()
        );
        assert!(HeifImage::from_rgb8(0, 1, vec![]).is_err());
    }

    #[test]
    fn planar_images_convert_through_the_signalled_matrix() {
        // Full-range BT.601 grey level 128 → RGB 128.
        let f = HeifFrame::filled(2, 2, layout(Chroma::Yuv420, 8, false), 128).unwrap();
        let img = HeifImage::from(f.clone());
        assert_eq!(img.format, PixelFormat::Yuv420P);
        assert!(img.color.is_unspecified());
        assert_eq!(img.to_rgb8(), vec![128; 12]);
        assert_eq!(img.to_rgba8().iter().filter(|v| **v == 255).count(), 4);
        assert!(img.as_bytes().is_none());
        assert_eq!(img.clone().into_frame().unwrap(), f);
        // 10-bit samples scale to 8 bits: 1023 → 255.
        let mut deep = HeifFrame::filled(1, 1, layout(Chroma::Yuv444, 10, false), 512).unwrap();
        deep.set_sample(0, 0, 0, 1023);
        let img = HeifImage::from(deep);
        assert_eq!(img.format, PixelFormat::Yuv444P10Le);
        assert_eq!(
            img.to_rgb8(),
            vec![255, 255, 255],
            "luma 1023, neutral chroma → white"
        );
        // Identity matrix: planes G, B, R → Gbrp8, R = plane 2.
        let mut g = HeifFrame::zeroed(1, 1, layout(Chroma::Yuv444, 8, false)).unwrap();
        g.set_sample(0, 0, 0, 10);
        g.set_sample(1, 0, 0, 20);
        g.set_sample(2, 0, 0, 30);
        let img = HeifImage::from_frame(g, ColorInfo::srgb(), Metadata::default());
        assert_eq!(img.format, PixelFormat::Gbrp8);
        assert_eq!(img.to_rgb8(), vec![30, 10, 20]);
        // Promotion of grey + alpha 10-bit.
        let ga = HeifFrame::filled(2, 1, layout(Chroma::Mono, 10, true), 512).unwrap();
        let img = HeifImage::from(ga);
        assert_eq!(img.format, PixelFormat::Yuva444P10Le);
        assert_eq!(img.planes.len(), 4);
        img.validate().unwrap();
        let rgba = img.to_rgba8();
        assert_eq!(rgba.len(), 8);
        assert_eq!(rgba[3], 128, "10-bit alpha 512 → 128");
    }

    #[test]
    fn decode_options_enforce_limits() {
        let o = DecodeOptions::default()
            .with_max_width(Some(10))
            .with_max_height(Some(10))
            .with_max_pixels(Some(50))
            .with_max_bytes(Some(100));
        o.check_dims(5, 5).unwrap();
        assert!(matches!(
            o.check_dims(11, 1),
            Err(HeifError::LimitExceeded(_))
        ));
        assert!(matches!(
            o.check_dims(10, 10),
            Err(HeifError::LimitExceeded(_))
        ));
        assert!(o.check_input(101).is_err());
        o.check_input(100).unwrap();
        assert!(!DecodeOptions::default().strict);
        let unlimited = DecodeOptions::new(
            None, None, None, None, false, None, false, false, None, None,
        );
        unlimited.check_dims(u32::MAX, u32::MAX).unwrap();
        unlimited.check_input(usize::MAX).unwrap();
    }
}
