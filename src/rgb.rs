//! YCbCr → RGB conversion of a composed [`HeifFrame`] (H.273 §8.3
//! matrix coefficients, full / limited range), the last step a renderer
//! needs after [`decode_primary`](crate::decode::decode_primary) in the
//! `registry` build or after composing an externally decoded item in
//! the standalone build.
//!
//! The conversion is exact per sample (no dithering, round-to-nearest);
//! sub-sampled chroma is replicated to its co-sited luma samples
//! (chroma sample position 0 / "left-sited" in H.273 Figure 1 terms
//! puts the chroma sample at the left luma column of each pair, so
//! nearest replication is exact there for even columns). Monochrome
//! becomes grey; an alpha plane is carried through untouched.

use crate::error::{HeifError, Result};
use crate::image::{Chroma, HeifFrame, HeifPixelFormat, Plane};
use crate::props::Colr;

/// An interleaved RGB / RGBA picture, one `u16` per sample holding
/// `bit_depth` significant bits.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RgbImage16 {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// 3 (RGB) or 4 (RGBA).
    pub channels: usize,
    /// Bits per sample (8..=16), the source frame's depth.
    pub bit_depth: u8,
    /// `width × height × channels` samples, row-major.
    pub data: Vec<u16>,
}
impl RgbImage16 {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(width: u32, height: u32, channels: usize, bit_depth: u8, data: Vec<u16>) -> Self {
        Self {
            width,
            height,
            channels,
            bit_depth,
            data,
        }
    }
}

impl RgbImage16 {
    /// One sample.
    #[inline]
    pub fn sample(&self, x: u32, y: u32, c: usize) -> u16 {
        self.data[(y as usize * self.width as usize + x as usize) * self.channels + c]
    }

    /// The samples as bytes: one byte per sample at 8 bits, big-endian
    /// words above (PNG / TIFF order).
    pub fn to_be_bytes(&self) -> Vec<u8> {
        if self.bit_depth <= 8 {
            self.data.iter().map(|v| *v as u8).collect()
        } else {
            self.data.iter().flat_map(|v| v.to_be_bytes()).collect()
        }
    }
}

/// `(Kr, Kb)` of a `matrix_coefficients` code point (H.273 Table 4);
/// `None` for the identity matrix (0) and for the constant-luminance /
/// chromaticity-derived / ICtCp variants this crate does not convert.
pub fn matrix_kr_kb(matrix: u16) -> Option<(f64, f64)> {
    Some(match matrix {
        1 => (0.2126, 0.0722),
        4 => (0.30, 0.11),
        // 2 = unspecified: readers assume BT.601 (the MIAF default).
        2 | 5 | 6 => (0.299, 0.114),
        7 => (0.212, 0.087),
        9 => (0.2627, 0.0593),
        _ => return None,
    })
}

/// Convert `frame` to RGB(A) using the matrix / range of `colr`
/// (`nclx`; an ICC or absent `colr` falls back to the MIAF default:
/// BT.601 matrix, full range). Samples keep the frame's bit depth.
pub fn to_rgb(frame: &HeifFrame, colr: Option<&Colr>) -> Result<RgbImage16> {
    frame.validate()?;
    let depth = frame.format.bit_depth;
    let max = ((1u32 << depth) - 1) as f64;
    let has_alpha = frame.format.has_alpha;
    let channels = 3 + has_alpha as usize;
    let data = convert(
        frame.width,
        frame.height,
        frame.format,
        &frame.planes,
        colr,
        channels,
        |v: f64| (v * max).round().clamp(0.0, max) as u16,
        |a: u16| a,
        0u16,
        1,
    )?;
    Ok(RgbImage16 {
        width: frame.width,
        height: frame.height,
        channels,
        bit_depth: depth,
        data,
    })
}

/// Convert planes of `layout` to tightly packed 8-bit RGB (`alpha`
/// false) or RGBA (`alpha` true): the same kernel as [`to_rgb`], the
/// normalised result quantised to 8 bits (round to nearest), the alpha
/// plane scaled to 8 bits and opaque (255) when the layout has none.
pub fn planes_to_rgb8(
    width: u32,
    height: u32,
    layout: HeifPixelFormat,
    planes: &[Plane],
    colr: Option<&Colr>,
    alpha: bool,
) -> Result<Vec<u8>> {
    planes_to_rgb8_threads(width, height, layout, planes, colr, alpha, 1)
}

/// [`planes_to_rgb8`] on up to `threads` workers (bands of rows, one
/// per worker once the picture is large enough to share; the same
/// bytes for every budget).
pub fn planes_to_rgb8_threads(
    width: u32,
    height: u32,
    layout: HeifPixelFormat,
    planes: &[Plane],
    colr: Option<&Colr>,
    alpha: bool,
    threads: usize,
) -> Result<Vec<u8>> {
    let src_max = layout.max_value() as f64;
    convert(
        width,
        height,
        layout,
        planes,
        colr,
        3 + alpha as usize,
        |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8,
        |a: u16| (a as f64 / src_max * 255.0).round().clamp(0.0, 255.0) as u8,
        255u8,
        threads.max(1),
    )
}

/// Pictures smaller than this are converted sample by sample
/// ([`for_each_pixel`]); larger ones through per-sample-value tables.
const TABLE_MIN_PIXELS: usize = 1 << 12;

/// A conversion worker takes at least this many pixels.
const ROWS_PIXELS_PER_WORKER: usize = 1 << 19;

/// 8-bit pictures of at least this many pixels also get the two
/// `(luma, chroma)` tables of the red and blue channels.
const PAIR_TABLE_MIN_PIXELS: usize = 1 << 18;

/// One sample of a plane row: a byte, or a little-endian word.
#[inline(always)]
fn sample_at<const WIDE: bool>(row: &[u8], x: usize) -> usize {
    if WIDE {
        u16::from_le_bytes([row[2 * x], row[2 * x + 1]]) as usize
    } else {
        row[x] as usize
    }
}

/// The conversion behind [`to_rgb`] and [`planes_to_rgb8`]: `channels`
/// (3 or 4) output samples per pixel, `quant` mapping a normalised
/// colour value to an output sample, `alpha_of` a raw alpha sample
/// (`opaque` when the layout has no alpha plane).
///
/// The arithmetic is [`for_each_pixel`]'s, operation for operation, so
/// the output is the same bit for bit; what differs is where it is
/// done. Every term that depends on one sample only — the scaled luma,
/// the red-difference and blue-difference contributions, a quantised
/// grey level or alpha — is computed once per sample *value* into a
/// table (2^depth entries, depths up to 12), and for 8-bit pictures of
/// some size the quantised red and blue of every (luma, chroma) pair
/// as well; rows are walked through slices rather than per-sample
/// offsets. Only green, which depends on all three samples, is still
/// evaluated per pixel.
#[allow(clippy::too_many_arguments)]
fn convert<T: Copy + Send + Sync>(
    width: u32,
    height: u32,
    layout: HeifPixelFormat,
    planes: &[Plane],
    colr: Option<&Colr>,
    channels: usize,
    quant: impl Fn(f64) -> T + Copy + Sync,
    alpha_of: impl Fn(u16) -> T + Copy + Sync,
    opaque: T,
    threads: usize,
) -> Result<Vec<T>> {
    let (w, h) = (width as usize, height as usize);
    let alpha_plane = layout.alpha_plane().filter(|_| channels == 4);
    if w * h < TABLE_MIN_PIXELS || layout.bit_depth > 12 {
        let mut data = Vec::with_capacity(w * h * channels);
        for_each_pixel(width, height, layout, planes, colr, |rgb, a| {
            data.extend_from_slice(&[quant(rgb[0]), quant(rgb[1]), quant(rgb[2])]);
            if channels == 4 {
                data.push(if alpha_plane.is_some() {
                    alpha_of(a)
                } else {
                    opaque
                });
            }
        })?;
        return Ok(data);
    }
    check_planes(width, height, layout, planes)?;
    if layout.bytes_per_sample() == 2 {
        convert_rows::<T, true>(
            w, h, layout, planes, colr, channels, quant, alpha_of, opaque, threads,
        )
    } else {
        convert_rows::<T, false>(
            w, h, layout, planes, colr, channels, quant, alpha_of, opaque, threads,
        )
    }
}

/// [`convert`]'s table-driven body for one sample width.
#[allow(clippy::too_many_arguments)]
fn convert_rows<T: Copy + Send + Sync, const WIDE: bool>(
    w: usize,
    h: usize,
    layout: HeifPixelFormat,
    planes: &[Plane],
    colr: Option<&Colr>,
    channels: usize,
    quant: impl Fn(f64) -> T + Copy + Sync,
    alpha_of: impl Fn(u16) -> T + Copy + Sync,
    opaque: T,
    threads: usize,
) -> Result<Vec<T>> {
    let (matrix, full_range) = match colr {
        Some(Colr::Nclx {
            matrix, full_range, ..
        }) => (*matrix, *full_range),
        _ => (6, true),
    };
    let depth = layout.bit_depth as u32;
    let max = ((1u32 << depth) - 1) as f64;
    let sh = (1u32 << (depth - 8)) as f64;
    let (y_off, y_scale, c_scale) = if full_range {
        (0.0, max, max)
    } else {
        (16.0 * sh, 219.0 * sh, 224.0 * sh)
    };
    let mid = (1u32 << (depth - 1)) as f64;
    let mono = layout.chroma == Chroma::Mono;
    let kr_kb = if mono || matrix == 0 {
        None
    } else {
        Some(matrix_kr_kb(matrix).ok_or_else(|| {
            HeifError::unsupported(format!("matrix_coefficients {matrix} conversion"))
        })?)
    };
    // A sample above the nominal maximum (a deep plane with stray
    // high bits) is outside the tables: the same expressions, direct.
    let n = 1usize << depth;
    let scaled = |v: usize| (v as f64 - y_off) / y_scale;
    let luma: Vec<f64> = (0..n).map(scaled).collect();
    let level: Vec<T> = luma.iter().map(|v| quant(*v)).collect();
    let level_of = |v: usize| level.get(v).copied().unwrap_or_else(|| quant(scaled(v)));
    let alpha_plane = layout.alpha_plane().filter(|_| channels == 4);
    let alpha_tab: Vec<T> = if alpha_plane.is_some() {
        (0..n).map(|v| alpha_of(v as u16)).collect()
    } else {
        Vec::new()
    };
    let bps = if WIDE { 2 } else { 1 };
    let row_of = |plane: usize, y: usize, samples: usize| -> &[u8] {
        let p = &planes[plane];
        &p.data[y * p.stride..][..samples * bps]
    };
    let mut out = vec![opaque; w * h * channels];
    let (sx, sy) = if mono { (0, 0) } else { layout.chroma.shift() };
    let (cw, _) = layout.plane_dims(1.min(layout.plane_count() - 1), w as u32, h as u32);
    // Per-value terms of the matrix equations (H.273 8.3): the red /
    // blue chroma contributions `2 (1 − K) P`.
    let (red_add, blue_add): (Vec<f64>, Vec<f64>) = match kr_kb {
        Some((kr, kb)) => (
            (0..n)
                .map(|v| 2.0 * (1.0 - kr) * ((v as f64 - mid) / c_scale))
                .collect(),
            (0..n)
                .map(|v| 2.0 * (1.0 - kb) * ((v as f64 - mid) / c_scale))
                .collect(),
        ),
        None => (Vec::new(), Vec::new()),
    };
    // Quantised red / blue by (luma, chroma) pair.
    let pairs = !WIDE && kr_kb.is_some() && w * h >= PAIR_TABLE_MIN_PIXELS;
    let pair_table = |add: &[f64]| -> Vec<T> {
        let mut t = Vec::with_capacity(n * n);
        for y in &luma {
            t.extend(add.iter().map(|a| quant(y + a)));
        }
        t
    };
    let (red_pair, blue_pair) = if pairs {
        (pair_table(&red_add), pair_table(&blue_add))
    } else {
        (Vec::new(), Vec::new())
    };
    // Rows are independent: a band of them per worker.
    let rows = |first: usize, band: &mut [T]| {
        for (k, orow) in band.chunks_exact_mut(w * channels).enumerate() {
            let y = first + k;
            let yrow = row_of(0, y, w);
            match kr_kb {
                None if mono => {
                    for (x, px) in orow.chunks_exact_mut(channels).enumerate() {
                        let g = level_of(sample_at::<WIDE>(yrow, x));
                        px[0] = g;
                        px[1] = g;
                        px[2] = g;
                    }
                }
                None => {
                    // Identity: Y = G, Cb = B, Cr = R (equations 41–43).
                    let (brow, rrow) = (
                        row_of(1, y >> sy, cw as usize),
                        row_of(2, y >> sy, cw as usize),
                    );
                    for (x, px) in orow.chunks_exact_mut(channels).enumerate() {
                        px[0] = level_of(sample_at::<WIDE>(rrow, x >> sx));
                        px[1] = level_of(sample_at::<WIDE>(yrow, x));
                        px[2] = level_of(sample_at::<WIDE>(brow, x >> sx));
                    }
                }
                Some((kr, kb)) => {
                    let (brow, rrow) = (
                        row_of(1, y >> sy, cw as usize),
                        row_of(2, y >> sy, cw as usize),
                    );
                    let denom = 1.0 - kr - kb;
                    for (x, px) in orow.chunks_exact_mut(channels).enumerate() {
                        let yv = sample_at::<WIDE>(yrow, x);
                        let cb = sample_at::<WIDE>(brow, x >> sx);
                        let cr = sample_at::<WIDE>(rrow, x >> sx);
                        if yv >= n || cb >= n || cr >= n {
                            let ey = scaled(yv);
                            let pb = (cb as f64 - mid) / c_scale;
                            let pr = (cr as f64 - mid) / c_scale;
                            let r = ey + 2.0 * (1.0 - kr) * pr;
                            let b = ey + 2.0 * (1.0 - kb) * pb;
                            let g = (ey - kr * r - kb * b) / denom;
                            px[0] = quant(r);
                            px[1] = quant(g);
                            px[2] = quant(b);
                            continue;
                        }
                        let ey = luma[yv];
                        let r = ey + red_add[cr];
                        let b = ey + blue_add[cb];
                        let g = (ey - kr * r - kb * b) / denom;
                        if pairs {
                            px[0] = red_pair[yv * n + cr];
                            px[2] = blue_pair[yv * n + cb];
                        } else {
                            px[0] = quant(r);
                            px[2] = quant(b);
                        }
                        px[1] = quant(g);
                    }
                }
            }
            if let Some(a) = alpha_plane {
                let arow = row_of(a, y, w);
                for (x, px) in orow.chunks_exact_mut(channels).enumerate() {
                    let v = sample_at::<WIDE>(arow, x);
                    px[3] = alpha_tab
                        .get(v)
                        .copied()
                        .unwrap_or_else(|| alpha_of(v as u16));
                }
            }
        }
    };
    // Worth a thread only when each worker gets a real share.
    let workers = threads.min(w * h / ROWS_PIXELS_PER_WORKER).min(h).max(1);
    if workers == 1 {
        rows(0, &mut out);
    } else {
        let rows_each = h.div_ceil(workers);
        let rows = &rows;
        std::thread::scope(|scope| {
            for (i, band) in out.chunks_mut(rows_each * w * channels).enumerate() {
                scope.spawn(move || rows(i * rows_each, band));
            }
        });
    }
    Ok(out)
}

/// The plane count and sizes `layout` needs for a `width × height`
/// picture, checked without touching a sample.
fn check_planes(width: u32, height: u32, layout: HeifPixelFormat, planes: &[Plane]) -> Result<()> {
    if planes.len() != layout.plane_count() {
        return Err(HeifError::invalid(format!(
            "{} planes, layout needs {}",
            planes.len(),
            layout.plane_count()
        )));
    }
    let bps = layout.bytes_per_sample();
    for (i, p) in planes.iter().enumerate() {
        let (w, h) = layout.plane_dims(i, width, height);
        if p.stride < w as usize * bps
            || p.stride == 0
            || h == 0
            || p.data.len() < p.stride * (h as usize - 1) + w as usize * bps
        {
            return Err(HeifError::invalid(format!(
                "plane {i} too small for its layout"
            )));
        }
    }
    Ok(())
}

/// The YCbCr → RGB kernel shared by [`to_rgb`] and [`planes_to_rgb8`]:
/// calls `f(rgb, alpha)` per pixel in row-major order with the
/// normalised (0.0 ..= 1.0, unclamped) RGB and the raw alpha sample (0
/// when the layout has no alpha plane). H.273 §8.3 equations with the
/// matrix / range of `colr` (MIAF default for ICC / absent); identity
/// matrix 0 reads the planes as G, B, R; monochrome becomes grey;
/// sub-sampled chroma is replicated to its co-sited luma samples.
pub fn for_each_pixel(
    width: u32,
    height: u32,
    layout: HeifPixelFormat,
    planes: &[Plane],
    colr: Option<&Colr>,
    mut f: impl FnMut([f64; 3], u16),
) -> Result<()> {
    // Geometry check without cloning the planes.
    check_planes(width, height, layout, planes)?;
    let bps = layout.bytes_per_sample();
    let sample = |plane: usize, x: u32, y: u32| -> u16 {
        let p = &planes[plane];
        let i = y as usize * p.stride + x as usize * bps;
        if bps == 1 {
            p.data[i] as u16
        } else {
            u16::from_le_bytes([p.data[i], p.data[i + 1]])
        }
    };
    let (matrix, full_range) = match colr {
        Some(Colr::Nclx {
            matrix, full_range, ..
        }) => (*matrix, *full_range),
        _ => (6, true),
    };
    let depth = layout.bit_depth as u32;
    let max = ((1u32 << depth) - 1) as f64;
    let alpha = layout.alpha_plane();
    // Limited-range scale factors (H.273 equations 20–22 inverted).
    let sh = (1u32 << (depth - 8)) as f64;
    let (y_off, y_scale, c_scale) = if full_range {
        (0.0, max, max)
    } else {
        (16.0 * sh, 219.0 * sh, 224.0 * sh)
    };
    let mid = (1u32 << (depth - 1)) as f64;
    let (w, h) = (width, height);
    if layout.chroma == Chroma::Mono {
        for y in 0..h {
            for x in 0..w {
                let g = (sample(0, x, y) as f64 - y_off) / y_scale;
                f([g, g, g], alpha.map(|a| sample(a, x, y)).unwrap_or(0));
            }
        }
        return Ok(());
    }
    let (sx, sy) = layout.chroma.shift();
    let kr_kb = if matrix == 0 {
        None
    } else {
        Some(matrix_kr_kb(matrix).ok_or_else(|| {
            HeifError::unsupported(format!("matrix_coefficients {matrix} conversion"))
        })?)
    };
    for y in 0..h {
        for x in 0..w {
            let yv = sample(0, x, y) as f64;
            let cb = sample(1, x >> sx, y >> sy) as f64;
            let cr = sample(2, x >> sx, y >> sy) as f64;
            let rgb = match kr_kb {
                None => {
                    // Identity: Y = G, Cb = B, Cr = R (equations 41–43).
                    [
                        (cr - y_off) / y_scale,
                        (yv - y_off) / y_scale,
                        (cb - y_off) / y_scale,
                    ]
                }
                Some((kr, kb)) => {
                    let ey = (yv - y_off) / y_scale;
                    let pb = (cb - mid) / c_scale;
                    let pr = (cr - mid) / c_scale;
                    let r = ey + 2.0 * (1.0 - kr) * pr;
                    let b = ey + 2.0 * (1.0 - kb) * pb;
                    let g = (ey - kr * r - kb * b) / (1.0 - kr - kb);
                    [r, g, b]
                }
            };
            f(rgb, alpha.map(|a| sample(a, x, y)).unwrap_or(0));
        }
    }
    Ok(())
}

/// Convert an interleaved RGB / RGBA picture to a planar 4:4:4 YCbCr
/// (or monochrome, when `chroma` is [`Chroma::Mono`]: the R = G = B
/// grey level) frame at the picture's bit depth, using the matrix /
/// range of `colr` exactly as [`to_rgb`] does in the other direction
/// (H.273 equations 20–22 / 38–40; identity matrix 0 = GBR). An alpha
/// channel becomes the alpha plane.
pub fn from_rgb(rgb: &RgbImage16, colr: Option<&Colr>, chroma: Chroma) -> Result<HeifFrame> {
    if !matches!(chroma, Chroma::Yuv444 | Chroma::Mono) {
        return Err(HeifError::unsupported(
            "from_rgb writes 4:4:4 or monochrome frames only",
        ));
    }
    if !(3..=4).contains(&rgb.channels) || !(8..=16).contains(&rgb.bit_depth) {
        return Err(HeifError::invalid(format!(
            "from_rgb: {} channels at {} bits",
            rgb.channels, rgb.bit_depth
        )));
    }
    let (matrix, full_range) = match colr {
        Some(Colr::Nclx {
            matrix, full_range, ..
        }) => (*matrix, *full_range),
        _ => (6, true),
    };
    let depth = rgb.bit_depth as u32;
    let max = ((1u32 << depth) - 1) as f64;
    let sh = (1u32 << (depth - 8)) as f64;
    let (y_off, y_scale, c_scale) = if full_range {
        (0.0, max, max)
    } else {
        (16.0 * sh, 219.0 * sh, 224.0 * sh)
    };
    let mid = (1u32 << (depth - 1)) as f64;
    let has_alpha = rgb.channels == 4;
    let fmt = HeifPixelFormat::new(chroma, rgb.bit_depth, has_alpha)?;
    let mut out = HeifFrame::zeroed(rgb.width, rgb.height, fmt)?;
    let quant = |v: f64| v.round().clamp(0.0, max) as u16;
    let kr_kb = if chroma == Chroma::Mono || matrix == 0 {
        None
    } else {
        Some(matrix_kr_kb(matrix).ok_or_else(|| {
            HeifError::unsupported(format!("matrix_coefficients {matrix} conversion"))
        })?)
    };
    for y in 0..rgb.height {
        for x in 0..rgb.width {
            let r = rgb.sample(x, y, 0) as f64 / max;
            let g = rgb.sample(x, y, 1) as f64 / max;
            let b = rgb.sample(x, y, 2) as f64 / max;
            match (chroma, kr_kb) {
                (Chroma::Mono, _) => {
                    out.set_sample(0, x, y, quant(g * y_scale + y_off));
                }
                (_, None) => {
                    // Identity: Y = G, Cb = B, Cr = R.
                    out.set_sample(0, x, y, quant(g * y_scale + y_off));
                    out.set_sample(1, x, y, quant(b * y_scale + y_off));
                    out.set_sample(2, x, y, quant(r * y_scale + y_off));
                }
                (_, Some((kr, kb))) => {
                    let ey = kr * r + (1.0 - kr - kb) * g + kb * b;
                    let pb = (b - ey) / (2.0 * (1.0 - kb));
                    let pr = (r - ey) / (2.0 * (1.0 - kr));
                    out.set_sample(0, x, y, quant(ey * y_scale + y_off));
                    out.set_sample(1, x, y, quant(pb * c_scale + mid));
                    out.set_sample(2, x, y, quant(pr * c_scale + mid));
                }
            }
            if let Some(a) = out.format.alpha_plane() {
                out.set_sample(a, x, y, rgb.sample(x, y, 3));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::fill_to_ycbcr;
    use crate::image::HeifPixelFormat;

    fn frame_of(chroma: Chroma, depth: u8, ycc: [u16; 3]) -> HeifFrame {
        let mut f =
            HeifFrame::zeroed(2, 2, HeifPixelFormat::new(chroma, depth, false).unwrap()).unwrap();
        for (p, v) in ycc.iter().enumerate() {
            let (pw, ph) = f.plane_dims(p);
            for y in 0..ph {
                for x in 0..pw {
                    f.set_sample(p, x, y, *v);
                }
            }
        }
        f
    }

    #[test]
    fn round_trips_the_overlay_fill_conversion() {
        for (matrix, full) in [(6u16, true), (1, false), (9, false), (0, true), (5, true)] {
            let colr = Colr::Nclx {
                primaries: 1,
                transfer: 13,
                matrix,
                full_range: full,
            };
            for depth in [8u8, 10] {
                let max = (1u32 << depth) - 1;
                for rgb in [
                    [0u16, 0, 0],
                    [65535, 65535, 65535],
                    [65535, 0, 0],
                    [12000, 40000, 30000],
                ] {
                    let ycc = fill_to_ycbcr(rgb, depth, Some(&colr));
                    let f = frame_of(Chroma::Yuv444, depth, ycc);
                    let out = to_rgb(&f, Some(&colr)).unwrap();
                    assert_eq!(out.channels, 3);
                    for c in 0..3 {
                        let want = (rgb[c] as f64 / 65535.0 * max as f64).round() as i32;
                        let got = out.sample(1, 1, c) as i32;
                        assert!(
                            (got - want).abs() <= 1 + (depth as i32 - 8),
                            "matrix {matrix} full {full} depth {depth} rgb {rgb:?}: ch {c} got {got} want {want}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn mono_and_alpha_and_subsampling() {
        let mut f = HeifFrame::filled(
            2,
            2,
            HeifPixelFormat::new(Chroma::Mono, 8, true).unwrap(),
            200,
        )
        .unwrap();
        f.set_sample(1, 1, 1, 17);
        let limited = Colr::Nclx {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
        };
        let out = to_rgb(&f, Some(&limited)).unwrap();
        assert_eq!(out.channels, 4);
        assert_eq!(out.sample(1, 1, 3), 17);
        assert_eq!(
            out.sample(0, 0, 0),
            ((200.0 - 16.0) / 219.0 * 255.0f64).round() as u16
        );
        assert_eq!(out.to_be_bytes().len(), 16);
        // 4:2:0 replicates the single chroma sample over the 2x2 block.
        let g = frame_of(Chroma::Yuv420, 8, [128, 64, 192]);
        let out = to_rgb(&g, None).unwrap();
        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(out.sample(x, y, 0), out.sample(0, 0, 0));
            }
        }
        assert!(out.sample(0, 0, 0) > out.sample(0, 0, 1), "Cr high → red");
        assert!(
            to_rgb(
                &frame_of(Chroma::Yuv444, 8, [1, 2, 3]),
                Some(&Colr::Nclx {
                    primaries: 1,
                    transfer: 1,
                    matrix: 14,
                    full_range: true
                })
            )
            .is_err()
        );
    }
    /// The table-driven conversion against the per-sample kernel it
    /// replaces, bit for bit: every layout, depths 8 / 10 / 12, the
    /// matrices this crate converts and the identity, both ranges,
    /// with and without alpha, at a size that takes the per-value
    /// tables and one that also takes the (luma, chroma) pair tables —
    /// including samples above the nominal maximum of a deep plane.
    #[test]
    fn table_conversion_equals_the_per_sample_kernel() {
        let mut state = 0x2545_F491u32;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for (w, h) in [(80u32, 60u32), (601, 449)] {
            for chroma in [Chroma::Mono, Chroma::Yuv420, Chroma::Yuv422, Chroma::Yuv444] {
                for depth in [8u8, 10, 12] {
                    if (w, h) == (601, 449) && depth == 10 {
                        continue;
                    }
                    for alpha in [false, true] {
                        let fmt = HeifPixelFormat::new(chroma, depth, alpha).unwrap();
                        let mut f = HeifFrame::zeroed(w, h, fmt).unwrap();
                        let max = fmt.max_value() as u32;
                        for p in 0..fmt.plane_count() {
                            let (pw, ph) = f.plane_dims(p);
                            for y in 0..ph {
                                for x in 0..pw {
                                    // Now and then a stray high bit.
                                    let r = next();
                                    let v = if depth > 8 && r % 251 == 0 {
                                        (r >> 8) & 0xffff
                                    } else {
                                        (r >> 8) % (max + 1)
                                    };
                                    f.set_sample(p, x, y, v as u16);
                                }
                            }
                        }
                        for (matrix, full_range) in [
                            (6u16, true),
                            (6, false),
                            (1, true),
                            (1, false),
                            (9, false),
                            (0, true),
                            (0, false),
                        ] {
                            let colr = Colr::Nclx {
                                primaries: 1,
                                transfer: 13,
                                matrix,
                                full_range,
                            };
                            let what = format!(
                                "{w}x{h} {chroma:?} {depth}b a={alpha} m={matrix} full={full_range}"
                            );
                            // The 16-bit form.
                            let top = fmt.max_value() as f64;
                            let mut want = Vec::new();
                            for_each_pixel(w, h, fmt, &f.planes, Some(&colr), |rgb, a| {
                                want.extend(rgb.map(|v| (v * top).round().clamp(0.0, top) as u16));
                                if alpha {
                                    want.push(a);
                                }
                            })
                            .unwrap();
                            assert!(
                                to_rgb(&f, Some(&colr)).unwrap().data == want,
                                "to_rgb {what}"
                            );
                            // The packed 8-bit forms.
                            for rgba in [false, true] {
                                let mut want = Vec::new();
                                for_each_pixel(w, h, fmt, &f.planes, Some(&colr), |rgb, a| {
                                    want.extend(
                                        rgb.map(|v| (v * 255.0).round().clamp(0.0, 255.0) as u8),
                                    );
                                    if rgba {
                                        want.push(if alpha {
                                            (a as f64 / top * 255.0).round().clamp(0.0, 255.0) as u8
                                        } else {
                                            255
                                        });
                                    }
                                })
                                .unwrap();
                                let got = planes_to_rgb8(w, h, fmt, &f.planes, Some(&colr), rgba)
                                    .unwrap();
                                assert!(got == want, "planes_to_rgb8 rgba={rgba} {what}");
                                for threads in [2, 5] {
                                    let got = planes_to_rgb8_threads(
                                        w,
                                        h,
                                        fmt,
                                        &f.planes,
                                        Some(&colr),
                                        rgba,
                                        threads,
                                    )
                                    .unwrap();
                                    assert!(got == want, "{threads} workers rgba={rgba} {what}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    /// Bands of rows on several workers give the serial bytes: a
    /// picture large enough for four workers, odd on both axes, with
    /// subsampled chroma and alpha.
    #[test]
    fn threaded_conversion_equals_the_serial_one() {
        let (w, h) = (1701u32, 1301u32);
        for (chroma, depth) in [(Chroma::Yuv420, 8u8), (Chroma::Yuv422, 10)] {
            let fmt = HeifPixelFormat::new(chroma, depth, true).unwrap();
            let mut f = HeifFrame::zeroed(w, h, fmt).unwrap();
            let max = fmt.max_value() as u32;
            let mut state = 0x9E37_79B9u32;
            for p in 0..fmt.plane_count() {
                let (pw, ph) = f.plane_dims(p);
                for y in 0..ph {
                    for x in 0..pw {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        f.set_sample(p, x, y, ((state >> 12) % (max + 1)) as u16);
                    }
                }
            }
            let serial = planes_to_rgb8(w, h, fmt, &f.planes, None, true).unwrap();
            for threads in [2, 3, 4, 16] {
                let got =
                    planes_to_rgb8_threads(w, h, fmt, &f.planes, None, true, threads).unwrap();
                assert!(got == serial, "{chroma:?} {depth}b on {threads} workers");
            }
        }
    }
}
