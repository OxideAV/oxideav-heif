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
    // Demuxer: the cover still (stream 0), then composed (heif), raw
    // master, raw alpha.
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let mut d = ctx
        .containers
        .open_demuxer("heif", Box::new(std::io::Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let streams = d.streams().to_vec();
    assert_eq!(streams.len(), 4, "{streams:#?}");
    assert_eq!(streams[0].params.codec_id.as_str(), "heif", "cover still");
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
    assert_eq!(vf.image_plane_count(), 4, "Y Cb Cr A");
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

/// Sequence-level metadata both ways (HEIF §8.3, Annex A.2.2 / A.3):
/// the `SequenceWriter` writes the whole-sequence Exif / XMP as items of
/// the master track's `MetaBox` and per-picture metadata as timed
/// `Exif` / `metx` tracks linked by `cdsc`; the reader parses the track
/// `meta` and the metadata tracks, the contract `decode_all` puts the
/// sequence metadata on every frame refined by the time-parallel
/// samples, the framework demuxer puts it on the track's stream
/// parameters, surfaces the metadata tracks as `Data` streams and
/// composes a `"heif"` stream whose frames carry the per-picture blobs;
/// every black-box opener still accepts the file.
#[cfg(feature = "registry")]
#[test]
fn sequence_level_metadata_round_trips() {
    use std::io::Cursor;

    use oxideav_core::{BlobKind, Error, Frame, RuntimeContext};
    use oxideav_heif::encode::encode_hevc_picture;
    use oxideav_heif::image::Chroma;
    use oxideav_heif::props::{Colr, Ispe, Pixi, Property};
    use oxideav_heif::sequence::MetadataTrackKind;
    use oxideav_heif::writer::{
        SequenceMetadataTrack, SequenceSample, SequenceWriter, XMP_NAMESPACE,
    };
    use oxideav_heif::{HeifFrame, HeifPixelFormat};

    let picture = |i: u32| {
        let mut f = HeifFrame::zeroed(
            32,
            32,
            HeifPixelFormat::new(Chroma::Yuv420, 8, false).unwrap(),
        )
        .unwrap();
        for y in 0..32 {
            for x in 0..32 {
                f.set_sample(0, x, y, ((x * 3 + y * 5 + i * 40) % 256) as u16);
            }
        }
        f
    };
    let pictures: Vec<HeifFrame> = (0..3).map(picture).collect();
    let coded: Vec<_> = pictures
        .iter()
        .map(|p| encode_hevc_picture(p, "pcm", 0).unwrap())
        .collect();
    let seq_exif = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x01\0\0\0\0\0\0".to_vec();
    let seq_xmp = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF/></x:xmpmeta>".to_vec();
    let exif_of = |i: u8| {
        let mut e = seq_exif.clone();
        e[18] = i + 2;
        e
    };
    let xmp_of =
        |i: u8| format!("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" frame=\"{i}\"/>").into_bytes();
    let mut sw = SequenceWriter::new(*b"hvc1", coded[0].config.clone(), 32, 32, 10)
        .with_exif(Some(seq_exif.clone()))
        .with_xmp(Some(seq_xmp.clone()));
    for c in &coded {
        sw.push_sample(c.data.clone(), 1, true);
    }
    // A cover still (MIAF needs the file-level meta) with its own Exif
    // / XMP items — the ids the track-level meta must steer clear of
    // for readers that pool every MetaBox's items.
    let mut still = oxideav_heif::HeifWriter::new();
    let cover = still.add_coded_item(
        *b"hvc1",
        coded[0].data.clone(),
        vec![
            (coded[0].config.clone(), true),
            (Property::Ispe(Ispe::new(32, 32)), false),
            (Property::Pixi(Pixi::new(vec![8, 8, 8])), false),
            (Property::Colr(Colr::MIAF_DEFAULT), false),
        ],
    );
    still.add_exif(cover, &seq_exif);
    still.add_xmp_bytes(cover, seq_xmp.clone());
    still.set_primary(cover);
    sw.still = Some(still);
    sw.cover_sample = Some(0);
    sw.push_metadata_track(SequenceMetadataTrack::new(
        MetadataTrackKind::Exif,
        10,
        (0..3u8)
            .map(|i| SequenceSample::new(exif_of(i), 1, true))
            .collect(),
    ));
    // Two XMP samples: the second covers pictures 1 and 2.
    sw.push_metadata_track(SequenceMetadataTrack::new(
        MetadataTrackKind::Xmp,
        10,
        vec![
            SequenceSample::new(xmp_of(0), 1, true),
            SequenceSample::new(xmp_of(1), 2, true),
        ],
    ));
    let bytes = sw.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let mv = parse_movie(&f).unwrap().expect("moov");
    assert_eq!(mv.tracks.len(), 3);
    let master = &mv.tracks[0];
    let meta = master.meta.as_ref().expect("track-level meta");
    assert_eq!(meta.items.len(), 2);
    assert_eq!(
        meta.handler.as_ref().map(|h| h.handler_type),
        Some(*b"pict")
    );
    let file_ids: Vec<u32> = f.meta().unwrap().items.iter().map(|i| i.id).collect();
    assert!(
        meta.items.iter().all(|i| !file_ids.contains(&i.id)),
        "track-level item ids {:?} steer clear of the file-level {file_ids:?}",
        meta.items.iter().map(|i| i.id).collect::<Vec<_>>()
    );
    assert_eq!(
        master.sequence_metadata(&f),
        (Some(seq_exif.clone()), Some(seq_xmp.clone()))
    );
    let mts = mv.metadata_tracks_of(1);
    assert_eq!(mts.len(), 2);
    assert!(mts.iter().all(|t| t.is_metadata() && !t.is_visual()));
    assert_eq!(mts[0].metadata_kind(), Some(MetadataTrackKind::Exif));
    assert_eq!(mts[1].metadata_kind(), Some(MetadataTrackKind::Xmp));
    assert_eq!(mts[0].references_of(b"cdsc"), vec![1]);
    assert_eq!(&mts[0].sample_entries[0].entry_type, b"Exif");
    assert_eq!(
        mts[1].sample_entries[0].namespace.as_deref(),
        Some(XMP_NAMESPACE)
    );
    assert_eq!(mts[0].samples.len(), 3);
    assert_eq!(mts[1].samples.len(), 2);
    assert_eq!(
        sample_bytes(&f, &mts[0].samples[1]).unwrap(),
        oxideav_heif::meta::exif_data_block(&exif_of(1)).as_slice()
    );
    assert_eq!(
        mts[1].sample_index_at(2, 10),
        Some(1),
        "time-parallel lookup"
    );
    assert_eq!(mts[1].sample_duration_total(), 3);
    assert!(mv.visual_tracks().count() == 1);
    let rep = oxideav_heif::miaf::check(&f, oxideav_heif::miaf::MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    // Contract: every frame carries the sequence metadata refined by the
    // time-parallel samples.
    let all = oxideav_heif::decode_all(&bytes).unwrap();
    let frames: Vec<_> = all.iter().filter(|f| f.track_id.is_some()).collect();
    assert_eq!(frames.len(), 3);
    for (i, fr) in frames.iter().enumerate() {
        assert_eq!(fr.track_id, Some(1));
        assert_eq!(
            fr.image.metadata.exif.as_deref(),
            Some(exif_of(i as u8).as_slice()),
            "frame {i}"
        );
        assert_eq!(
            fr.image.metadata.xmp.as_deref(),
            Some(xmp_of(i.min(1) as u8).as_slice()),
            "frame {i}"
        );
        assert_eq!(fr.image.planes[0].data, pictures[i].tight().planes[0].data);
    }
    // Framework: stream parameters, Data streams, the composed stream.
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let mut demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes.clone())), &ctx.codecs)
        .unwrap();
    let streams = demuxer.streams().to_vec();
    let by_id = |id: &str| {
        streams
            .iter()
            .find(|s| s.params.codec_id.as_str() == id)
            .cloned()
    };
    let composed = streams
        .iter()
        .find(|s| {
            s.params.codec_id.as_str() == "heif"
                && s.time_base == oxideav_core::TimeBase::new(1, 10)
        })
        .cloned()
        .expect("composed heif stream");
    let raw = by_id("h265").expect("raw track stream");
    let exif_stream = by_id("exif").expect("exif data stream");
    let xmp_stream = by_id("xmp").expect("xmp data stream");
    assert_eq!(
        streams.len(),
        5,
        "cover still + composed + raw + exif + xmp"
    );
    for s in [&composed, &raw] {
        assert_eq!(
            s.params.blob(&BlobKind::EXIF).map(|b| b.data.as_slice()),
            Some(seq_exif.as_slice())
        );
        assert_eq!(
            s.params.blob(&BlobKind::XMP).map(|b| b.data.as_slice()),
            Some(seq_xmp.as_slice())
        );
    }
    assert_eq!(exif_stream.params.media_type, oxideav_core::MediaType::Data);
    assert_eq!(
        demuxer
            .metadata()
            .iter()
            .find(|(k, _)| k == "track:1:metadata_tracks")
            .map(|(_, v)| v.as_str()),
        Some("2,3")
    );
    let mut dec = ctx.codecs.first_decoder(&composed.params).unwrap();
    let mut exif_packets = Vec::new();
    let mut xmp_packets = Vec::new();
    let mut raw_packets = 0;
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == composed.index => dec.send_packet(&p).unwrap(),
            Ok(p) if p.stream_index == exif_stream.index => exif_packets.push(p),
            Ok(p) if p.stream_index == xmp_stream.index => xmp_packets.push(p),
            Ok(p) if p.stream_index == raw.index => raw_packets += 1,
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(raw_packets, 3);
    assert_eq!(exif_packets.len(), 3);
    for (i, p) in exif_packets.iter().enumerate() {
        assert_eq!(p.data, exif_of(i as u8), "exif packet {i}");
        assert_eq!(p.pts, Some(i as i64));
        assert_eq!(p.duration, Some(1));
    }
    assert_eq!(xmp_packets.len(), 2);
    assert_eq!(xmp_packets[1].data, xmp_of(1));
    assert_eq!(xmp_packets[1].duration, Some(2));
    dec.flush().unwrap();
    let mut i = 0;
    while let Ok(Frame::Video(v)) = dec.receive_frame() {
        let blobs = v.blobs();
        let exif = blobs
            .iter()
            .find(|b| b.is(&BlobKind::EXIF))
            .map(|b| b.data.clone());
        let xmp = blobs
            .iter()
            .find(|b| b.is(&BlobKind::XMP))
            .map(|b| b.data.clone());
        assert_eq!(exif, Some(exif_of(i as u8)), "composed frame {i}");
        assert_eq!(xmp, Some(xmp_of(i.min(1) as u8)), "composed frame {i}");
        assert_eq!(v.image_planes()[0].data, pictures[i].tight().planes[0].data);
        i += 1;
    }
    assert_eq!(i, 3);
    // Black-box openers (asserted when installed).
    let path =
        std::env::temp_dir().join(format!("oxideav-heif-seqmeta-{}.heic", std::process::id()));
    std::fs::write(&path, &bytes).unwrap();
    for (bin, args) in [
        ("heif-info", vec![path.to_string_lossy().to_string()]),
        (
            "ffmpeg",
            vec![
                "-nostdin".into(),
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                path.to_string_lossy().to_string(),
                "-f".into(),
                "null".into(),
                "-".into(),
            ],
        ),
        (
            "sips",
            vec![
                "-g".into(),
                "pixelWidth".into(),
                path.to_string_lossy().to_string(),
            ],
        ),
    ] {
        match std::process::Command::new(bin).args(&args).output() {
            Ok(out) => assert!(
                out.status.success(),
                "{bin} refused the sequence with metadata tracks: {}",
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(_) => eprintln!("SKIP: {bin} not installed"),
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// `cprt` (ISO/IEC 14496-12 §8.10.2) is the one string carriage the
/// base format defines at the presentation level: the sequence writer
/// emits one per declaration in the movie `udta`, the parser reads
/// them back, and the demuxer reports them as `copyright` (`und`) /
/// `copyright:<lang>` pairs.
#[test]
#[cfg(feature = "registry")]
fn copyright_declarations_round_trip_through_cprt() {
    use std::io::Cursor;

    use oxideav_core::RuntimeContext;
    use oxideav_heif::encode::encode_hevc_picture;
    use oxideav_heif::image::Chroma;
    use oxideav_heif::sequence::Copyright;
    use oxideav_heif::writer::SequenceWriter;
    use oxideav_heif::{HeifFrame, HeifPixelFormat};

    let pic = HeifFrame::zeroed(
        32,
        32,
        HeifPixelFormat::new(Chroma::Yuv420, 8, false).unwrap(),
    )
    .unwrap();
    let coded = encode_hevc_picture(&pic, "pcm", 0).unwrap();
    let mut sw = SequenceWriter::new(*b"hvc1", coded.config.clone(), 32, 32, 10)
        .with_copyright("und", "© 2026 Example")
        .with_copyright("eng", "Copyright 2026 Example")
        .with_track_copyright("fra", "Droits réservés");
    sw.push_sample(coded.data.clone(), 1, true);
    let bytes = sw.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let mv = parse_movie(&f).unwrap().expect("moov");
    assert_eq!(
        mv.tracks[0].copyrights,
        vec![Copyright::new("fra", "Droits réservés")]
    );
    assert_eq!(
        mv.copyrights,
        vec![
            Copyright::new("und", "© 2026 Example"),
            Copyright::new("eng", "Copyright 2026 Example"),
        ]
    );
    assert_eq!(
        mv.copyrights[1].packed_language(),
        Some((5 << 10) | (14 << 5) | 7)
    );
    let mut ctx = RuntimeContext::new();
    oxideav_heif::register(&mut ctx);
    let demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let md = demuxer.metadata();
    assert!(md.contains(&("copyright".to_string(), "© 2026 Example".to_string())));
    assert!(md.contains(&(
        "copyright:eng".to_string(),
        "Copyright 2026 Example".to_string()
    )));
    // A language that is not a three-letter lower-case code is refused
    // at write time rather than packed into garbage.
    let mut bad = SequenceWriter::new(*b"hvc1", coded.config.clone(), 32, 32, 10)
        .with_copyright("en-US", "x");
    bad.push_sample(coded.data.clone(), 1, true);
    assert!(bad.write_to_vec().unwrap_err().to_string().contains("cprt"));
    // No declarations: no udta at all.
    let mut none = SequenceWriter::new(*b"hvc1", coded.config.clone(), 32, 32, 10);
    none.push_sample(coded.data.clone(), 1, true);
    let plain = none.write_to_vec().unwrap();
    assert!(!plain.windows(4).any(|w| w == b"udta"));
}
