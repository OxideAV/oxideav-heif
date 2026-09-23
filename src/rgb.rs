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
use crate::image::{Chroma, HeifFrame};
use crate::props::Colr;

/// An interleaved RGB / RGBA picture, one `u16` per sample holding
/// `bit_depth` significant bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbImage {
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

impl RgbImage {
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
/// BT.601 matrix, full range).
pub fn to_rgb(frame: &HeifFrame, colr: Option<&Colr>) -> Result<RgbImage> {
    frame.validate()?;
    let (matrix, full_range) = match colr {
        Some(Colr::Nclx {
            matrix, full_range, ..
        }) => (*matrix, *full_range),
        _ => (6, true),
    };
    let depth = frame.format.bit_depth as u32;
    let max = ((1u32 << depth) - 1) as f64;
    let channels = 3 + frame.format.has_alpha as usize;
    let alpha = frame.format.alpha_plane();
    let (w, h) = (frame.width, frame.height);
    let mut data = Vec::with_capacity(w as usize * h as usize * channels);
    // Limited-range scale factors (H.273 equations 20–22 inverted).
    let sh = (1u32 << (depth - 8)) as f64;
    let (y_off, y_scale, c_scale) = if full_range {
        (0.0, max, max)
    } else {
        (16.0 * sh, 219.0 * sh, 224.0 * sh)
    };
    let mid = (1u32 << (depth - 1)) as f64;
    let quant = |v: f64| (v * max).round().clamp(0.0, max) as u16;
    if frame.format.chroma == Chroma::Mono {
        for y in 0..h {
            for x in 0..w {
                let g = quant((frame.sample(0, x, y) as f64 - y_off) / y_scale);
                data.extend_from_slice(&[g, g, g]);
                if let Some(a) = alpha {
                    data.push(frame.sample(a, x, y));
                }
            }
        }
    } else {
        let (sx, sy) = frame.format.chroma.shift();
        let kr_kb = if matrix == 0 {
            None
        } else {
            Some(matrix_kr_kb(matrix).ok_or_else(|| {
                HeifError::unsupported(format!("matrix_coefficients {matrix} conversion"))
            })?)
        };
        for y in 0..h {
            for x in 0..w {
                let yv = frame.sample(0, x, y) as f64;
                let cb = frame.sample(1, x >> sx, y >> sy) as f64;
                let cr = frame.sample(2, x >> sx, y >> sy) as f64;
                let rgb = match kr_kb {
                    None => {
                        // Identity: Y = G, Cb = B, Cr = R (equations 41–43).
                        [
                            quant((cr - y_off) / y_scale),
                            quant((yv - y_off) / y_scale),
                            quant((cb - y_off) / y_scale),
                        ]
                    }
                    Some((kr, kb)) => {
                        let ey = (yv - y_off) / y_scale;
                        let pb = (cb - mid) / c_scale;
                        let pr = (cr - mid) / c_scale;
                        let r = ey + 2.0 * (1.0 - kr) * pr;
                        let b = ey + 2.0 * (1.0 - kb) * pb;
                        let g = (ey - kr * r - kb * b) / (1.0 - kr - kb);
                        [quant(r), quant(g), quant(b)]
                    }
                };
                data.extend_from_slice(&rgb);
                if let Some(a) = alpha {
                    data.push(frame.sample(a, x, y));
                }
            }
        }
    }
    Ok(RgbImage {
        width: w,
        height: h,
        channels,
        bit_depth: frame.format.bit_depth,
        data,
    })
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
        assert!(to_rgb(
            &frame_of(Chroma::Yuv444, 8, [1, 2, 3]),
            Some(&Colr::Nclx {
                primaries: 1,
                transfer: 1,
                matrix: 14,
                full_range: true
            })
        )
        .is_err());
    }
}
