//! The framework path: probe → open → packets → decode through a
//! `RuntimeContext` that has this crate and the codec crates
//! registered.
#![cfg(feature = "registry")]

mod common;

use oxideav_heif::encode::EncodeOptions;

use std::io::Cursor;

use common::{all_bundles, fixture_bytes, fixture_root};
use oxideav_core::{Error, Frame, RuntimeContext};

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    ctx
}

#[test]
fn probe_open_and_decode_every_bundle_through_the_registry() {
    let ctx = context();
    for (root, bundle) in all_bundles() {
        let bytes = fixture_bytes(&root, bundle);
        let mut cur = Cursor::new(bytes.clone());
        let name = ctx.containers.probe_input(&mut cur, Some("heic")).unwrap();
        assert_eq!(name, "heif", "{bundle}");
        let mut demuxer = ctx
            .containers
            .open_demuxer(&name, Box::new(Cursor::new(bytes)), &ctx.codecs)
            .unwrap();
        let streams = demuxer.streams().to_vec();
        assert!(!streams.is_empty(), "{bundle}");
        let still = &streams[0];
        assert_eq!(still.params.codec_id.as_str(), "heif", "{bundle}");
        assert!(
            still.params.width.is_some() && still.params.pixel_format.is_some(),
            "{bundle}"
        );
        let pkt = demuxer.next_packet().unwrap();
        assert_eq!(pkt.stream_index, 0);
        assert!(pkt.is_keyframe());
        let mut dec = ctx.codecs.first_decoder(&still.params).unwrap();
        dec.send_packet(&pkt).unwrap();
        dec.flush().unwrap();
        let Frame::Video(v) = dec.receive_frame().unwrap() else {
            panic!("{bundle}: expected a video frame");
        };
        // The announced geometry / format match the emitted planes.
        let pf = still.params.pixel_format.unwrap();
        let (w, h) = (still.params.width.unwrap(), still.params.height.unwrap());
        assert_eq!(
            v.image_plane_count(),
            pf.plane_count(),
            "{bundle}: plane count for {pf:?}"
        );
        assert_eq!(
            v.planes[0].stride,
            pf.plane_row_bytes(0, w).unwrap(),
            "{bundle}: luma stride for {w}x{h} {pf:?}"
        );
        assert_eq!(
            v.planes[0].data.len(),
            pf.plane_size_bytes(0, w, h).unwrap(),
            "{bundle}"
        );
        assert!(matches!(dec.receive_frame(), Err(Error::Eof)));
        // Sequence tracks follow the still.
        let n_tracks = streams.len() - 1;
        if bundle == "image-sequence-3frame" {
            assert_eq!(n_tracks, 1);
            let s = &streams[1];
            assert_eq!(s.params.codec_id.as_str(), "h265");
            assert!(!s.params.extradata.is_empty());
            let mut vdec = ctx.codecs.first_decoder(&s.params).unwrap();
            let mut count = 0;
            loop {
                match demuxer.next_packet() {
                    Ok(p) => {
                        assert_eq!(p.stream_index, 1);
                        vdec.send_packet(&p).unwrap();
                        count += 1;
                    }
                    Err(Error::Eof) => break,
                    Err(e) => panic!("{e}"),
                }
            }
            assert_eq!(count, 3);
            vdec.flush().unwrap();
            let mut frames = 0;
            while let Ok(f) = vdec.receive_frame() {
                assert!(matches!(f, Frame::Video(_)));
                frames += 1;
            }
            assert_eq!(frames, 3);
            // Seek back to the first sync sample and read again.
            assert_eq!(demuxer.seek_to(1, 0).unwrap(), 0);
            assert!(demuxer.next_packet().is_ok());
            assert!(demuxer.duration_micros().unwrap() > 0);
        } else {
            assert_eq!(n_tracks, 0, "{bundle}");
            // A burst carries its other items as further still packets
            // (pinned against decode_all below); a plain still is one.
            let mut more = 0;
            loop {
                match demuxer.next_packet() {
                    Ok(p) => {
                        assert_eq!(p.stream_index, 0, "{bundle}");
                        assert_eq!(p.pts, Some(more + 1), "{bundle}: pts is the item index");
                        more += 1;
                    }
                    Err(Error::Eof) => break,
                    Err(e) => panic!("{bundle}: {e}"),
                }
            }
            let want = if bundle == "multi-image-burst-3" {
                2
            } else {
                0
            };
            assert_eq!(more, want, "{bundle}: still packets after the primary");
            assert_eq!(streams[0].duration, Some(want + 1), "{bundle}");
        }
        assert!(demuxer.metadata().iter().any(|(k, _)| k == "major_brand"));
    }
}

#[test]
fn extension_hints_and_probe_fallback() {
    let ctx = context();
    assert_eq!(ctx.containers.container_for_extension("heic"), Some("heif"));
    assert_eq!(
        ctx.containers.container_for_extension("heifs"),
        Some("heif")
    );
    // Not a HEIF file at all: the probe declines and there is no
    // extension to fall back to.
    let mut junk = Cursor::new(vec![0u8; 64]);
    assert!(ctx.containers.probe_input(&mut junk, None).is_err());
}

#[test]
fn set_active_streams_skips_the_still() {
    let Some(root) = fixture_root() else {
        return;
    };
    let ctx = context();
    let bytes = fixture_bytes(&root, "image-sequence-3frame");
    let mut d = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    d.set_active_streams(&[1]);
    let p = d.next_packet().unwrap();
    assert_eq!(p.stream_index, 1);
}

/// A full-range still (the MIAF default and what every producer in the
/// interop corpus writes) is announced and emitted with the framework's
/// full-range (`YuvJ*`) layout; a limited-range file keeps `Yuv*`.
#[test]
fn still_stream_carries_the_full_range_pixel_format() {
    use oxideav_core::PixelFormat;
    let ctx = context();
    let open = |bytes: Vec<u8>| {
        let mut d = ctx
            .containers
            .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
            .unwrap();
        let pf = d.streams()[0].params.pixel_format.unwrap();
        let pkt = d.next_packet().unwrap();
        let mut dec = ctx.codecs.first_decoder(&d.streams()[0].params).unwrap();
        dec.send_packet(&pkt).unwrap();
        dec.flush().unwrap();
        let Frame::Video(v) = dec.receive_frame().unwrap() else {
            panic!("video frame");
        };
        (pf, v.image_plane_count())
    };
    let full = std::fs::read(common::interop_root().join("sips_rgb_96x80.heic")).unwrap();
    assert_eq!(open(full), (PixelFormat::YuvJ420P, 3));
    // A limited-range file written by this crate.
    let src = oxideav_heif::HeifFrame::filled(
        32,
        32,
        oxideav_heif::HeifPixelFormat::new(oxideav_heif::Chroma::Yuv420, 8, false).unwrap(),
        128,
    )
    .unwrap();
    let mut opts = EncodeOptions::default().with_hevc_mode("pcm".into());
    opts.colr = oxideav_heif::props::Colr::Nclx {
        primaries: 1,
        transfer: 13,
        matrix: 6,
        full_range: false,
    };
    let limited = oxideav_heif::encode_still(&src, &opts).unwrap();
    assert_eq!(open(limited), (PixelFormat::Yuv420P, 3));
}

/// Identity-matrix items (`matrix_coefficients = 0`, e.g. a black-box
/// producer's lossless RGB) are announced and emitted as planar RGB
/// (`Gbrp*`) with the H.273 signal on the stream and on each frame;
/// YCbCr items keep their YCbCr labels. The label follows the black-box
/// decoder's own reading recorded in the interop manifest.
#[test]
fn identity_matrix_items_are_planar_rgb_with_their_colour_signal() {
    use oxideav_core::{ColorRange, MatrixCoefficients, PixelFormat};
    let ctx = context();
    let manifest = std::fs::read_to_string(common::interop_root().join("manifest.tsv")).unwrap();
    let mut gbr_seen = 0;
    for line in manifest.lines().filter(|l| !l.starts_with('#')) {
        let cols: Vec<&str> = line.split('\t').collect();
        let (file, bb) = (cols[0], cols[7]);
        if bb == "-" || !(file.ends_with(".heic") || file.ends_with(".avif")) {
            continue;
        }
        let bytes = std::fs::read(common::interop_root().join(file)).unwrap();
        let mut demuxer = ctx
            .containers
            .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
            .unwrap();
        let still = demuxer.streams()[0].clone();
        let pf = still.params.pixel_format.unwrap();
        let signal = still.params.color_signal;
        let expected_gbr = match bb {
            "gbrp" => Some(PixelFormat::Gbrp8),
            "gbrp10le" => Some(PixelFormat::Gbrp10Le),
            "gbrp12le" => Some(PixelFormat::Gbrp12Le),
            _ => None,
        };
        match expected_gbr {
            Some(g) => {
                gbr_seen += 1;
                assert_eq!(pf, g, "{file}");
                assert_eq!(signal.matrix, MatrixCoefficients(0), "{file}");
            }
            None => {
                assert!(
                    PixelFormat::Gbrp8 != pf && PixelFormat::Gbrp10Le != pf,
                    "{file}: {pf:?}"
                );
                assert_ne!(signal.matrix, MatrixCoefficients(0), "{file}");
            }
        }
        assert_ne!(signal.range, ColorRange::Unspecified, "{file}");
        let pkt = demuxer.next_packet().unwrap();
        let mut dec = ctx.codecs.first_decoder(&still.params).unwrap();
        dec.send_packet(&pkt).unwrap();
        let Frame::Video(v) = dec.receive_frame().unwrap() else {
            panic!("{file}: video frame");
        };
        assert_eq!(v.color_signal(), Some(signal), "{file}: frame signal");
        assert_eq!(v.image_plane_count(), pf.plane_count(), "{file}");
    }
    assert!(gbr_seen >= 2, "the manifest carries identity-matrix files");
}

/// The encoder takes planar RGB in: AV1 codes it as an identity-matrix
/// 4:4:4 item (lossless stays exact through the demuxer as `Gbrp8`),
/// HEVC converts through the configured matrix.
#[test]
fn planar_rgb_encodes_as_identity_matrix_items() {
    use oxideav_core::{CodecId, CodecParameters, PixelFormat, VideoFrame, VideoPlane};
    let ctx = context();
    let (w, h) = (48u32, 32u32);
    let plane = |f: fn(u32, u32) -> u8| VideoPlane {
        stride: w as usize,
        data: (0..h).flat_map(|y| (0..w).map(move |x| f(x, y))).collect(),
    };
    let src = VideoFrame {
        pts: Some(0),
        planes: vec![
            plane(|x, y| (x * 5 + y) as u8),
            plane(|x, y| (y * 7 + x * 2) as u8),
            plane(|x, _| (255 - x * 3) as u8),
        ],
    };
    for (codec, mode) in [("av1", "pcm"), ("hevc", "intra")] {
        let mut params = CodecParameters::video(CodecId::new("heif"));
        params.width = Some(w);
        params.height = Some(h);
        params.pixel_format = Some(PixelFormat::Gbrp8);
        params.options = oxideav_core::CodecOptions::new()
            .set("codec", codec)
            .set("mode", mode);
        let mut enc = ctx.codecs.first_encoder(&params).unwrap();
        enc.send_frame(&Frame::Video(src.clone())).unwrap();
        enc.flush().unwrap();
        let file = enc.receive_packet().unwrap().data;
        let mut demuxer = ctx
            .containers
            .open_demuxer("heif", Box::new(Cursor::new(file)), &ctx.codecs)
            .unwrap();
        let still = demuxer.streams()[0].clone();
        let pkt = demuxer.next_packet().unwrap();
        let mut dec = ctx.codecs.first_decoder(&still.params).unwrap();
        dec.send_packet(&pkt).unwrap();
        let Frame::Video(v) = dec.receive_frame().unwrap() else {
            panic!("video frame");
        };
        if codec == "av1" {
            assert_eq!(still.params.pixel_format, Some(PixelFormat::Gbrp8));
            for p in 0..3 {
                assert_eq!(v.planes[p].data, src.planes[p].data, "plane {p} exact");
            }
        } else {
            assert_eq!(still.params.pixel_format, Some(PixelFormat::YuvJ420P));
            assert_eq!(still.params.color_signal.matrix.0, 6);
        }
    }
}

// ---------------------------------------------------------------------
// Registry path == Layer 1 (`decode_all`) for bursts and sequences
// ---------------------------------------------------------------------

use std::collections::VecDeque;
use std::time::Duration;

use oxideav_core::{Decoder, Demuxer, StreamInfo, TimeBase};
use oxideav_heif::{decode_all, HeifDemuxer, HeifImage};

/// Every packet of `stream` through the decoder the registry resolves
/// for it: the frames as `HeifImage`s labelled by the stream
/// parameters, each with its packet's `(pts, duration)`.
#[allow(clippy::type_complexity)]
fn pump_stream(
    ctx: &RuntimeContext,
    bytes: Vec<u8>,
    stream: u32,
) -> (StreamInfo, Vec<(HeifImage, Option<i64>, Option<i64>)>) {
    let mut d = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let info = d.streams()[stream as usize].clone();
    let mut dec = ctx.codecs.first_decoder(&info.params).unwrap();
    let mut out = Vec::new();
    let mut timing: VecDeque<(Option<i64>, Option<i64>)> = VecDeque::new();
    fn drain(
        dec: &mut dyn Decoder,
        params: &oxideav_core::CodecParameters,
        timing: &mut VecDeque<(Option<i64>, Option<i64>)>,
        out: &mut Vec<(HeifImage, Option<i64>, Option<i64>)>,
    ) {
        loop {
            match dec.receive_frame() {
                Ok(Frame::Video(v)) => {
                    let (pts, dur) = timing.pop_front().unwrap_or((None, None));
                    out.push((HeifImage::from_video_frame(&v, params).unwrap(), pts, dur));
                }
                Ok(_) => {}
                Err(Error::NeedMore) | Err(Error::Eof) => return,
                Err(e) => panic!("decode: {e}"),
            }
        }
    }
    loop {
        match d.next_packet() {
            Ok(p) if p.stream_index == stream => {
                timing.push_back((p.pts, p.duration));
                dec.send_packet(&p).unwrap();
                drain(dec.as_mut(), &info.params, &mut timing, &mut out);
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("demux: {e}"),
        }
    }
    dec.flush().unwrap();
    drain(dec.as_mut(), &info.params, &mut timing, &mut out);
    (info, out)
}

/// Byte-for-byte equality of two images' visible samples (layout,
/// geometry and every plane row; the label's range flavour is not
/// compared, strides may differ).
fn same_pixels(a: &HeifImage, b: &HeifImage) -> Result<(), String> {
    let fa = a.to_frame().map_err(|e| e.to_string())?;
    let fb = b.to_frame().map_err(|e| e.to_string())?;
    if (fa.width, fa.height, fa.format) != (fb.width, fb.height, fb.format) {
        return Err(format!(
            "{}x{} {:?} vs {}x{} {:?}",
            fa.width, fa.height, fa.format, fb.width, fb.height, fb.format
        ));
    }
    for p in 0..fa.format.plane_count() {
        let (_, rows) = fa.plane_dims(p);
        for y in 0..rows {
            if fa.row(p, y) != fb.row(p, y) {
                return Err(format!("plane {p} row {y} differs"));
            }
        }
    }
    Ok(())
}

/// The still-item delay Layer 1 derives from a sample duration.
fn track_delay(ticks: i64, tb: TimeBase) -> Duration {
    let d = ticks as u64;
    let ts = tb.0.den as u64;
    Duration::new(d / ts, ((d % ts) * 1_000_000_000 / ts) as u32)
}

/// Stream 0 carries one packet per displayable image item — the
/// primary first, then the burst — and each decodes to the very
/// planes `decode_all` returns for that item, over every vendored
/// bundle and every real-world interop file. Item packets are untimed:
/// `pts` = item index, duration 1, time base 1/1.
#[test]
fn still_stream_carries_every_displayable_item_byte_exact_with_decode_all() {
    let ctx = context();
    let mut files: Vec<(String, Vec<u8>)> = all_bundles()
        .into_iter()
        .map(|(root, b)| (b.to_string(), fixture_bytes(&root, b)))
        .collect();
    let mut interop: Vec<_> = std::fs::read_dir(common::interop_root())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("heic" | "avif")
            )
        })
        .collect();
    interop.sort();
    for p in interop {
        files.push((
            p.file_name().unwrap().to_string_lossy().into_owned(),
            std::fs::read(&p).unwrap(),
        ));
    }
    assert!(files.len() >= 13 + 30);
    let mut bursts = 0;
    for (name, bytes) in files {
        let items: Vec<_> = decode_all(&bytes)
            .unwrap_or_else(|e| panic!("{name}: decode_all: {e}"))
            .into_iter()
            .filter(|f| f.item_id.is_some())
            .collect();
        let direct = HeifDemuxer::from_bytes(bytes.clone(), &ctx.codecs).unwrap();
        assert_eq!(
            direct.still_items().len(),
            items.len(),
            "{name}: still items vs decode_all item frames"
        );
        assert_eq!(
            direct.still_items().to_vec(),
            items.iter().map(|f| f.item_id.unwrap()).collect::<Vec<_>>(),
            "{name}: item order"
        );
        let ids = items
            .iter()
            .map(|f| f.item_id.unwrap().to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            direct
                .metadata()
                .contains(&("stream:0:item_ids".to_string(), ids)),
            "{name}: item_ids metadata"
        );
        let (info, got) = pump_stream(&ctx, bytes, 0);
        assert_eq!(got.len(), items.len(), "{name}: still packets decoded");
        assert_eq!(info.time_base, TimeBase::new(1, 1), "{name}");
        assert_eq!(info.duration, Some(items.len() as i64), "{name}");
        for (i, ((img, pts, dur), want)) in got.iter().zip(&items).enumerate() {
            assert_eq!(*pts, Some(i as i64), "{name} item {i}: pts is the index");
            assert_eq!(*dur, Some(1), "{name} item {i}: duration 1");
            same_pixels(img, &want.image).unwrap_or_else(|e| panic!("{name} item {i}: {e}"));
            assert_eq!(img.color, want.image.color, "{name} item {i}: colour");
        }
        if items.len() > 1 {
            bursts += 1;
        }
    }
    assert!(bursts >= 1, "the corpus carries a burst");
}

/// The burst bundle: three items, three still packets, each matching
/// its own `expected_<i>.png` through the registry; `max_frames`-style
/// consumers reading one packet get the primary (`decode`).
#[test]
fn burst_items_match_their_oracles_and_the_primary_comes_first() {
    let Some(root) = fixture_root() else {
        return;
    };
    let ctx = context();
    let bytes = fixture_bytes(&root, "multi-image-burst-3");
    let (_, got) = pump_stream(&ctx, bytes.clone(), 0);
    assert_eq!(got.len(), 3);
    let primary = oxideav_heif::decode(&bytes).unwrap();
    same_pixels(&got[0].0, &primary).unwrap();
    for (i, (img, _, _)) in got.iter().enumerate() {
        let png = common::png::read_png(
            &std::fs::read(
                root.join("multi-image-burst-3")
                    .join(format!("expected_{i}.png")),
            )
            .unwrap(),
        );
        let rgba = img.to_rgba8();
        let (mut sum, mut max) = (0u64, 0u32);
        for y in 0..img.height {
            for x in 0..img.width {
                for c in 0..3 {
                    let ours = rgba[((y * img.width + x) * 4 + c) as usize] as i32;
                    let v = png.sample(x, y, c as usize);
                    let theirs = if png.bit_depth == 16 {
                        (v as f64 / 257.0).round() as i32
                    } else {
                        v as i32
                    };
                    let d = (ours - theirs).unsigned_abs();
                    sum += d as u64;
                    max = max.max(d);
                }
            }
        }
        let mean = sum as f64 / (img.width * img.height * 3) as f64;
        assert!(
            mean <= 1.5 && max <= 12,
            "item {i}: mean {mean:.3} max {max}"
        );
    }
    // Seeking the still stream lands on the item index.
    let mut d = HeifDemuxer::from_bytes(bytes, &ctx.codecs).unwrap();
    assert_eq!(d.seek_to(0, 2).unwrap(), 2);
    assert_eq!(d.next_packet().unwrap().pts, Some(2));
    assert!(matches!(d.next_packet(), Err(Error::Eof)));
    assert_eq!(d.seek_to(0, 99).unwrap(), 2);
}

/// Image-sequence tracks come out one packet per sample, timed from
/// the sample table in the track's time base, and decode to the planes
/// `decode_all` returns for those samples with the same delays — the
/// raw `h265` track of the corpus sequence and the two real-world
/// `.heics` (the alpha one through the composed `"heif"` stream).
#[test]
fn sequence_streams_match_decode_all_frames_and_delays() {
    let ctx = context();
    let cases = [
        common::vendored_root().join("image-sequence-3frame/input.heic"),
        common::interop_root().join("sips_seq_rgb_96x80.heics"),
        common::interop_root().join("sips_seq_rgba_96x80.heics"),
    ];
    for path in cases {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        let want: Vec<_> = decode_all(&bytes)
            .unwrap_or_else(|e| panic!("{name}: decode_all: {e}"))
            .into_iter()
            .filter(|f| f.track_id.is_some())
            .collect();
        assert!(!want.is_empty(), "{name}: sequence frames");
        let direct = HeifDemuxer::from_bytes(bytes.clone(), &ctx.codecs).unwrap();
        let streams = direct.streams().to_vec();
        let still = streams
            .iter()
            .position(|s| s.params.codec_id.as_str() == "heif");
        // The composed (alpha) "heif" stream when there is one after
        // the still, else the first raw visual track.
        let stream = streams
            .iter()
            .filter(|s| Some(s.index as usize) != still)
            .find(|s| s.params.codec_id.as_str() == "heif")
            .or_else(|| streams.iter().find(|s| Some(s.index as usize) != still))
            .unwrap_or_else(|| panic!("{name}: no track stream"))
            .index;
        let (info, got) = pump_stream(&ctx, bytes, stream);
        assert_eq!(got.len(), want.len(), "{name}: track frames");
        let mut last_pts = None;
        for (i, ((img, pts, dur), w)) in got.iter().zip(&want).enumerate() {
            same_pixels(img, &w.image).unwrap_or_else(|e| panic!("{name} sample {i}: {e}"));
            let delay = track_delay(
                dur.unwrap_or_else(|| panic!("{name} sample {i}: duration")),
                info.time_base,
            );
            assert_eq!(Some(delay), w.delay, "{name} sample {i}: delay");
            assert!(delay > Duration::ZERO, "{name} sample {i}");
            let p = pts.unwrap_or_else(|| panic!("{name} sample {i}: pts"));
            assert!(
                !matches!(last_pts, Some(l) if p <= l),
                "{name} sample {i}: pts increases"
            );
            last_pts = Some(p);
        }
    }
}

/// `mode=pcm` (lossless) with a packed RGB / RGBA source codes the
/// pixels as an identity-matrix 4:4:4 item (G, B, R planes,
/// `matrix_coefficients = 0`, full range) on both codecs, so the file
/// says what it holds and the round trip is exact — through the
/// registry (`Gbrp8` / `Gbrap8` stream + frames) and through Layer 1
/// (`decode`, `to_rgba8`). Lossy coding keeps the 4:2:0 YCbCr default.
#[test]
fn lossless_rgb_round_trips_exactly_as_identity_matrix_items() {
    use oxideav_core::{
        CodecId, CodecParameters, ColorRange, MatrixCoefficients, PixelFormat, VideoFrame,
        VideoPlane,
    };
    let ctx = context();
    let (w, h) = (9u32, 7u32);
    let rgba: Vec<u8> = (0..h)
        .flat_map(|y| {
            (0..w).flat_map(move |x| {
                [
                    (x * 255 / w) as u8,
                    (y * 255 / h) as u8,
                    ((x + y) * 7) as u8,
                    if (x + y) % 3 == 0 {
                        255
                    } else {
                        (x * 40 + y * 17) as u8
                    },
                ]
            })
        })
        .collect();
    let rgb: Vec<u8> = rgba.chunks(4).flat_map(|p| p[..3].to_vec()).collect();
    let opaque: Vec<u8> = rgba
        .chunks(4)
        .flat_map(|p| [p[0], p[1], p[2], 255])
        .collect();
    for codec in ["hevc", "av1"] {
        for (pf, bpp, src, want_pf, want_rgba) in [
            (PixelFormat::Rgba, 4usize, &rgba, PixelFormat::Gbrap8, &rgba),
            (PixelFormat::Rgb24, 3, &rgb, PixelFormat::Gbrp8, &opaque),
        ] {
            let mut params = CodecParameters::video(CodecId::new("heif"));
            params.width = Some(w);
            params.height = Some(h);
            params.pixel_format = Some(pf);
            params.options = oxideav_core::CodecOptions::new()
                .set("codec", codec)
                .set("mode", "pcm");
            let mut enc = ctx.codecs.first_encoder(&params).unwrap();
            enc.send_frame(&Frame::Video(VideoFrame {
                pts: Some(0),
                planes: vec![VideoPlane {
                    stride: w as usize * bpp,
                    data: src.clone(),
                }],
            }))
            .unwrap();
            enc.flush().unwrap();
            let file = enc.receive_packet().unwrap().data;
            let tag = format!("{codec} {pf:?}");
            // Layer 1.
            let img = oxideav_heif::decode(&file).unwrap();
            assert_eq!(
                img.format,
                oxideav_heif::PixelFormat::try_from(want_pf).unwrap(),
                "{tag}"
            );
            assert_eq!(img.color.matrix, 0, "{tag}: identity matrix signalled");
            assert_eq!(img.color.range, oxideav_heif::ColorRange::Full, "{tag}");
            assert_eq!(&img.to_rgba8(), want_rgba, "{tag}: Layer 1 exact");
            assert_eq!(
                oxideav_heif::info(&file).unwrap().format,
                img.format,
                "{tag}"
            );
            // Registry.
            let (info, got) = pump_stream(&ctx, file, 0);
            assert_eq!(
                info.params.pixel_format,
                Some(want_pf),
                "{tag}: stream label"
            );
            assert_eq!(
                info.params.color_signal.matrix,
                MatrixCoefficients(0),
                "{tag}"
            );
            assert_eq!(info.params.color_signal.range, ColorRange::Full, "{tag}");
            assert_eq!(got.len(), 1);
            let back = &got[0].0;
            assert_eq!(&back.to_rgba8(), want_rgba, "{tag}: registry exact");
            // The planes are G, B, R(, A) of the source, untouched.
            let f = back.to_frame().unwrap();
            for y in 0..h {
                for x in 0..w {
                    let px = &src[((y * w + x) as usize) * bpp..][..bpp];
                    assert_eq!(f.sample(0, x, y), px[1] as u16, "{tag}: G");
                    assert_eq!(f.sample(1, x, y), px[2] as u16, "{tag}: B");
                    assert_eq!(f.sample(2, x, y), px[0] as u16, "{tag}: R");
                    if bpp == 4 {
                        assert_eq!(f.sample(3, x, y), px[3] as u16, "{tag}: A");
                    }
                }
            }
        }
    }
    // The standalone one-call path under the lossless mode.
    let pcm = EncodeOptions::default().with_hevc_mode("pcm".into());
    let file = oxideav_heif::encode_rgba8(w, h, &rgba, &pcm).unwrap();
    let img = oxideav_heif::decode(&file).unwrap();
    assert_eq!(img.format, oxideav_heif::PixelFormat::Gbrap8);
    assert_eq!(img.to_rgba8(), rgba);
    // Lossy coding is unchanged: 4:2:0 YCbCr through the BT.601 default
    // (an even size: odd sizes promote to 4:4:4 through their `clap`).
    let even: Vec<u8> = (0..16 * 16 * 3).map(|i| (i * 7) as u8).collect();
    let lossy = oxideav_heif::encode_rgb8(16, 16, &even, &EncodeOptions::default()).unwrap();
    let img = oxideav_heif::decode(&lossy).unwrap();
    assert_eq!(img.format, oxideav_heif::PixelFormat::Yuv420P);
    assert_eq!(img.color.matrix, 6);
    assert_eq!(oxideav_heif::info(&lossy).unwrap().format, img.format);
}
