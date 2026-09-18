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
    let f = HeifFile::parse(&fixture_bytes(&root, "image-sequence-3frame")).unwrap();
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
    let f = HeifFile::parse(&fixture_bytes(&root, "single-image-1x1")).unwrap();
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
