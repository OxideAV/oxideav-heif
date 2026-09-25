//! The framework path: probe → open → packets → decode through a
//! `RuntimeContext` that has this crate and the codec crates
//! registered.
#![cfg(feature = "registry")]

mod common;

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
            assert!(matches!(demuxer.next_packet(), Err(Error::Eof)), "{bundle}");
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
    let mut opts = oxideav_heif::EncodeOptions {
        hevc_mode: "pcm".into(),
        ..Default::default()
    };
    opts.colr = oxideav_heif::props::Colr::Nclx {
        primaries: 1,
        transfer: 13,
        matrix: 6,
        full_range: false,
    };
    let limited = oxideav_heif::encode_still(&src, &opts).unwrap();
    assert_eq!(open(limited), (PixelFormat::Yuv420P, 3));
}
