//! Encode-path budget behaviour (round 464): the thread budget is
//! spent on grid tiles / the codecs without changing a byte, packed
//! sources convert straight into the coding layout, deeper sources
//! code at 10 bits, the automatic grid tiles large pictures, and the
//! writer streams its `mdat`.
#![cfg(feature = "registry")]

mod common;

use oxideav_core::{
    CodecId, CodecOptions, CodecParameters, Encoder, ExecutionContext, Frame, PixelFormat,
    VideoFrame, VideoPlane,
};
use oxideav_heif::decode::{decode_primary, ItemDecoder};
use oxideav_heif::encode::{
    encode_still, encode_still_owned, packed_to_planar, packed_to_planar_for, to_yuv420,
    EncodeOptions, StillCodec, GRID_AUTO_MIN_PIXELS, GRID_AUTO_TILE,
};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};
use oxideav_heif::props::Colr;
use oxideav_heif::{HeifFile, HeifWriter};

/// A textured picture (gradients + a disc + noise-like hashing) so the
/// coders have real work on every tile.
fn picture(w: u32, h: u32, depth: u8, chroma: Chroma, alpha: bool) -> HeifFrame {
    let fmt = HeifPixelFormat::new(chroma, depth, alpha).unwrap();
    let mut f = HeifFrame::zeroed(w, h, fmt).unwrap();
    let max = fmt.max_value() as u32;
    for p in 0..fmt.plane_count() {
        let (pw, ph) = f.plane_dims(p);
        for y in 0..ph {
            for x in 0..pw {
                let hash = (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) >> 7 & 31;
                let v = match p {
                    0 => (x * max / pw.max(1) + y * max / ph.max(1)) / 2 + hash,
                    1 => max / 2 + (x * 40 / pw.max(1)) + hash / 4,
                    2 => max / 2 - (y * 40 / ph.max(1)) + hash / 4,
                    _ => (x + y) * max / (pw + ph).max(1),
                };
                f.set_sample(p, x, y, v.min(max) as u16);
            }
        }
    }
    f
}

fn packed_rgb(w: u32, h: u32, alpha: bool, wide: bool) -> (VideoFrame, PixelFormat) {
    let ch = 3 + alpha as usize;
    let bps = if wide { 2 } else { 1 };
    let mut data = Vec::with_capacity(w as usize * h as usize * ch * bps);
    for y in 0..h {
        for x in 0..w {
            let hash = (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) >> 7 & 0xff;
            let r = (x * 65535 / w.max(1)) ^ (hash << 4);
            let g = y * 65535 / h.max(1);
            let b = ((x + y) * 65535 / (w + h).max(1)) ^ (hash << 3);
            let a = 65535 - (x * 65535 / w.max(1));
            for v in [r, g, b, a].into_iter().take(ch) {
                let v = v.min(65535) as u16;
                if wide {
                    data.extend_from_slice(&v.to_le_bytes());
                } else {
                    data.push((v >> 8) as u8);
                }
            }
        }
    }
    let pf = match (alpha, wide) {
        (false, false) => PixelFormat::Rgb24,
        (true, false) => PixelFormat::Rgba,
        (false, true) => PixelFormat::Rgb48Le,
        (true, true) => PixelFormat::Rgba64Le,
    };
    (
        VideoFrame {
            pts: Some(0),
            planes: vec![VideoPlane {
                stride: w as usize * ch * bps,
                data,
            }],
        },
        pf,
    )
}

fn decode(bytes: &[u8]) -> oxideav_heif::decode::DecodedImage {
    let f = HeifFile::parse(bytes).unwrap();
    decode_primary(&f, ItemDecoder::direct()).unwrap()
}

#[test]
fn grid_tiles_in_parallel_are_byte_identical_to_serial() {
    // HEVC: 300 / 64 → 5 columns, 220 / 64 → 4 rows; AV1 (slower in a
    // debug build): 3 x 2 tiles. Every budget reproduces the serial
    // file byte for byte; the hidden items are the tiles plus the
    // alpha auxiliary.
    for (codec, w, h, tiles, budgets) in [
        (StillCodec::Hevc, 300, 220, 20, &[2usize, 3, 8][..]),
        (StillCodec::Av1, 160, 96, 6, &[4usize][..]),
    ] {
        let src = picture(w, h, 8, Chroma::Yuv420, true);
        let base = EncodeOptions::default()
            .with_codec(codec)
            .with_grid_tile(Some(64))
            .with_thumbnail_max_dim((codec == StillCodec::Hevc).then_some(48))
            .with_av1_quality(Some(50))
            .with_qp(30);
        let serial = encode_still(&src, &base).unwrap();
        for &threads in budgets {
            let par = encode_still(&src, &base.clone().with_threads(Some(threads))).unwrap();
            assert!(
                par == serial,
                "{codec:?}: {threads} threads differ from serial"
            );
        }
        let img = decode(&serial);
        assert_eq!((img.width(), img.height()), (w, h));
        assert!(img.frame.format.has_alpha);
        let f = HeifFile::parse(&serial).unwrap();
        assert_eq!(
            f.meta
                .as_ref()
                .unwrap()
                .items
                .iter()
                .filter(|i| i.is_hidden())
                .count(),
            tiles + 1,
            "{codec:?}: tile count"
        );
    }
}

#[test]
fn hevc_wavefront_budget_keeps_the_bytes() {
    // The quadtree coder (rd set) runs the wavefront: the bytes are
    // those of the serial pass for any budget.
    let src = picture(200, 136, 8, Chroma::Yuv420, false);
    let base = EncodeOptions::default().with_hevc_rd(Some(1)).with_qp(28);
    let serial = encode_still(&src, &base).unwrap();
    let par = encode_still(&src, &base.with_threads(Some(4))).unwrap();
    assert_eq!(par, serial);
    let img = decode(&serial);
    assert_eq!(img.frame.format.bit_depth, 8);
}

#[test]
fn deeper_sources_code_at_ten_bits() {
    // A 16-bit 4:4:4 source codes as Main 10 4:2:0; a 12-bit one at 12.
    let src16 = picture(64, 48, 16, Chroma::Yuv444, false);
    let bytes = encode_still(&src16, &EncodeOptions::default()).unwrap();
    let img = decode(&bytes);
    assert_eq!(img.frame.format.bit_depth, 10);
    assert_eq!(img.frame.format.chroma, Chroma::Yuv420);
    let src12 = picture(64, 48, 12, Chroma::Yuv420, false);
    let bytes = encode_still(
        &src12,
        &EncodeOptions::default().with_hevc_mode("pcm".into()),
    )
    .unwrap();
    let img = decode(&bytes);
    assert_eq!(img.frame.format.bit_depth, 12);
    // Lossless at 12 bits reproduces the source.
    assert_eq!(img.frame, src12);
    // An explicit depth wins.
    let bytes = encode_still(&src16, &EncodeOptions::default().with_hevc_depth(Some(8))).unwrap();
    assert_eq!(decode(&bytes).frame.format.bit_depth, 8);
    assert!(encode_still(&src16, &EncodeOptions::default().with_hevc_depth(Some(9))).is_err());
}

#[test]
fn to_yuv420_round_trips_depth_changes() {
    let src = picture(10, 6, 10, Chroma::Yuv422, true);
    let down = to_yuv420(&src, 8).unwrap();
    assert_eq!(
        down.format,
        HeifPixelFormat::new(Chroma::Yuv420, 8, false).unwrap()
    );
    assert_eq!(down.sample(0, 3, 2), (src.sample(0, 3, 2) + 2) >> 2);
    let up = to_yuv420(&down, 10).unwrap();
    // Bit replication: 0 → 0, 255 → 1023, and every value within 5
    // codes of the 10-bit source after one round trip (±2 from the
    // rounding down, up to 3 from the replicated low bits).
    for y in 0..6 {
        for x in 0..10 {
            let d = (up.sample(0, x, y) as i32 - src.sample(0, x, y) as i32).abs();
            assert!(d <= 5, "({x},{y}) {d}");
        }
    }
    assert_eq!(
        to_yuv420(&HeifFrame::filled(2, 2, down.format, 255).unwrap(), 10)
            .unwrap()
            .sample(0, 0, 0),
        1023
    );
    let same = to_yuv420(&src, 10).unwrap();
    assert_eq!(same.format.chroma, Chroma::Yuv420);
    assert_eq!(same.sample(0, 5, 5), src.sample(0, 5, 5));
}

#[test]
fn packed_conversion_matches_the_two_step_path() {
    // The direct packed → 4:2:0 conversion equals the historical
    // 4:4:4 → to_yuv420 path for 8-bit sources, sample for sample.
    let colr = Colr::MIAF_DEFAULT;
    for (alpha, w, h) in [(false, 33, 17), (true, 48, 32)] {
        let (vf, pf) = packed_rgb(w, h, alpha, false);
        let two_step = to_yuv420(&packed_to_planar(&vf, w, h, pf, &colr).unwrap(), 8).unwrap();
        let direct = packed_to_planar_for(
            &vf,
            w,
            h,
            pf,
            &colr,
            HeifPixelFormat::new(Chroma::Yuv420, 8, false).unwrap(),
        )
        .unwrap();
        assert_eq!(direct, two_step, "alpha={alpha}");
        // With the alpha plane carried.
        if alpha {
            let with = packed_to_planar_for(
                &vf,
                w,
                h,
                pf,
                &colr,
                HeifPixelFormat::new(Chroma::Yuv420, 8, true).unwrap(),
            )
            .unwrap();
            let full = packed_to_planar(&vf, w, h, pf, &colr).unwrap();
            assert_eq!(with.planes[3], full.planes[3]);
            assert_eq!(with.planes[..3], direct.planes[..]);
        }
    }
    // 16-bit source straight to 10-bit 4:4:4 and 4:2:0.
    let (vf, pf) = packed_rgb(20, 12, false, true);
    let t444 = packed_to_planar_for(
        &vf,
        20,
        12,
        pf,
        &colr,
        HeifPixelFormat::new(Chroma::Yuv444, 10, false).unwrap(),
    )
    .unwrap();
    let t420 = packed_to_planar_for(
        &vf,
        20,
        12,
        pf,
        &colr,
        HeifPixelFormat::new(Chroma::Yuv420, 10, false).unwrap(),
    )
    .unwrap();
    assert_eq!(to_yuv420(&t444, 10).unwrap(), t420);
    // The full-resolution 16-bit path rounded to 10 bits agrees with
    // the direct 10-bit conversion within one code (it rounds the
    // 16-bit result; the direct path rounds the 10-bit conversion).
    let via16 = to_yuv420(&packed_to_planar(&vf, 20, 12, pf, &colr).unwrap(), 10).unwrap();
    for p in 0..3 {
        let (pw, ph) = via16.plane_dims(p);
        for y in 0..ph {
            for x in 0..pw {
                let d = (via16.sample(p, x, y) as i32 - t420.sample(p, x, y) as i32).abs();
                assert!(d <= 1, "plane {p} ({x},{y}): {d}");
            }
        }
    }
}

#[test]
fn framework_encoder_threads_option_and_execution_context() {
    let (vf, pf) = packed_rgb(160, 96, true, false);
    let make = |opts: CodecOptions| {
        let mut params = CodecParameters::video(CodecId::new("heif"));
        params.width = Some(160);
        params.height = Some(96);
        params.pixel_format = Some(pf);
        params.options = opts;
        oxideav_heif::encode::make_encoder(&params).unwrap()
    };
    let run = |enc: &mut Box<dyn Encoder>| {
        enc.send_frame(&Frame::Video(vf.clone())).unwrap();
        enc.flush().unwrap();
        enc.receive_packet().unwrap().data.to_vec()
    };
    let base = CodecOptions::new().set("grid", "64").set("qp", "30");
    let mut serial = make(base.clone());
    let a = run(&mut serial);
    // `threads=auto` and an execution context spend the budget on the
    // tiles; the file does not change.
    let mut auto = make(base.clone().set("threads", "auto"));
    let b = run(&mut auto);
    assert_eq!(a, b);
    let mut ctx = make(base.clone());
    ctx.set_execution_context(&ExecutionContext::with_threads(4));
    let c = run(&mut ctx);
    assert_eq!(a, c);
    // Explicit `threads` wins over the context.
    let mut explicit = make(base.clone().set("threads", "2"));
    explicit.set_execution_context(&ExecutionContext::with_threads(8));
    let d = run(&mut explicit);
    assert_eq!(a, d);
    // The decoded picture carries the alpha and the 4:2:0 coding.
    let img = decode(&a);
    assert!(img.frame.format.has_alpha);
    assert_eq!(img.frame.format.chroma, Chroma::Yuv420);
    // `grid=none` writes a single item; `grid=auto` on a small picture too.
    let one = run(&mut make(
        CodecOptions::new().set("grid", "none").set("qp", "30"),
    ));
    let auto_small = run(&mut make(CodecOptions::new().set("qp", "30")));
    assert_eq!(one, auto_small);
    let f = HeifFile::parse(&one).unwrap();
    // Only the alpha auxiliary is hidden: no tiles.
    assert_eq!(
        f.meta
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|i| i.is_hidden())
            .count(),
        1
    );
    // Refusals.
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(160);
    params.height = Some(96);
    params.pixel_format = Some(pf);
    params.options = CodecOptions::new().set("threads", "many");
    assert!(oxideav_heif::encode::make_encoder(&params).is_err());
    params.options = CodecOptions::new().set("grid", "big");
    assert!(oxideav_heif::encode::make_encoder(&params).is_err());
    params.options = CodecOptions::new().set("depth", "9");
    assert!(oxideav_heif::encode::make_encoder(&params).is_err());
}

#[test]
fn framework_encoder_chroma_and_filters_options() {
    let (vf, pf) = packed_rgb(96, 64, false, false);
    let encode = |opts: CodecOptions| {
        let mut params = CodecParameters::video(CodecId::new("heif"));
        params.width = Some(96);
        params.height = Some(64);
        params.pixel_format = Some(pf);
        params.options = opts;
        let mut enc = oxideav_heif::encode::make_encoder(&params).unwrap();
        enc.send_frame(&Frame::Video(vf.clone())).unwrap();
        enc.flush().unwrap();
        enc.receive_packet().unwrap().data.to_vec()
    };
    // AV1: packed RGB codes 4:2:0 by default, 4:4:4 on request.
    let a420 = decode(&encode(CodecOptions::new().set("codec", "av1")));
    assert_eq!(a420.frame.format.chroma, Chroma::Yuv420);
    let a444 = decode(&encode(
        CodecOptions::new().set("codec", "av1").set("chroma", "444"),
    ));
    assert_eq!(a444.frame.format.chroma, Chroma::Yuv444);
    // HEVC always codes 4:2:0; the filters change the bytes.
    let on = encode(CodecOptions::new().set("chroma", "444"));
    assert_eq!(decode(&on).frame.format.chroma, Chroma::Yuv420);
    let off = encode(CodecOptions::new().set("filters", "off"));
    assert_ne!(on, off);
    // A 16-bit source lands as a Main 10 item.
    let (vf16, pf16) = packed_rgb(96, 64, false, true);
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(96);
    params.height = Some(64);
    params.pixel_format = Some(pf16);
    let mut enc = oxideav_heif::encode::make_encoder(&params).unwrap();
    enc.send_frame(&Frame::Video(vf16)).unwrap();
    enc.flush().unwrap();
    let img = decode(&enc.receive_packet().unwrap().data);
    assert_eq!(img.frame.format.bit_depth, 10);
}

#[test]
fn automatic_grid_above_four_megapixels() {
    // 2048 x 2049 exceeds the 4 MP rule by one row: tiled at 512 px;
    // 2048 x 2048 is a single item. PCM keeps the test fast.
    let opts = EncodeOptions::default().with_hevc_mode("pcm".into());
    assert_eq!(opts.effective_grid_tile(2048, 2048), None);
    assert_eq!(opts.effective_grid_tile(2048, 2049), Some(GRID_AUTO_TILE));
    assert_eq!(
        opts.clone()
            .with_grid_tile(Some(0))
            .effective_grid_tile(4032, 3024),
        None
    );
    let (w, h) = (2048u32, 2049u32);
    assert!(w as u64 * h as u64 > GRID_AUTO_MIN_PIXELS);
    let src = picture(2048, 2049, 8, Chroma::Yuv420, false);
    let bytes = encode_still_owned(src.clone(), &opts).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let grid = f
        .meta
        .as_ref()
        .unwrap()
        .items
        .iter()
        .find(|i| i.item_type == *b"grid");
    assert!(grid.is_some(), "auto grid");
    assert_eq!(
        f.meta
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|i| i.is_hidden())
            .count(),
        4 * 5
    );
    let img = decode(&bytes);
    assert_eq!((img.width(), img.height()), (2048, 2049));
    // Lossless through the grid (the odd height composes at 4:4:4;
    // compare the luma plane and the chroma at the 4:2:0 positions).
    assert!(
        img.frame.planes[0].data == src.planes[0].data,
        "luma through the grid"
    );
    let back = to_yuv420(&img.frame, 8).unwrap();
    assert!(
        back.planes[1].data == src.planes[1].data,
        "cb through the grid"
    );
    assert!(
        back.planes[2].data == src.planes[2].data,
        "cr through the grid"
    );
    let single = encode_still(&picture(2048, 2048, 8, Chroma::Yuv420, false), &opts).unwrap();
    let f = HeifFile::parse(&single).unwrap();
    assert!(f
        .meta
        .as_ref()
        .unwrap()
        .items
        .iter()
        .all(|i| i.item_type != *b"grid"));
}

#[test]
fn writer_streams_the_same_bytes_it_builds() {
    let src = picture(96, 80, 8, Chroma::Yuv420, true);
    let opts = EncodeOptions::default()
        .with_thumbnail_max_dim(Some(32))
        .with_exif(Some(vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 0, 0]));
    let mut w = HeifWriter::new();
    let master = oxideav_heif::encode::encode_still_into(&mut w, &src, &opts).unwrap();
    w.set_primary(master);
    let vec = w.write_to_vec().unwrap();
    let mut streamed = Vec::new();
    w.write_to(&mut streamed).unwrap();
    assert_eq!(vec, streamed);
    assert_eq!(vec, encode_still(&src, &opts).unwrap());
    let owned = encode_still_owned(src.clone(), &opts).unwrap();
    assert_eq!(vec, owned, "owned input codes identically");
    let img = decode(&vec);
    assert!(img.exif.is_some());
    assert_eq!(img.thumbnail_ids.len(), 1);
}

#[test]
fn framework_encoder_honours_the_colour_signal() {
    use oxideav_core::{ColorSignal, VideoFrame, VideoPlane};
    let (w, h) = (64u32, 48u32);
    let planar = |signal: Option<ColorSignal>| -> VideoFrame {
        let src = picture(w, h, 8, Chroma::Yuv420, false);
        let mut vf = VideoFrame {
            pts: Some(0),
            planes: src
                .planes
                .iter()
                .map(|p| VideoPlane {
                    stride: p.stride,
                    data: p.data.clone(),
                })
                .collect(),
        };
        if let Some(s) = signal {
            vf.set_color_signal(s);
        }
        vf
    };
    let encode =
        |vf: VideoFrame, pf: PixelFormat, opts: CodecOptions, stream: Option<ColorSignal>| {
            let mut params = CodecParameters::video(CodecId::new("heif"));
            params.width = Some(w);
            params.height = Some(h);
            params.pixel_format = Some(pf);
            params.options = opts;
            if let Some(s) = stream {
                params.color_signal = s;
            }
            let mut enc = oxideav_heif::encode::make_encoder(&params).unwrap();
            enc.send_frame(&Frame::Video(vf)).unwrap();
            enc.flush().unwrap();
            decode(&enc.receive_packet().unwrap().data).nclx
        };
    let bt2020 = ColorSignal::from_code_points(9, 16, 9, false);
    // A frame record: BT.2020 / PQ / limited lands in the colr.
    let nclx = encode(
        planar(Some(bt2020)),
        PixelFormat::Yuv420P,
        CodecOptions::new(),
        None,
    );
    assert_eq!(
        nclx,
        Colr::Nclx {
            primaries: 9,
            transfer: 16,
            matrix: 9,
            full_range: false
        }
    );
    // The stream-level signal applies when the frame carries none; an
    // explicit `range` option wins over the signalled range.
    let nclx = encode(
        planar(None),
        PixelFormat::Yuv420P,
        CodecOptions::new().set("range", "full"),
        Some(bt2020),
    );
    assert_eq!(
        nclx,
        Colr::Nclx {
            primaries: 9,
            transfer: 16,
            matrix: 9,
            full_range: true
        }
    );
    // No signal at all: the MIAF default.
    let nclx = encode(
        planar(None),
        PixelFormat::Yuv420P,
        CodecOptions::new(),
        None,
    );
    assert_eq!(nclx, Colr::MIAF_DEFAULT);
    // Unspecified code points keep the defaults they stand for.
    let nclx = encode(
        planar(Some(ColorSignal::from_code_points(2, 2, 2, true))),
        PixelFormat::Yuv420P,
        CodecOptions::new(),
        None,
    );
    assert_eq!(nclx, Colr::MIAF_DEFAULT);
    // A packed RGB source: the signal's matrix describes RGB (identity),
    // the conversion matrix stays the default; primaries follow.
    let (mut rgb, pf) = packed_rgb(w, h, false, false);
    rgb.set_color_signal(ColorSignal::from_code_points(12, 13, 0, true));
    let nclx = encode(rgb, pf, CodecOptions::new(), None);
    assert_eq!(
        nclx,
        Colr::Nclx {
            primaries: 12,
            transfer: 13,
            matrix: 6,
            full_range: true
        }
    );
}
