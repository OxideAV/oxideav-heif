//! Image-sequence tracks: the sample-table walk against the corpus
//! sequence bundle, decoded through the framework codec (`registry`)
//! and cross-checked with a black-box decoder and the per-frame PNG
//! oracles.

mod common;

use common::{fixture_bytes, fixture_root};
use oxideav_heif::sequence::{parse_movie, sample_bytes};
use oxideav_heif::HeifFile;

#[test]
fn sequence_bundle_sample_table() {
    let Some(root) = fixture_root() else {
        return;
    };
    let f = HeifFile::from_vec(fixture_bytes(&root, "image-sequence-3frame")).unwrap();
    let mv = parse_movie(&f).unwrap().expect("moov");
    assert_eq!(mv.tracks.len(), 1);
    let t = &mv.tracks[0];
    assert_eq!(&t.handler, b"pict");
    assert!(t.enabled && t.in_movie);
    assert_eq!(t.samples.len(), 3);
    assert_eq!((t.width, t.height), (96, 96));
    let e = t.primary_entry().unwrap();
    assert_eq!(&e.entry_type, b"hvc1");
    assert_eq!((e.width, e.height), (96, 96));
    let h = e.hvcc.as_ref().expect("hvcC in the sample entry");
    assert_eq!(h.chroma_format_idc, 1);
    assert!(
        e.ccst.is_some(),
        "ccst is mandatory for pict tracks (§7.2.3)"
    );
    assert_eq!(t.orientation().unwrap().rotation, 0);
    // Samples are contiguous, in decode order, and inside the file.
    let mut dts = 0;
    for s in &t.samples {
        assert_eq!(s.dts, dts);
        dts += s.duration as u64;
        assert!(s.size > 0);
        assert!(sample_bytes(&f, s).is_ok());
        // Each sample is a run of length-prefixed NAL units.
        let nals =
            oxideav_heif::hvcc::split_length_prefixed(sample_bytes(&f, s).unwrap(), h.length_size)
                .unwrap();
        assert!(!nals.is_empty());
    }
    assert_eq!(t.sample_duration_total(), t.duration);
    assert!(t.samples[0].is_sync);
    // No sequence in the still-only bundles.
    let f = HeifFile::from_vec(fixture_bytes(&root, "single-image-1x1")).unwrap();
    assert!(parse_movie(&f).unwrap().is_none());
}

#[cfg(feature = "registry")]
#[test]
fn sequence_frames_decode_and_match_oracles() {
    use common::png::read_png;
    use oxideav_core::{CodecParameters, Error, Frame, Packet, TimeBase};

    let Some(root) = fixture_root() else {
        return;
    };
    let bytes = fixture_bytes(&root, "image-sequence-3frame");
    let f = HeifFile::parse(&bytes).unwrap();
    let mv = parse_movie(&f).unwrap().unwrap();
    let t = &mv.tracks[0];
    let e = t.primary_entry().unwrap();
    let h = e.hvcc.as_ref().unwrap();
    let mut params = CodecParameters::video(oxideav_core::CodecId::new("h265"));
    params.extradata = h.raw.clone();
    params.width = Some(e.width as u32);
    params.height = Some(e.height as u32);
    let mut dec = oxideav_h265::make_decoder(&params).unwrap();
    let tb = TimeBase::new(1, t.timescale as i64);
    for s in &t.samples {
        let pkt = Packet::new(0, tb, sample_bytes(&f, s).unwrap().to_vec())
            .with_pts(s.pts() as i64)
            .with_keyframe(s.is_sync);
        dec.send_packet(&pkt).unwrap();
    }
    dec.flush().unwrap();
    let mut frames = Vec::new();
    loop {
        match dec.receive_frame() {
            Ok(Frame::Video(v)) => frames.push(v),
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(frames.len(), 3);
    // Black-box: the whole track as raw planes.
    let out = std::env::temp_dir().join(format!("oxideav-heif-seq-{}.raw", std::process::id()));
    let st = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(root.join("image-sequence-3frame").join("input.heic"))
        .args(["-f", "rawvideo"])
        .arg(&out)
        .status();
    if let Ok(st) = st {
        if st.success() {
            let raw = std::fs::read(&out).unwrap();
            let _ = std::fs::remove_file(&out);
            let mut ours = Vec::new();
            for v in &frames {
                for p in v.image_planes() {
                    ours.extend_from_slice(&p.data);
                }
            }
            assert_eq!(ours.len(), raw.len(), "three 96x96 4:2:0 frames");
            assert_eq!(ours.iter().zip(&raw).filter(|(a, b)| a != b).count(), 0);
        }
    }
    // Per-frame PNG oracles (nearest-chroma RGB, tolerance as in e2e).
    for (i, v) in frames.iter().enumerate() {
        let png = read_png(
            &std::fs::read(
                root.join("image-sequence-3frame")
                    .join(format!("expected_{i}.png")),
            )
            .unwrap(),
        );
        assert_eq!((png.width, png.height), (96, 96));
        let mut max = 0i32;
        for y in 0..96u32 {
            for x in 0..96u32 {
                let yv = v.planes[0].data[(y as usize) * v.planes[0].stride + x as usize] as f64;
                let cb = v.planes[1].data[(y as usize / 2) * v.planes[1].stride + x as usize / 2]
                    as f64
                    - 128.0;
                let cr = v.planes[2].data[(y as usize / 2) * v.planes[2].stride + x as usize / 2]
                    as f64
                    - 128.0;
                let rgb = [
                    yv + 1.402 * cr,
                    yv - 0.344136 * cb - 0.714136 * cr,
                    yv + 1.772 * cb,
                ];
                for (c, v) in rgb.iter().enumerate() {
                    let d = (v.round().clamp(0.0, 255.0) as i32 - png.sample(x, y, c) as i32).abs();
                    max = max.max(d);
                }
            }
        }
        assert!(max <= 12, "frame {i}: max diff {max}");
    }
}

/// An Apple ImageIO image sequence with an alpha auxiliary track
/// (`auxv` handler, `auxl` reference, `auxi` written as a plain Box —
/// a shape libheif 1.23 refuses): the container surfaces the link, and
/// the framework demuxer composes master + time-parallel alpha into a
/// `"heif"` stream whose frames carry alpha. Pixels are pinned to the
/// black-box video decoder's raw output of both tracks (FNV-1a 64).
#[cfg(feature = "registry")]
#[test]
fn apple_alpha_sequence_composes_alpha_through_the_demuxer() {
    use common::{fnv1a64, interop_root};
    use oxideav_core::{Decoder, Frame};
    use oxideav_heif::props::AuxKind;
    let bytes = std::fs::read(interop_root().join("sips_seq_rgba_96x80.heics")).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let mv = parse_movie(&f).unwrap().expect("moov");
    assert_eq!(mv.tracks.len(), 2);
    let alpha = mv.alpha_track_of(1).expect("alpha track linked by auxl");
    assert_eq!(alpha.track_id, 2);
    assert_eq!(&alpha.handler, b"auxv");
    assert!(
        !alpha.in_movie,
        "§7.5.3.1: auxiliary tracks should not be in_movie"
    );
    assert_eq!(alpha.aux_kind(), Some(AuxKind::Alpha));
    assert_eq!(
        alpha.primary_entry().unwrap().aux_track_type.as_deref(),
        Some("urn:mpeg:hevc:2015:auxid:1")
    );
    assert_eq!(alpha.sample_index_at(0, mv.tracks[0].timescale), Some(0));
    // Demuxer: still, composed (heif), raw master, raw alpha.
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let mut d = ctx
        .containers
        .open_demuxer("heif", Box::new(std::io::Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let streams = d.streams().to_vec();
    assert_eq!(streams.len(), 4, "{streams:#?}");
    assert_eq!(streams[0].params.codec_id.as_str(), "heif");
    assert_eq!(
        streams[1].params.codec_id.as_str(),
        "heif",
        "composed stream"
    );
    assert!(streams[1].params.pixel_format.unwrap().has_alpha());
    assert_eq!(streams[2].params.codec_id.as_str(), "h265");
    assert_eq!(streams[3].params.codec_id.as_str(), "h265");
    assert!(d
        .metadata()
        .iter()
        .any(|(k, v)| k == "track:1:alpha_track" && v == "2"));
    let mut composed = None;
    let mut raw_master = None;
    let mut raw_alpha = None;
    while let Ok(p) = d.next_packet() {
        match p.stream_index {
            1 => composed = Some(p),
            2 => raw_master = Some(p),
            3 => raw_alpha = Some(p),
            _ => {}
        }
    }
    let composed = composed.expect("composed packet");
    let mut codec = oxideav_heif::demux::HeifCodec::new(oxideav_core::CodecId::new("heif"));
    codec.send_packet(&composed).unwrap();
    let Frame::Video(vf) = codec.receive_frame().unwrap() else {
        panic!("video frame");
    };
    assert_eq!(vf.planes.len(), 4, "Y Cb Cr A");
    let img = codec.last_image().unwrap();
    assert_eq!((img.width(), img.height()), (96, 80));
    assert!(img.frame.format.has_alpha);
    assert_eq!(img.frame.format.bit_depth, 10);
    // Master planes: byte-exact against the black-box decoder (yuv420p10le).
    let master: Vec<u8> = img
        .frame
        .without_alpha()
        .tight()
        .planes
        .iter()
        .flat_map(|p| p.data.iter().copied())
        .collect();
    assert_eq!(master.len(), 23040);
    assert_eq!(fnv1a64(&master), 0xfa2dc301e1fd4945, "master planes");
    // Raw alpha track through the HEVC decoder: byte-exact (gray).
    let mut dec = ctx.codecs.first_decoder(&streams[3].params).unwrap();
    dec.send_packet(&raw_alpha.unwrap()).unwrap();
    dec.flush().unwrap();
    let Frame::Video(av) = dec.receive_frame().unwrap() else {
        panic!("alpha frame");
    };
    let a8: Vec<u8> = (0..80)
        .flat_map(|y| {
            av.planes[0].data[y * av.planes[0].stride..y * av.planes[0].stride + 96].to_vec()
        })
        .collect();
    assert_eq!(fnv1a64(&a8), 0xc228547e6c918dc5, "alpha plane");
    // The composed alpha is that plane depth-matched to the 10-bit master.
    let a_plane = img.frame.format.alpha_plane().unwrap();
    for y in 0..80u32 {
        for x in 0..96u32 {
            let v8 = a8[(y * 96 + x) as usize] as u32;
            let want = (v8 * 1023 + 127) / 255;
            assert_eq!(
                img.frame.sample(a_plane, x, y) as u32,
                want,
                "alpha at {x},{y}"
            );
        }
    }
    let _ = raw_master;
}
