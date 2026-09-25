//! Packed-input conversion of the `"heif"` framework encoder: RGB /
//! RGBA / BGR(A) / 16-bit / grey+alpha frames (what a PNG decoder
//! hands the pipeline) are converted to planar YCbCr + alpha inside
//! the encoder, and round-trip through our decoder and
//! `rgb::to_rgb` within ±1 code.
#![cfg(feature = "registry")]

use oxideav_core::{
    CodecId, CodecOptions, CodecParameters, Frame, PixelFormat, RuntimeContext, VideoFrame,
    VideoPlane,
};
use oxideav_heif::rgb::to_rgb;
use oxideav_heif::{decode_primary, HeifFile, ItemDecoder};

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    ctx
}

/// Source colour at `(x, y)`: constant over 2×2 blocks so the 4:2:0
/// coding loses nothing, varying smoothly so every channel is
/// exercised; returns `[r, g, b, a]` in 16-bit scale.
fn colour(x: u32, y: u32, w: u32, h: u32) -> [u16; 4] {
    let (bx, by) = (x / 2, y / 2);
    let fx = bx as f64 / (w / 2).max(1) as f64;
    let fy = by as f64 / (h / 2).max(1) as f64;
    let r = (fx * 65535.0) as u16;
    let g = (fy * 65535.0) as u16;
    let b = (((fx + fy) * 0.5) * 65535.0) as u16;
    let a = ((0.25 + 0.75 * fy) * 65535.0) as u16;
    [r, g, b, a]
}

fn packed(pf: PixelFormat, w: u32, h: u32) -> VideoFrame {
    let (bpp, wide) = match pf {
        PixelFormat::Rgb24 | PixelFormat::Bgr24 => (3, false),
        PixelFormat::Rgba | PixelFormat::Bgra => (4, false),
        PixelFormat::Rgb48Le => (6, true),
        PixelFormat::Rgba64Le => (8, true),
        PixelFormat::Ya8 => (2, false),
        PixelFormat::Ya16Le => (4, true),
        _ => unreachable!(),
    };
    let stride = w as usize * bpp + 5; // padded rows
    let mut data = vec![0u8; stride * h as usize];
    for y in 0..h {
        for x in 0..w {
            let [r, g, b, a] = colour(x, y, w, h);
            let grey = ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u16;
            let chans: Vec<u16> = match pf {
                PixelFormat::Rgb24 | PixelFormat::Rgb48Le => vec![r, g, b],
                PixelFormat::Rgba | PixelFormat::Rgba64Le => vec![r, g, b, a],
                PixelFormat::Bgr24 => vec![b, g, r],
                PixelFormat::Bgra => vec![b, g, r, a],
                PixelFormat::Ya8 | PixelFormat::Ya16Le => vec![grey, a],
                _ => unreachable!(),
            };
            let at = y as usize * stride + x as usize * bpp;
            for (i, c) in chans.iter().enumerate() {
                if wide {
                    data[at + 2 * i..at + 2 * i + 2].copy_from_slice(&c.to_le_bytes());
                } else {
                    data[at + i] = (*c >> 8) as u8;
                }
            }
        }
    }
    VideoFrame {
        pts: Some(0),
        planes: vec![VideoPlane { stride, data }],
    }
}

fn encode(ctx: &RuntimeContext, pf: PixelFormat, w: u32, h: u32, range: &str) -> Vec<u8> {
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(w);
    params.height = Some(h);
    params.pixel_format = Some(pf);
    params.options = CodecOptions::new().set("mode", "pcm").set("range", range);
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    enc.send_frame(&Frame::Video(packed(pf, w, h))).unwrap();
    enc.flush().unwrap();
    enc.receive_packet().unwrap().data
}

#[test]
fn packed_inputs_round_trip_within_one_code() {
    let ctx = context();
    let (w, h) = (48u32, 32u32);
    for pf in [
        PixelFormat::Rgb24,
        PixelFormat::Rgba,
        PixelFormat::Bgr24,
        PixelFormat::Bgra,
        PixelFormat::Rgb48Le,
        PixelFormat::Rgba64Le,
        PixelFormat::Ya8,
        PixelFormat::Ya16Le,
    ] {
        for range in ["full", "limited"] {
            let bytes = encode(&ctx, pf, w, h, range);
            let f = HeifFile::parse(&bytes).unwrap();
            let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
            assert_eq!((img.width(), img.height()), (w, h), "{pf:?}");
            let has_alpha = matches!(
                pf,
                PixelFormat::Rgba
                    | PixelFormat::Bgra
                    | PixelFormat::Rgba64Le
                    | PixelFormat::Ya8
                    | PixelFormat::Ya16Le
            );
            assert_eq!(img.frame.format.has_alpha, has_alpha, "{pf:?}: alpha plane");
            assert!(
                matches!(img.nclx, oxideav_heif::props::Colr::Nclx { full_range, .. } if full_range == (range == "full")),
                "{pf:?}: range written"
            );
            let rgb = to_rgb(&img.frame, Some(&img.nclx)).unwrap();
            let grey = matches!(pf, PixelFormat::Ya8 | PixelFormat::Ya16Le);
            let mut max = 0i32;
            for y in 0..h {
                for x in 0..w {
                    let [r, g, b, a] = colour(x, y, w, h);
                    let want = if grey {
                        let v = ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u16;
                        [v, v, v, a]
                    } else {
                        [r, g, b, a]
                    };
                    for (c, wv) in want.iter().enumerate().take(3) {
                        let d = (rgb.sample(x, y, c) as i32 - (*wv >> 8) as i32).abs();
                        max = max.max(d);
                    }
                    if has_alpha {
                        let d = (rgb.sample(x, y, 3) as i32 - (want[3] >> 8) as i32).abs();
                        max = max.max(d);
                    }
                }
            }
            // 8-bit full range is ±1; limited range quantises to 219
            // codes and 16-bit sources are rounded to 8 bits before
            // coding, one extra code of slack each.
            let wide = matches!(
                pf,
                PixelFormat::Rgb48Le | PixelFormat::Rgba64Le | PixelFormat::Ya16Le
            );
            let tol = 1 + (range != "full") as i32 + wide as i32;
            assert!(max <= tol, "{pf:?} {range}: max diff {max}");
        }
    }
}
