//! ISO 21496-1 gain maps: the binary metadata payload (Annex C.2) a
//! `tmap` derived item carries, and the application of a gain map to a
//! baseline image (§6) to obtain the alternate (HDR-headroom-applied)
//! rendition — an explicit opt-in; the default decode path still
//! yields the baseline image.
//!
//! Pipeline (Figure 2): decode base + gain map → unnormalise the gain
//! map (Formula 1: `G = (max − min) · Gnorm^(1/γ) + min`, log₂ domain)
//! → resample it to the base size (§6.2.2) → linearise the base (H.273
//! transfer characteristics) → convert to the gain-map application
//! space primaries (Annex B: the base's or the alternate's, per
//! `use_base_colour_space`) → combine per channel (Formula 2:
//! `Alt = (Base + k_base) · 2^(W·G) − k_alt`, with the headroom weight
//! `W` of Formula 3). The result is a linear RGB picture in the
//! application space; [`LinearRgbImage::encode`] re-encodes it with an
//! H.273 transfer in a target set of primaries for display / tests.
//!
//! Carriage in HEIF (which `dimg` input is the baseline, that the
//! `tmap` item body is the C.2 structure verbatim, and that the `tmap`
//! item's own `colr` describes the alternate image) follows what every
//! black-box producer measured in this crate's interop corpus writes;
//! ISO 21496-1 C.3 delegates it to the file format and the staged
//! HEIF / MIAF texts do not cover `tmap` (see the crate README).

use crate::error::{HeifError, Result};
use crate::image::{Chroma, HeifFrame};
use crate::props::Colr;
use crate::rgb::to_rgb;

/// A signed rational (`numerator / denominator`) as stored in the
/// metadata; `den` is never 0 after a successful parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rational {
    /// Numerator (signed for the min / max / offsets, non-negative
    /// for gamma and the headrooms).
    pub num: i64,
    /// Denominator.
    pub den: u32,
}

impl Rational {
    /// The value.
    pub fn value(&self) -> f64 {
        self.num as f64 / self.den as f64
    }
}

/// Per-channel metadata (`GainMapChannel`, C.2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GainMapChannel {
    /// `min(G)`, log₂ domain (5.2.5.2).
    pub gain_map_min: Rational,
    /// `max(G)`, log₂ domain (5.2.5.3).
    pub gain_map_max: Rational,
    /// `γ` (5.2.5.6), > 0.
    pub gamma: Rational,
    /// `k_baseline` (5.2.5.4).
    pub base_offset: Rational,
    /// `k_alternate` (5.2.5.5).
    pub alternate_offset: Rational,
}

/// `GainMapMetadata` (C.2.2), the body of a `tmap` item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GainMapMetadata {
    /// `minimum_version` — 0 for this edition.
    pub minimum_version: u16,
    /// `writer_version` (≥ `minimum_version`).
    pub writer_version: u16,
    /// `is_multichannel`: three per-channel entries (R, G, B) rather
    /// than one shared entry.
    pub is_multichannel: bool,
    /// `use_base_colour_space`: the gain-map application space takes
    /// the baseline image's primaries (else the alternate's).
    pub use_base_colour_space: bool,
    /// `H_baseline` (5.2.6).
    pub base_hdr_headroom: Rational,
    /// `H_alternate` (5.2.7).
    pub alternate_hdr_headroom: Rational,
    /// One entry, or three in R, G, B order when multichannel.
    pub channels: Vec<GainMapChannel>,
}

fn be32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| HeifError::invalid("gain map metadata truncated"))
}

fn rational(b: &[u8], at: usize, signed: bool) -> Result<Rational> {
    let n = be32(b, at)?;
    let d = be32(b, at + 4)?;
    if d == 0 {
        return Err(HeifError::invalid("gain map metadata: zero denominator"));
    }
    Ok(Rational {
        num: if signed { n as i32 as i64 } else { n as i64 },
        den: d,
    })
}

impl GainMapMetadata {
    /// Parse the C.2 structure; trailing bytes beyond the recognised
    /// fields are ignored (C.2.1). An unknown `minimum_version` is an
    /// error: the reader must fall back to the base image (C.2.3).
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < 4 {
            return Err(HeifError::invalid("gain map metadata truncated"));
        }
        let minimum_version = u16::from_be_bytes([b[0], b[1]]);
        let writer_version = u16::from_be_bytes([b[2], b[3]]);
        if minimum_version != 0 {
            return Err(HeifError::unsupported(format!(
                "gain map metadata minimum_version {minimum_version}"
            )));
        }
        let flags = *b
            .get(4)
            .ok_or_else(|| HeifError::invalid("gain map metadata truncated"))?;
        let is_multichannel = flags & 0x80 != 0;
        let use_base_colour_space = flags & 0x40 != 0;
        let base_hdr_headroom = rational(b, 5, false)?;
        let alternate_hdr_headroom = rational(b, 13, false)?;
        let count = if is_multichannel { 3 } else { 1 };
        let mut channels = Vec::with_capacity(count);
        let mut at = 21;
        for _ in 0..count {
            let c = GainMapChannel {
                gain_map_min: rational(b, at, true)?,
                gain_map_max: rational(b, at + 8, true)?,
                gamma: rational(b, at + 16, false)?,
                base_offset: rational(b, at + 24, true)?,
                alternate_offset: rational(b, at + 32, true)?,
            };
            if c.gamma.num == 0 {
                return Err(HeifError::invalid("gain map metadata: zero gamma"));
            }
            if c.gain_map_max.value() < c.gain_map_min.value() {
                return Err(HeifError::invalid("gain map metadata: max(G) below min(G)"));
            }
            channels.push(c);
            at += 40;
        }
        Ok(Self {
            minimum_version,
            writer_version,
            is_multichannel,
            use_base_colour_space,
            base_hdr_headroom,
            alternate_hdr_headroom,
            channels,
        })
    }

    /// Parse the body of a `tmap` item — the `ToneMapImage` of
    /// ISO/IEC 23008-12:2025/Amd 1 §6.6.2.4.2: one `version` byte that
    /// shall be 0, then the C.2 `GainMapMetadata` to the end of the
    /// item. "Readers shall not process a ToneMapImage with an
    /// unrecognized version number" (§6.6.2.4.3): any other version is
    /// `Unsupported`.
    pub fn parse_tmap_body(b: &[u8]) -> Result<Self> {
        match b.first() {
            Some(0) => Self::parse(&b[1..]),
            Some(v) => Err(HeifError::unsupported(format!("ToneMapImage version {v}"))),
            None => Err(HeifError::invalid("empty tmap item body")),
        }
    }

    /// Serialise as a `tmap` item body (version prefix + C.2).
    pub fn serialize_tmap_body(&self) -> Vec<u8> {
        let mut v = vec![0u8];
        v.extend_from_slice(&self.serialize());
        v
    }

    /// Serialise to the C.2 layout.
    pub fn serialize(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(21 + 40 * self.channels.len());
        v.extend_from_slice(&self.minimum_version.to_be_bytes());
        v.extend_from_slice(&self.writer_version.to_be_bytes());
        v.push(((self.is_multichannel as u8) << 7) | ((self.use_base_colour_space as u8) << 6));
        let put = |v: &mut Vec<u8>, r: &Rational| {
            v.extend_from_slice(&(r.num as i32 as u32).to_be_bytes());
            v.extend_from_slice(&r.den.to_be_bytes());
        };
        put(&mut v, &self.base_hdr_headroom);
        put(&mut v, &self.alternate_hdr_headroom);
        let n = if self.is_multichannel { 3 } else { 1 };
        for c in self.channels.iter().take(n) {
            put(&mut v, &c.gain_map_min);
            put(&mut v, &c.gain_map_max);
            put(&mut v, &c.gamma);
            put(&mut v, &c.base_offset);
            put(&mut v, &c.alternate_offset);
        }
        v
    }

    /// The per-channel entry for RGB channel `c` (5.2.5.1: a single
    /// entry applies to all three).
    pub fn channel(&self, c: usize) -> &GainMapChannel {
        if self.is_multichannel && self.channels.len() >= 3 {
            &self.channels[c]
        } else {
            &self.channels[0]
        }
    }

    /// The weighting factor `W` for a target headroom (Formula 3):
    /// `sign(H_alt − H_base) · clamp((H_target − H_base) / (H_alt −
    /// H_base), 0, 1)`; 0 when the two headrooms coincide (5.2.7
    /// forbids that, so the gain map is then left unapplied).
    pub fn weight(&self, h_target: f64) -> f64 {
        let hb = self.base_hdr_headroom.value();
        let ha = self.alternate_hdr_headroom.value();
        let span = ha - hb;
        if span == 0.0 {
            return 0.0;
        }
        span.signum() * ((h_target - hb) / span).clamp(0.0, 1.0)
    }
}

// ── H.273 transfer characteristics ──────────────────────────────────

/// Linear light from an encoded value in `[0, 1]` for an H.273
/// `transfer_characteristics` code point (Table 3 inverted). Supported:
/// 1 / 6 / 14 / 15 (BT.709 / 601 / 2020 curve), 8 (linear), 13 (sRGB),
/// 16 (PQ, `Lo = 1` at 10 000 cd/m²), 18 (HLG, nominal range 0..1);
/// 2 (unspecified) is read as sRGB, the MIAF default.
pub fn transfer_to_linear(v: f64, transfer: u16) -> Result<f64> {
    Ok(match transfer {
        1 | 6 | 14 | 15 => {
            const A: f64 = 1.099_296_826_809_442;
            const B: f64 = 0.018_053_968_510_807;
            if v < 4.5 * B {
                v / 4.5
            } else {
                ((v + (A - 1.0)) / A).powf(1.0 / 0.45)
            }
        }
        8 => v,
        2 | 13 => {
            const A: f64 = 1.055;
            const B: f64 = 0.003_130_8;
            if v < 12.92 * B {
                v / 12.92
            } else {
                ((v + (A - 1.0)) / A).powf(2.4)
            }
        }
        16 => {
            const C1: f64 = 0.835_937_5;
            const C2: f64 = 18.851_562_5;
            const C3: f64 = 18.687_5;
            const M: f64 = 78.843_75;
            const N: f64 = 0.159_301_757_812_5;
            let p = v.max(0.0).powf(1.0 / M);
            ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / N)
        }
        18 => {
            const A: f64 = 0.178_832_77;
            const B: f64 = 0.284_668_92;
            const C: f64 = 0.559_910_73;
            if v <= 0.5 {
                v * v / 3.0
            } else {
                (((v - C) / A).exp() + B) / 12.0
            }
        }
        other => {
            return Err(HeifError::unsupported(format!(
                "transfer_characteristics {other} linearisation"
            )))
        }
    })
}

/// Encoded value from linear light (H.273 Table 3, same code points
/// as [`transfer_to_linear`]); input outside `[0, 1]` is clipped for
/// the SDR curves, PQ / HLG take their nominal ranges.
pub fn linear_to_transfer(l: f64, transfer: u16) -> Result<f64> {
    Ok(match transfer {
        1 | 6 | 14 | 15 => {
            const A: f64 = 1.099_296_826_809_442;
            const B: f64 = 0.018_053_968_510_807;
            let l = l.clamp(0.0, 1.0);
            if l < B {
                4.5 * l
            } else {
                A * l.powf(0.45) - (A - 1.0)
            }
        }
        8 => l.clamp(0.0, 1.0),
        2 | 13 => {
            const A: f64 = 1.055;
            const B: f64 = 0.003_130_8;
            let l = l.clamp(0.0, 1.0);
            if l < B {
                12.92 * l
            } else {
                A * l.powf(1.0 / 2.4) - (A - 1.0)
            }
        }
        16 => {
            const C1: f64 = 0.835_937_5;
            const C2: f64 = 18.851_562_5;
            const C3: f64 = 18.687_5;
            const M: f64 = 78.843_75;
            const N: f64 = 0.159_301_757_812_5;
            let ln = l.clamp(0.0, 1.0).powf(N);
            ((C1 + C2 * ln) / (1.0 + C3 * ln)).powf(M)
        }
        18 => {
            const A: f64 = 0.178_832_77;
            const B: f64 = 0.284_668_92;
            const C: f64 = 0.559_910_73;
            let l = l.clamp(0.0, 1.0);
            if l <= 1.0 / 12.0 {
                (3.0 * l).sqrt()
            } else {
                A * (12.0 * l - B).ln() + C
            }
        }
        other => {
            return Err(HeifError::unsupported(format!(
                "transfer_characteristics {other} encoding"
            )))
        }
    })
}

// ── H.273 colour primaries ──────────────────────────────────────────

/// `(x, y)` chromaticities of `(red, green, blue, white)` for an H.273
/// `colour_primaries` code point (Table 2); 2 (unspecified) is read as
/// BT.709, the MIAF default.
pub fn primaries_chromaticities(primaries: u16) -> Result<[(f64, f64); 4]> {
    Ok(match primaries {
        1 | 2 => [
            (0.640, 0.330),
            (0.300, 0.600),
            (0.150, 0.060),
            (0.3127, 0.3290),
        ],
        9 => [
            (0.708, 0.292),
            (0.170, 0.797),
            (0.131, 0.046),
            (0.3127, 0.3290),
        ],
        other => {
            return Err(HeifError::unsupported(format!(
                "colour_primaries {other} conversion"
            )))
        }
    })
}

type Mat3 = [[f64; 3]; 3];

fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

fn mat_inv(m: &Mat3) -> Mat3 {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    let c = |r: usize, cidx: usize| -> f64 {
        let (r1, r2) = ((r + 1) % 3, (r + 2) % 3);
        let (c1, c2) = ((cidx + 1) % 3, (cidx + 2) % 3);
        m[r1][c1] * m[r2][c2] - m[r1][c2] * m[r2][c1]
    };
    let mut inv = [[0.0; 3]; 3];
    for (i, row) in inv.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = c(j, i) / det;
        }
    }
    inv
}

/// RGB → XYZ matrix for a set of primaries (white point normalised to
/// `Y = 1`): the standard derivation from chromaticities.
fn rgb_to_xyz(primaries: u16) -> Result<Mat3> {
    let [r, g, b, w] = primaries_chromaticities(primaries)?;
    let col = |(x, y): (f64, f64)| [x / y, 1.0, (1.0 - x - y) / y];
    let (cr, cg, cb, cw) = (col(r), col(g), col(b), col(w));
    let p: Mat3 = [
        [cr[0], cg[0], cb[0]],
        [cr[1], cg[1], cb[1]],
        [cr[2], cg[2], cb[2]],
    ];
    let inv = mat_inv(&p);
    let s: Vec<f64> = (0..3)
        .map(|i| (0..3).map(|k| inv[i][k] * cw[k]).sum())
        .collect();
    Ok([
        [cr[0] * s[0], cg[0] * s[1], cb[0] * s[2]],
        [cr[1] * s[0], cg[1] * s[1], cb[1] * s[2]],
        [cr[2] * s[0], cg[2] * s[1], cb[2] * s[2]],
    ])
}

/// Linear RGB conversion matrix from `from` primaries to `to`
/// primaries (identity when they coincide).
pub fn primaries_conversion(from: u16, to: u16) -> Result<Mat3> {
    let norm = |p: u16| if p == 2 { 1 } else { p };
    if norm(from) == norm(to) {
        return Ok([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
    }
    Ok(mat_mul(&mat_inv(&rgb_to_xyz(to)?), &rgb_to_xyz(from)?))
}

// ── application ─────────────────────────────────────────────────────

/// A linear-light RGB picture (three `f32` per pixel, row-major) in a
/// given set of primaries; values may exceed 1 (HDR headroom).
#[derive(Clone, Debug, PartialEq)]
pub struct LinearRgbImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// H.273 `colour_primaries` the samples are expressed in.
    pub primaries: u16,
    /// `width × height × 3` samples.
    pub data: Vec<f32>,
}

impl LinearRgbImage {
    /// One sample.
    #[inline]
    pub fn sample(&self, x: u32, y: u32, c: usize) -> f32 {
        self.data[(y as usize * self.width as usize + x as usize) * 3 + c]
    }

    /// Re-encode with an H.273 `transfer` in `primaries`, quantised to
    /// `bit_depth` (values above 1.0 clip: SDR display of an HDR
    /// rendition). Returns an interleaved RGB [`RgbImage`](crate::rgb::RgbImage).
    pub fn encode(
        &self,
        primaries: u16,
        transfer: u16,
        bit_depth: u8,
    ) -> Result<crate::rgb::RgbImage> {
        let m = primaries_conversion(self.primaries, primaries)?;
        let max = ((1u32 << bit_depth) - 1) as f64;
        let mut data = Vec::with_capacity(self.data.len());
        for px in self.data.chunks_exact(3) {
            let rgb = [px[0] as f64, px[1] as f64, px[2] as f64];
            for row in &m {
                let l = row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2];
                let v = linear_to_transfer(l, transfer)?;
                data.push((v * max).round().clamp(0.0, max) as u16);
            }
        }
        Ok(crate::rgb::RgbImage {
            width: self.width,
            height: self.height,
            channels: 3,
            bit_depth,
            data,
        })
    }
}

/// The colour code points of a `colr`, with the MIAF defaults for an
/// absent / ICC / unspecified one.
fn cicp(colr: Option<&Colr>) -> (u16, u16) {
    match colr {
        Some(Colr::Nclx {
            primaries,
            transfer,
            ..
        }) => (if *primaries == 2 { 1 } else { *primaries }, *transfer),
        _ => (1, 13),
    }
}

/// Unnormalised, resampled gain map: `width × height × channels` log₂
/// values (Formula 1 applied per stored channel, bilinear resampling
/// to the base geometry when the sizes differ).
fn unnormalised_gain(
    gain: &HeifFrame,
    gain_colr: Option<&Colr>,
    meta: &GainMapMetadata,
    width: u32,
    height: u32,
) -> Result<(Vec<f64>, usize)> {
    // Stored samples as [0, 1] per channel through the gain map's own
    // `colr` matrix and range (HEIF Amd 1 §6.6.2.4.1: a limited-range
    // map is clipped to 0.0..1.0 after the matrix / range are
    // applied — `to_rgb` clamps to the code range): the grey level of
    // a monochrome map, the RGB of a colour one.
    let max = ((1u32 << gain.format.bit_depth) - 1) as f64;
    let rgb = to_rgb(&gain.without_alpha(), gain_colr)?;
    let stored_channels = if gain.format.chroma == Chroma::Mono {
        1
    } else {
        3
    };
    let mut stored = Vec::with_capacity(rgb.data.len() / rgb.channels * stored_channels);
    for px in rgb.data.chunks_exact(rgb.channels) {
        stored.extend(px[..stored_channels].iter().map(|s| *s as f64 / max));
    }
    // §6.6.2.4.1: a single-channel map with multi-channel metadata is
    // treated as three identical colour channels (each with its own
    // metadata); a multi-channel map with single-channel metadata uses
    // that one entry for every channel (`meta.channel`).
    let channels = if stored_channels == 3 || meta.is_multichannel {
        3
    } else {
        1
    };
    // Formula 1 per channel.
    let n = stored.len() / stored_channels;
    let mut log2 = Vec::with_capacity(n * channels);
    for i in 0..n {
        for c in 0..channels {
            let s = stored[i * stored_channels + if stored_channels == 3 { c } else { 0 }];
            let ch = meta.channel(c);
            let (mn, mx, gamma) = (
                ch.gain_map_min.value(),
                ch.gain_map_max.value(),
                ch.gamma.value(),
            );
            log2.push((mx - mn) * s.max(0.0).powf(1.0 / gamma) + mn);
        }
    }
    if (gain.width, gain.height) == (width, height) {
        return Ok((log2, channels));
    }
    // §6.2.2: resample after unnormalising; bilinear between source
    // sample centres.
    let (gw, gh) = (gain.width as usize, gain.height as usize);
    let mut out = Vec::with_capacity(width as usize * height as usize * channels);
    for y in 0..height {
        let sy = ((y as f64 + 0.5) * gh as f64 / height as f64 - 0.5).clamp(0.0, (gh - 1) as f64);
        let (y0, fy) = (sy.floor() as usize, sy - sy.floor());
        let y1 = (y0 + 1).min(gh - 1);
        for x in 0..width {
            let sx =
                ((x as f64 + 0.5) * gw as f64 / width as f64 - 0.5).clamp(0.0, (gw - 1) as f64);
            let (x0, fx) = (sx.floor() as usize, sx - sx.floor());
            let x1 = (x0 + 1).min(gw - 1);
            for c in 0..channels {
                let at = |xx: usize, yy: usize| log2[(yy * gw + xx) * channels + c];
                let top = at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx;
                let bot = at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx;
                out.push(top * (1.0 - fy) + bot * fy);
            }
        }
    }
    Ok((out, channels))
}

/// Apply a gain map (§6): `base` is the baseline output image with its
/// `colr`, `gain` the decoded gain-map item with its own `colr`,
/// `alternate_colr` the alternate image's colour information (the
/// `tmap` item's `colr`; the base's when absent, 5.3.3), `h_target`
/// the HDR headroom to render for (log₂ of the display's HDR / SDR
/// white ratio; `H_baseline` leaves the base untouched, `H_alternate`
/// applies the map fully). Returns linear RGB in the gain-map
/// application space (Annex B).
pub fn apply_gain_map(
    base: &HeifFrame,
    base_colr: Option<&Colr>,
    gain: &HeifFrame,
    gain_colr: Option<&Colr>,
    alternate_colr: Option<&Colr>,
    meta: &GainMapMetadata,
    h_target: f64,
) -> Result<LinearRgbImage> {
    base.validate()?;
    gain.validate()?;
    let (base_primaries, base_transfer) = cicp(base_colr);
    let (alt_primaries, _) = match alternate_colr {
        Some(c) => cicp(Some(c)),
        None => (base_primaries, base_transfer),
    };
    let app_primaries = if meta.use_base_colour_space {
        base_primaries
    } else {
        alt_primaries
    };
    let to_app = primaries_conversion(base_primaries, app_primaries)?;
    let (log2, gchan) = unnormalised_gain(gain, gain_colr, meta, base.width, base.height)?;
    let w = meta.weight(h_target);
    // Linearise the base through its transfer (the exact YCbCr→RGB of
    // this crate, then the OETF inverse per sample, cached per code).
    let rgb = to_rgb(&base.without_alpha(), base_colr)?;
    let max = ((1u32 << rgb.bit_depth) - 1) as f64;
    let mut lut = Vec::with_capacity(max as usize + 1);
    for i in 0..=(max as usize) {
        lut.push(transfer_to_linear(i as f64 / max, base_transfer)?);
    }
    let n = base.width as usize * base.height as usize;
    let mut data = Vec::with_capacity(n * 3);
    for (i, px) in rgb.data.chunks_exact(rgb.channels).enumerate() {
        let lin = [
            lut[px[0] as usize],
            lut[px[1] as usize],
            lut[px[2] as usize],
        ];
        for (c, row) in to_app.iter().enumerate() {
            let b = row[0] * lin[0] + row[1] * lin[1] + row[2] * lin[2];
            let g = log2[i * gchan + if gchan == 3 { c } else { 0 }];
            let ch = meta.channel(c);
            let alt = (b + ch.base_offset.value()) * (w * g).exp2() - ch.alternate_offset.value();
            data.push(alt as f32);
        }
    }
    Ok(LinearRgbImage {
        width: base.width,
        height: base.height,
        primaries: app_primaries,
        data,
    })
}

/// HDR reference white assumed when a reconstructed alternate is
/// PQ-coded: ISO 21496-1 scales the application space so the HDR
/// reference white is 1.0 (B.2) but fixes no absolute luminance for
/// it; 203 cd/m² is the value its definition of HDR headroom uses as
/// the example (3.6, Note 1). Override with
/// [`crate::decode::ItemDecoder::with_reference_white`].
pub const DEFAULT_HDR_REFERENCE_WHITE_NITS: f64 = 203.0;

/// Factor from application-space linear (HDR reference white = 1.0)
/// to the input domain of an H.273 transfer: PQ (16) is absolute —
/// 1.0 = 10 000 cd/m² — so the reference white lands at
/// `reference_white_nits / 10 000`; every relative curve puts its
/// peak signal at the alternate's nominal peak luminance, which is
/// `2^H_alternate` × the reference white (21496-1 3.6 / 3.10).
pub fn alternate_signal_scale(
    transfer: u16,
    meta: &GainMapMetadata,
    reference_white_nits: f64,
) -> f64 {
    match transfer {
        16 => reference_white_nits / 10_000.0,
        _ => (-meta.alternate_hdr_headroom.value()).exp2(),
    }
}

/// The normative reconstruction of a `tmap` derived image item (HEIF
/// Amd 1 §6.6.2.4.1: "Reconstruction is done by applying the gain map
/// to the base image according to ISO 21496-1 section 6"): the map
/// applied at the alternate headroom, re-encoded in the `tmap` item's
/// own `colr` (`alternate_colr`: primaries + transfer + matrix +
/// range) as a 4:4:4 (monochrome for a grey base) frame at
/// `bit_depth`, with [`alternate_signal_scale`] placing the reference
/// white (`reference_white_nits` for PQ). Only colour is tone-mapped;
/// the base's alpha plane is carried over unchanged (4th-ed. WD
/// §6.6.2.4.1).
#[allow(clippy::too_many_arguments)]
pub fn reconstruct_tone_map(
    base: &HeifFrame,
    base_colr: Option<&Colr>,
    gain: &HeifFrame,
    gain_colr: Option<&Colr>,
    alternate_colr: Option<&Colr>,
    meta: &GainMapMetadata,
    bit_depth: u8,
    reference_white_nits: f64,
) -> Result<HeifFrame> {
    let h = meta.alternate_hdr_headroom.value();
    let mut lin = apply_gain_map(base, base_colr, gain, gain_colr, alternate_colr, meta, h)?;
    let alt = alternate_colr.or(base_colr);
    let (primaries, transfer) = cicp(alt);
    let scale = alternate_signal_scale(transfer, meta, reference_white_nits) as f32;
    for v in lin.data.iter_mut() {
        *v *= scale;
    }
    let rgb = lin.encode(primaries, transfer, bit_depth)?;
    let chroma = if base.format.chroma == Chroma::Mono && gain.format.chroma == Chroma::Mono {
        Chroma::Mono
    } else {
        Chroma::Yuv444
    };
    let mut out = crate::rgb::from_rgb(&rgb, alt, chroma)?;
    if let Some(alpha) = base.alpha_as_frame() {
        let alpha = if alpha.format.bit_depth == bit_depth {
            alpha
        } else {
            crate::compose::rescale_depth(&alpha, bit_depth)?
        };
        out = out.with_alpha_plane(&alpha)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::HeifPixelFormat;

    fn r(num: i64, den: u32) -> Rational {
        Rational { num, den }
    }

    fn meta(gamma: Rational, multi: bool) -> GainMapMetadata {
        let ch = GainMapChannel {
            gain_map_min: r(-1, 1),
            gain_map_max: r(3, 1),
            gamma,
            base_offset: r(1, 64),
            alternate_offset: r(1, 64),
        };
        GainMapMetadata {
            minimum_version: 0,
            writer_version: 0,
            is_multichannel: multi,
            use_base_colour_space: true,
            base_hdr_headroom: r(0, 1),
            alternate_hdr_headroom: r(2, 1),
            channels: if multi { vec![ch; 3] } else { vec![ch] },
        }
    }

    #[test]
    fn metadata_round_trips_and_rejects_bad_fields() {
        let m = meta(r(3, 2), true);
        let bytes = m.serialize();
        assert_eq!(bytes.len(), 21 + 3 * 40);
        assert_eq!(GainMapMetadata::parse(&bytes).unwrap(), m);
        let single = meta(r(1, 1), false);
        let b = single.serialize();
        assert_eq!(b.len(), 61);
        // Trailing bytes are ignored (C.2.1).
        let mut padded = b.clone();
        padded.extend_from_slice(&[9; 7]);
        assert_eq!(GainMapMetadata::parse(&padded).unwrap(), single);
        assert!(GainMapMetadata::parse(&b[..40]).is_err());
        let mut v1 = b.clone();
        v1[1] = 1;
        assert!(GainMapMetadata::parse(&v1).is_err(), "minimum_version 1");
        let mut zero_den = b.clone();
        zero_den[9..13].copy_from_slice(&0u32.to_be_bytes());
        assert!(GainMapMetadata::parse(&zero_den).is_err());
    }

    #[test]
    fn weight_follows_formula_3() {
        let m = meta(r(1, 1), false);
        assert_eq!(m.weight(0.0), 0.0);
        assert_eq!(m.weight(1.0), 0.5);
        assert_eq!(m.weight(2.0), 1.0);
        assert_eq!(m.weight(5.0), 1.0);
        assert_eq!(m.weight(-1.0), 0.0);
        let mut inverted = m.clone();
        inverted.base_hdr_headroom = r(2, 1);
        inverted.alternate_hdr_headroom = r(0, 1);
        assert_eq!(inverted.weight(1.0), -0.5);
        let mut same = m;
        same.alternate_hdr_headroom = r(0, 1);
        assert_eq!(same.weight(1.0), 0.0);
    }

    #[test]
    fn transfers_invert() {
        for t in [1u16, 8, 13, 16, 18] {
            for i in 0..=64 {
                let l = i as f64 / 64.0;
                let v = linear_to_transfer(l, t).unwrap();
                let back = transfer_to_linear(v, t).unwrap();
                assert!((back - l).abs() < 1e-6, "transfer {t} at {l}: {back}");
            }
        }
        assert!(transfer_to_linear(0.5, 17).is_err());
        // sRGB anchor: 0.5 encoded ≈ 0.2140 linear.
        assert!((transfer_to_linear(0.5, 13).unwrap() - 0.214_04).abs() < 1e-4);
    }

    #[test]
    fn primaries_matrices_are_consistent() {
        let m = rgb_to_xyz(1).unwrap();
        // White maps to Y = 1 with D65 chromaticity.
        let y: f64 = m[1].iter().sum();
        assert!((y - 1.0).abs() < 1e-9);
        let x: f64 = m[0].iter().sum();
        let z: f64 = m[2].iter().sum();
        assert!((x / (x + y + z) - 0.3127).abs() < 1e-4);
        let a = primaries_conversion(1, 9).unwrap();
        let b = primaries_conversion(9, 1).unwrap();
        let id = mat_mul(&a, &b);
        for (i, row) in id.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                assert!((v - if i == j { 1.0 } else { 0.0 }).abs() < 1e-9);
            }
        }
        // BT.709 white is BT.2020 white.
        let w: Vec<f64> = (0..3).map(|i| a[i].iter().sum()).collect();
        assert!(w.iter().all(|v| (v - 1.0).abs() < 1e-9));
    }

    /// Build a gain map from a synthetic base / alternate pair with the
    /// informative Annex A recipe (A.1 log-ratio, A.3 normalise + gamma),
    /// then apply it and recover the alternate — at full headroom
    /// exactly (to quantisation), at the base headroom the base itself.
    #[test]
    fn synthetic_pair_round_trips_through_annex_a() {
        let (w, h) = (16u32, 8u32);
        let fmt = HeifPixelFormat::new(Chroma::Yuv444, 8, false).unwrap();
        let colr = Colr::Nclx {
            primaries: 1,
            transfer: 8, // linear coding keeps the test exact
            matrix: 0,   // identity: planes are G, B, R
            full_range: true,
        };
        let m = meta(r(2, 1), false); // gamma 2, min −1, max 3 stops
        let (kb, ka) = (1.0 / 64.0, 1.0 / 64.0);
        // Base: a ramp; gain: chosen per pixel in [−1, 3] stops.
        let mut base = HeifFrame::zeroed(w, h, fmt).unwrap();
        let mut gain =
            HeifFrame::zeroed(w, h, HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap()).unwrap();
        let mut want_alt = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let b = (x as f64 + 1.0) / (w as f64 + 1.0);
                let g = -1.0 + 4.0 * (y as f64 / (h - 1) as f64); // stops
                let bq = (b * 255.0).round();
                base.set_sample(0, x, y, bq as u16);
                base.set_sample(1, x, y, bq as u16);
                base.set_sample(2, x, y, bq as u16);
                // A.3: normalise then gamma, quantise to 8 bits.
                let gn = ((g + 1.0) / 4.0).powf(2.0);
                let gq = (gn * 255.0).round();
                gain.set_sample(0, x, y, gq as u16);
                // Expected alternate from the quantised values (Formula 2).
                let g_back = 4.0 * (gq / 255.0).powf(0.5) - 1.0;
                want_alt.push((bq / 255.0 + kb) * g_back.exp2() - ka);
            }
        }
        let full = apply_gain_map(&base, Some(&colr), &gain, None, None, &m, 2.0).unwrap();
        assert_eq!(full.primaries, 1);
        for (i, want) in want_alt.iter().enumerate() {
            for c in 0..3 {
                let got = full.data[i * 3 + c] as f64;
                assert!((got - want).abs() < 1e-4, "px {i} ch {c}: {got} vs {want}");
            }
        }
        let none = apply_gain_map(&base, Some(&colr), &gain, None, None, &m, 0.0).unwrap();
        for (i, px) in none.data.chunks_exact(3).enumerate() {
            let b = base.sample(0, i as u32 % w, i as u32 / w) as f64 / 255.0;
            assert!((px[0] as f64 - b).abs() < 1e-6);
        }
        // Half way: W = 0.5 → half the stops.
        let half = apply_gain_map(&base, Some(&colr), &gain, None, None, &m, 1.0).unwrap();
        let i = (3 * w + 5) as usize;
        let b = base.sample(0, 5, 3) as f64 / 255.0;
        let gq = gain.sample(0, 5, 3) as f64;
        let g_back = 4.0 * (gq / 255.0).powf(0.5) - 1.0;
        let want = (b + kb) * (0.5 * g_back).exp2() - ka;
        assert!((half.data[i * 3] as f64 - want).abs() < 1e-4);
        // Encoding back to 8-bit linear reproduces the clipped values.
        let enc = full.encode(1, 8, 8).unwrap();
        assert_eq!(enc.channels, 3);
        assert_eq!(
            enc.sample(0, 0, 0),
            (want_alt[0].clamp(0.0, 1.0) * 255.0).round() as u16
        );
    }

    #[test]
    fn half_size_gain_map_is_resampled_bilinearly() {
        let fmt = HeifPixelFormat::new(Chroma::Yuv444, 8, false).unwrap();
        let base = HeifFrame::filled(4, 2, fmt, 128).unwrap();
        // 2x1 gain map: 0 stops on the left, 1 stop on the right
        // (min 0, max 1, gamma 1 → stored 0 and 255).
        let mut gain =
            HeifFrame::zeroed(2, 1, HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap()).unwrap();
        gain.set_sample(0, 1, 0, 255);
        let mut m = meta(r(1, 1), false);
        m.channels[0].gain_map_min = r(0, 1);
        m.channels[0].gain_map_max = r(1, 1);
        m.channels[0].base_offset = r(0, 1);
        m.channels[0].alternate_offset = r(0, 1);
        let colr = Colr::Nclx {
            primaries: 1,
            transfer: 8,
            matrix: 0,
            full_range: true,
        };
        let out = apply_gain_map(&base, Some(&colr), &gain, None, None, &m, 2.0).unwrap();
        let b = 128.0 / 255.0;
        // Columns 0 and 3 sit on the source centres (0 and 1 stop);
        // 1 and 2 interpolate at ¼ and ¾ stop.
        let stops = |x: u32| (out.sample(x, 0, 0) as f64 / b).log2();
        assert!((stops(0)).abs() < 1e-5);
        assert!((stops(3) - 1.0).abs() < 1e-5);
        assert!((stops(1) - 0.25).abs() < 1e-5);
        assert!((stops(2) - 0.75).abs() < 1e-5);
    }
}
