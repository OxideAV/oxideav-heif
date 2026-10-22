//! Framework muxer round trip: frames → `oxideav-h265` encoder packets
//! → `"heif"` muxer (image sequence) → `"heif"` demuxer → decoder →
//! the same frames.
#![cfg(feature = "registry")]

use std::io::Cursor;

use oxideav_heif::encode::EncodeOptions;

use oxideav_core::{
    CodecId, CodecParameters, Decoder, Error, Frame, PixelFormat, RuntimeContext, StreamInfo,
    TimeBase,
};
use oxideav_heif::image::Chroma;
use oxideav_heif::{HeifFrame, HeifPixelFormat};

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    ctx
}

fn frame(i: u32) -> HeifFrame {
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
    for p in 1..3 {
        for y in 0..16 {
            for x in 0..16 {
                f.set_sample(p, x, y, ((x * 7 + y + i * 20 + p as u32 * 60) % 256) as u16);
            }
        }
    }
    f
}

#[test]
fn sequence_mux_demux_round_trip_is_exact() {
    let ctx = context();
    // Encode three lossless HEVC frames through the registry encoder.
    let mut params = CodecParameters::video(CodecId::new("h265"));
    params.width = Some(32);
    params.height = Some(32);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.options = oxideav_core::CodecOptions::new().set("mode", "pcm");
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    let frames: Vec<HeifFrame> = (0..3).map(frame).collect();
    let mut packets = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let (mut vf, _) = f.to_core().unwrap();
        vf.pts = Some(i as i64);
        enc.send_frame(&Frame::Video(vf)).unwrap();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
    }
    enc.flush().unwrap();
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    assert_eq!(packets.len(), 3);
    // Mux through the registry into a temporary file (a boxed sink must
    // be 'static, so a borrowed in-memory cursor cannot be used).
    let path = std::env::temp_dir().join(format!("oxideav-heif-mux-{}.heic", std::process::id()));
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 10),
        duration: Some(3),
        start_time: Some(0),
        params: enc.output_params().clone(),
    };
    let sink: Box<dyn oxideav_core::WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
    let mut muxer = ctx.containers.open_muxer("heif", sink, &[stream]).unwrap();
    muxer.write_header().unwrap();
    for p in &packets {
        let p = p.clone().with_duration(1).with_stream_index(0);
        muxer.write_packet(&p).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let bytes = std::fs::read(&path).unwrap();
    // Third-party readers open the aliased-cover sequence file (the
    // primary item's iloc points into the track mdat) — asserted when
    // the binary is present, SKIP otherwise.
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
                "{bin} refused the aliased-cover sequence: {}",
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(_) => eprintln!("SKIP: {bin} not installed"),
        }
    }
    let _ = std::fs::remove_file(&path);
    assert!(!bytes.is_empty());
    // Demux + decode.
    let mut cur = Cursor::new(bytes.clone());
    assert_eq!(ctx.containers.probe_input(&mut cur, None).unwrap(), "heif");
    let f = oxideav_heif::HeifFile::from_vec(bytes.clone()).unwrap();
    assert!(f.file_type.has_brand(b"msf1") && f.file_type.has_brand(b"hevc"));
    let rep = oxideav_heif::miaf::check(&f, oxideav_heif::miaf::MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let mut demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let streams = demuxer.streams().to_vec();
    assert_eq!(streams.len(), 2, "still + track");
    // The cover still is stream 0, the track follows it.
    assert_eq!(streams[0].params.codec_id.as_str(), "heif");
    let track = &streams[1];
    assert_eq!(track.params.codec_id.as_str(), "h265");
    assert_eq!(track.time_base, TimeBase::new(1, 10));
    let mut dec = ctx.codecs.first_decoder(&track.params).unwrap();
    let mut got = 0;
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == track.index => {
                assert_eq!(p.duration, Some(1));
                dec.send_packet(&p).unwrap();
                got += 1;
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(got, 3);
    dec.flush().unwrap();
    let mut i = 0;
    while let Ok(Frame::Video(v)) = dec.receive_frame() {
        let back = HeifFrame::from_core(&v, 32, 32, PixelFormat::Yuv420P).unwrap();
        assert_eq!(back, frames[i], "frame {i}");
        i += 1;
    }
    assert_eq!(i, 3);
    // The cover still decodes to frame 0.
    let img = oxideav_heif::decode_primary(&f, oxideav_heif::ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, frames[0]);
}

/// Find a top-level / nested box type anywhere in the file bytes.
/// r476 (core 0.1.40): the sequence muxer writes the stream's metadata
/// blobs — the ICC profile as the sample entry's `colr` (and the cover
/// item's), Exif / XMP as items describing the cover image — and the
/// demuxer / contract decode hand them back: every track frame carries
/// the ICC profile, the cover still the three.
#[test]
fn sequence_muxer_writes_the_stream_blobs() {
    use oxideav_core::{BlobKind, MetadataBlob};
    let ctx = context();
    let mut params = CodecParameters::video(CodecId::new("h265"));
    params.width = Some(32);
    params.height = Some(32);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.options = oxideav_core::CodecOptions::new().set("mode", "pcm");
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    let mut packets = Vec::new();
    for i in 0..2u32 {
        let (mut vf, _) = frame(i).to_core().unwrap();
        vf.pts = Some(i as i64);
        enc.send_frame(&Frame::Video(vf)).unwrap();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
    }
    enc.flush().unwrap();
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    let icc: Vec<u8> = (0..300u32).map(|i| (i * 13 % 250) as u8).collect();
    let exif: Vec<u8> = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0".to_vec();
    let xmp = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>".to_vec();
    let mut sparams = enc.output_params().clone();
    sparams.push_blob(MetadataBlob::new(BlobKind::ICC, icc.clone()));
    sparams.push_blob(MetadataBlob::new(BlobKind::EXIF, exif.clone()));
    sparams.push_blob(MetadataBlob::new(BlobKind::XMP, xmp.clone()));
    let path = std::env::temp_dir().join(format!(
        "oxideav-heif-mux-blobs-{}.heic",
        std::process::id()
    ));
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 10),
        duration: Some(2),
        start_time: Some(0),
        params: sparams,
    };
    let sink: Box<dyn oxideav_core::WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
    let mut muxer = ctx.containers.open_muxer("heif", sink, &[stream]).unwrap();
    muxer.write_header().unwrap();
    for p in &packets {
        muxer
            .write_packet(&p.clone().with_duration(1).with_stream_index(0))
            .unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    // Contract: the cover still carries all three, the track frames the
    // ICC profile.
    let info = oxideav_heif::info(&bytes).unwrap();
    assert!(
        info.has_icc && info.has_exif && info.has_xmp,
        "info(): {info:?}"
    );
    let frames = oxideav_heif::decode_all(&bytes).unwrap();
    let cover = frames.iter().find(|f| f.track_id.is_none()).unwrap();
    assert_eq!(
        cover.image.metadata,
        oxideav_heif::Metadata::new(
            Some(icc.clone()),
            Some(exif.clone()),
            Some(xmp.clone()),
            None
        )
    );
    let timed: Vec<_> = frames.iter().filter(|f| f.track_id.is_some()).collect();
    assert_eq!(timed.len(), 2);
    // r478: the track frames carry the three too — Exif / XMP from the
    // master track's MetaBox (HEIF §8.3).
    for f in &timed {
        assert_eq!(f.image.metadata, cover.image.metadata);
    }
    // Framework: the still stream's blobs and the track stream's.
    let bytes_copy = bytes.clone();
    let demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let streams = demuxer.streams();
    assert_eq!(
        oxideav_heif::metadata_of_blobs(&[], streams[0].params.blobs()),
        cover.image.metadata,
        "still stream blobs"
    );
    let track = streams
        .iter()
        .find(|s| s.params.codec_id.as_str() == "h265")
        .unwrap();
    assert_eq!(
        oxideav_heif::metadata_of_blobs(&[], track.params.blobs()),
        cover.image.metadata,
        "track stream blobs (ICC from the sample entry, Exif / XMP from the track meta)"
    );
    assert!(
        !has_box(&bytes_copy, b"nmhd"),
        "no metadata track without Data streams"
    );
}

fn has_box(bytes: &[u8], t: &[u8; 4]) -> bool {
    bytes.windows(4).any(|w| w == t)
}

/// A `"heif"` still stream (the `"heif"` encoder's whole-file packets)
/// passes through the muxer as the file: exactly one packet.
#[test]
fn still_stream_passes_through_as_the_file() {
    let ctx = context();
    let src = frame(0);
    let bytes =
        oxideav_heif::encode_still(&src, &EncodeOptions::default().with_hevc_mode("pcm".into()))
            .unwrap();
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(32);
    params.height = Some(32);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1),
        duration: Some(1),
        start_time: Some(0),
        params,
    };
    let path = std::env::temp_dir().join(format!("oxideav-heif-still-{}.heic", std::process::id()));
    let sink: Box<dyn oxideav_core::WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
    let mut muxer = ctx
        .containers
        .open_muxer("heif", sink, std::slice::from_ref(&stream))
        .unwrap();
    muxer.write_header().unwrap();
    let pkt = oxideav_core::Packet::new(0, TimeBase::new(1, 1), bytes.clone()).with_keyframe(true);
    muxer.write_packet(&pkt).unwrap();
    // One packet is the file (a second one would make a two-item
    // collection — see `still_burst_merges_into_one_collection…`).
    muxer.write_trailer().unwrap();
    drop(muxer);
    let written = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(written, bytes, "the packet is the file");
    let img = oxideav_heif::decode_primary(
        &oxideav_heif::HeifFile::parse(&written).unwrap(),
        oxideav_heif::ItemDecoder::direct(),
    )
    .unwrap();
    assert_eq!(img.frame, src);
    // Garbage is refused before anything is written.
    let path2 =
        std::env::temp_dir().join(format!("oxideav-heif-still2-{}.heic", std::process::id()));
    let sink: Box<dyn oxideav_core::WriteSeek> = Box::new(std::fs::File::create(&path2).unwrap());
    let mut muxer = ctx
        .containers
        .open_muxer("heif", sink, std::slice::from_ref(&stream))
        .unwrap();
    muxer.write_header().unwrap();
    let junk = oxideav_core::Packet::new(0, TimeBase::new(1, 1), vec![0u8; 64]);
    assert!(muxer.write_packet(&junk).is_err());
    assert!(muxer.write_trailer().is_err(), "nothing to write");
    drop(muxer);
    assert_eq!(std::fs::metadata(&path2).unwrap().len(), 0);
    let _ = std::fs::remove_file(&path2);
}

/// The sequence writer emits `stco` (32-bit chunk offsets, ISO/IEC
/// 14496-12 §8.7.5) and third-party readers open the file.
#[test]
fn sequence_file_carries_stco_and_opens_in_third_party_readers() {
    let ctx = context();
    let mut params = CodecParameters::video(CodecId::new("h265"));
    params.width = Some(32);
    params.height = Some(32);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.options = oxideav_core::CodecOptions::new().set("mode", "pcm");
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    let mut packets = Vec::new();
    for i in 0..2u32 {
        let (mut vf, _) = frame(i).to_core().unwrap();
        vf.pts = Some(i as i64);
        enc.send_frame(&Frame::Video(vf)).unwrap();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
    }
    enc.flush().unwrap();
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    let path = std::env::temp_dir().join(format!("oxideav-heif-stco-{}.heics", std::process::id()));
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 10),
        duration: Some(2),
        start_time: Some(0),
        params: enc.output_params().clone(),
    };
    let sink: Box<dyn oxideav_core::WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
    let mut muxer = ctx.containers.open_muxer("heif", sink, &[stream]).unwrap();
    muxer.write_header().unwrap();
    for p in &packets {
        muxer
            .write_packet(&p.clone().with_duration(1).with_stream_index(0))
            .unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let bytes = std::fs::read(&path).unwrap();
    assert!(has_box(&bytes, b"stco"), "stco present");
    assert!(!has_box(&bytes, b"co64"), "no co64 for small offsets");
    let f = oxideav_heif::HeifFile::parse(&bytes).unwrap();
    let mv = oxideav_heif::sequence::parse_movie(&f).unwrap().unwrap();
    assert_eq!(mv.tracks[0].samples.len(), 2);
    // Third-party readers, when present.
    let p = path.to_str().unwrap();
    let mut opened = Vec::new();
    for (bin, args) in [
        ("heif-info", vec![p]),
        ("magick", vec!["identify", p]),
        ("ffprobe", vec!["-v", "error", p]),
        ("/usr/bin/sips", vec!["-g", "pixelWidth", p]),
    ] {
        match std::process::Command::new(bin).args(&args).output() {
            Ok(o) => {
                assert!(
                    o.status.success(),
                    "{bin} refused the sequence: {}",
                    String::from_utf8_lossy(&o.stderr)
                );
                opened.push(bin);
            }
            Err(_) => eprintln!("SKIP {bin}: not installed"),
        }
    }
    eprintln!("sequence opened by {opened:?}");
    let _ = std::fs::remove_file(&path);
}

/// `SequenceWriter` with an alpha auxiliary track (HEIF §7.5.3): the
/// file re-parses with the `auxl` link, the demuxer composes frames
/// with the exact alpha, and the third-party readers present open it.
#[test]
fn sequence_writer_alpha_track_round_trips_and_opens_in_readers() {
    use oxideav_heif::encode::encode_hevc_picture;
    use oxideav_heif::writer::{SequenceAlphaTrack, SequenceWriter};
    let frames: Vec<HeifFrame> = (0..2).map(frame).collect();
    let alphas: Vec<HeifFrame> = (0..2u32)
        .map(|i| {
            let mut a = HeifFrame::zeroed(
                32,
                32,
                HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap(),
            )
            .unwrap();
            for y in 0..32 {
                for x in 0..32 {
                    a.set_sample(0, x, y, ((x * 8 + y * 3 + i * 50) % 256) as u16);
                }
            }
            a
        })
        .collect();
    let mut sw: Option<SequenceWriter> = None;
    let mut alpha_track: Option<SequenceAlphaTrack> = None;
    for (f, a) in frames.iter().zip(&alphas) {
        let pic = encode_hevc_picture(f, "pcm", 0).unwrap();
        let apic =
            encode_hevc_picture(&oxideav_heif::encode::to_yuv420_8(a).unwrap(), "pcm", 0).unwrap();
        let w =
            sw.get_or_insert_with(|| SequenceWriter::new(*b"hvc1", pic.config.clone(), 32, 32, 10));
        w.push_sample(pic.data.clone(), 1, true);
        let at = alpha_track.get_or_insert_with(|| {
            SequenceAlphaTrack::new(
                *b"hvc1",
                apic.config.clone(),
                32,
                32,
                oxideav_heif::props::AUX_URN_ALPHA_HEVC.to_string(),
                Vec::new(),
                Vec::new(),
            )
        });
        at.samples.push(oxideav_heif::writer::SequenceSample::new(
            apic.data.clone(),
            1,
            true,
        ));
    }
    let mut sw = sw.unwrap();
    sw.alpha = alpha_track;
    // Cover still (MIAF §7.2.1.4 wants a file-level meta), aliasing
    // sample 0 of the master track.
    let mut still = oxideav_heif::HeifWriter::new();
    let cover = still.add_coded_item(
        *b"hvc1",
        Vec::new(),
        vec![
            (sw.config.clone(), true),
            (
                oxideav_heif::props::Property::Ispe(oxideav_heif::props::Ispe::new(32, 32)),
                false,
            ),
            (
                oxideav_heif::props::Property::Pixi(oxideav_heif::props::Pixi::new(vec![8, 8, 8])),
                false,
            ),
            (
                oxideav_heif::props::Property::Colr(oxideav_heif::props::Colr::MIAF_DEFAULT),
                false,
            ),
        ],
    );
    still.set_primary(cover);
    sw.still = Some(still);
    sw.cover_sample = Some(0);
    let bytes = sw.write_to_vec().unwrap();
    let f = oxideav_heif::HeifFile::parse(&bytes).unwrap();
    let mv = oxideav_heif::sequence::parse_movie(&f).unwrap().unwrap();
    assert_eq!(mv.tracks.len(), 2);
    let at = mv.alpha_track_of(1).expect("alpha track");
    assert_eq!(at.track_id, 2);
    assert!(!at.in_movie);
    assert_eq!(at.samples.len(), 2);
    let rep = oxideav_heif::miaf::check(&f, oxideav_heif::miaf::MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    // Demux: the composed stream yields frames with the exact alpha.
    let ctx = context();
    let mut d = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes.clone())), &ctx.codecs)
        .unwrap();
    let streams = d.streams().to_vec();
    assert_eq!(streams.len(), 4, "still + composed + 2 raw");
    // The cover still is stream 0; the composed (alpha) stream, the
    // raw master and alpha tracks follow it.
    assert_eq!(streams[0].params.codec_id.as_str(), "heif");
    assert_eq!(streams[1].params.codec_id.as_str(), "heif");
    assert!(streams[1].params.pixel_format.unwrap().has_alpha());
    let mut n = 0;
    while let Ok(p) = d.next_packet() {
        if p.stream_index != 1 {
            continue;
        }
        let mut codec = oxideav_heif::demux::HeifCodec::new(CodecId::new("heif"));
        codec.send_packet(&p).unwrap();
        let img = codec.last_image().unwrap();
        let i = p.pts.unwrap() as usize;
        assert_eq!(img.frame.without_alpha(), frames[i].tight(), "frame {i}");
        assert_eq!(
            img.frame.alpha_as_frame().unwrap(),
            alphas[i].tight(),
            "alpha {i}"
        );
        n += 1;
    }
    assert_eq!(n, 2);
    // Third-party readers (SKIP when absent).
    let path = std::env::temp_dir().join(format!(
        "oxideav-heif-alpha-seq-{}.heics",
        std::process::id()
    ));
    std::fs::write(&path, &bytes).unwrap();
    for (bin, args) in [
        ("heif-info", vec![]),
        (
            "ffprobe",
            vec!["-hide_banner", "-loglevel", "error", "-show_streams"],
        ),
        ("sips", vec!["-g", "pixelWidth"]),
    ] {
        match std::process::Command::new(bin)
            .args(&args)
            .arg(&path)
            .output()
        {
            Ok(out) => {
                assert!(
                    out.status.success(),
                    "{bin} refused the alpha sequence: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                if bin == "ffprobe" {
                    let text = String::from_utf8_lossy(&out.stdout);
                    // The two tracks (the reader may list the cover still too).
                    assert!(text.matches("codec_type=video").count() >= 2, "{text}");
                }
            }
            Err(_) => eprintln!("SKIP: {bin} not installed"),
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// MIAF Amd 1:2025 Annex A.2 looping: the writer's `looping` setting
/// round-trips through `Track::loop_behaviour` for every case of
/// Table C.1.
#[test]
fn sequence_looping_round_trips_through_the_edit_list() {
    use oxideav_heif::encode::encode_hevc_picture;
    use oxideav_heif::sequence::LoopBehaviour;
    use oxideav_heif::writer::SequenceWriter;
    let pic = encode_hevc_picture(&frame(0), "pcm", 0).unwrap();
    for looping in [
        None,
        Some(LoopBehaviour::Times(1)),
        Some(LoopBehaviour::Times(3)),
        Some(LoopBehaviour::Forever),
    ] {
        let mut sw = SequenceWriter::new(*b"hvc1", pic.config.clone(), 32, 32, 10);
        sw.looping = looping;
        for _ in 0..2 {
            sw.push_sample(pic.data.clone(), 5, true);
        }
        let bytes = sw.write_to_vec().unwrap();
        let f = oxideav_heif::HeifFile::parse(&bytes).unwrap();
        let mv = oxideav_heif::sequence::parse_movie(&f).unwrap().unwrap();
        let t = &mv.tracks[0];
        let expected = match looping {
            None | Some(LoopBehaviour::Forever) => LoopBehaviour::Forever,
            Some(l) => l,
        };
        assert_eq!(t.loop_behaviour(), expected, "{looping:?}");
        match looping {
            None => assert!(t.edits.is_empty() && t.track_duration == 10),
            Some(LoopBehaviour::Times(1)) => {
                assert_eq!(t.edits.len(), 1);
                assert!(!t.repeat_edits);
                assert_eq!(t.track_duration, 10);
            }
            Some(LoopBehaviour::Times(n)) => {
                assert!(t.repeat_edits);
                assert_eq!(t.track_duration, 10 * n);
                assert_eq!(t.edits[0].segment_duration, 10);
            }
            Some(LoopBehaviour::Forever) => {
                assert!(t.repeat_edits);
                assert_eq!(t.track_duration, u64::MAX);
            }
            Some(other) => panic!("unexpected loop behaviour {other:?}"),
        }
    }
}

/// A burst through the framework: several frames of one `"heif"` still
/// stream, each with its own Exif / XMP blobs over the stream's ICC
/// profile, become the still encoder's whole-file packets; the `"heif"`
/// muxer merges them into one image collection — the first picture the
/// primary, every item with its own metadata items — that the demuxer
/// reads back as the same packets (per-item blobs on each frame) and
/// the contract `decode_all` as per-item images.
#[test]
fn still_burst_merges_into_one_collection_with_per_item_metadata() {
    use oxideav_core::{BlobKind, MetadataBlob};
    let ctx = context();
    let icc: Vec<u8> = (0..300u32).map(|i| (i * 13 % 250) as u8).collect();
    let exif_of = |i: u8| -> Vec<u8> {
        let mut e = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0".to_vec();
        e[19] = i + 1;
        e
    };
    let xmp_of = |i: u8| format!("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" n=\"{i}\"/>").into_bytes();
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(32);
    params.height = Some(32);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.options = oxideav_core::CodecOptions::new().set("mode", "pcm");
    params.push_blob(MetadataBlob::new(BlobKind::ICC, icc.clone()));
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    let mut packets = Vec::new();
    for i in 0..3u8 {
        let (mut vf, _) = frame(i as u32).to_core().unwrap();
        vf.pts = Some(i as i64);
        vf.push_blob(MetadataBlob::new(BlobKind::EXIF, exif_of(i)));
        vf.push_blob(MetadataBlob::new(BlobKind::XMP, xmp_of(i)));
        enc.send_frame(&Frame::Video(vf)).unwrap();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
    }
    enc.flush().unwrap();
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    assert_eq!(packets.len(), 3, "one file per frame");
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1),
        duration: Some(3),
        start_time: Some(0),
        params: enc.output_params().clone(),
    };
    let path = std::env::temp_dir().join(format!(
        "oxideav-heif-mux-burst-{}.heic",
        std::process::id()
    ));
    let sink: Box<dyn oxideav_core::WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
    let mut muxer = ctx.containers.open_muxer("heif", sink, &[stream]).unwrap();
    muxer.write_header().unwrap();
    for p in &packets {
        muxer.write_packet(&p.clone().with_stream_index(0)).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    // Structure: three coded images, each described by its own Exif and
    // XMP item, one shared ICC `colr`; MIAF-conformant with the HEVC
    // profile brands.
    let f = oxideav_heif::HeifFile::parse(&bytes).unwrap();
    let meta = f.meta().unwrap();
    let images: Vec<u32> = meta
        .items
        .iter()
        .filter(|i| i.is_image())
        .map(|i| i.id)
        .collect();
    assert_eq!(images.len(), 3, "{:?}", meta.items);
    for id in &images {
        assert_eq!(meta.metadata_of(*id).len(), 2, "item {id}: Exif + XMP");
    }
    assert_eq!(meta.primary_item_id, Some(images[0]));
    let rep = oxideav_heif::miaf::check(&f, oxideav_heif::miaf::MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    assert!(f.file_type.has_brand(b"MiHB") && f.file_type.has_brand(b"heic"));
    // Contract: three item frames, each with its own metadata and the
    // exact picture (lossless mode).
    let frames = oxideav_heif::decode_all(&bytes).unwrap();
    assert_eq!(frames.len(), 3);
    for (i, fr) in frames.iter().enumerate() {
        assert!(fr.delay.is_none() && fr.item_id.is_some());
        assert_eq!(
            fr.image.to_frame().unwrap(),
            frame(i as u32),
            "frame {i} pixels"
        );
        assert_eq!(
            fr.image.metadata,
            oxideav_heif::Metadata::new(
                Some(icc.clone()),
                Some(exif_of(i as u8)),
                Some(xmp_of(i as u8)),
                None
            ),
            "frame {i} metadata"
        );
    }
    // Framework: one still stream of three packets; the decoder puts
    // each item's blobs on its frame, the stream carries the first's.
    let mut demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let streams = demuxer.streams().to_vec();
    assert_eq!(streams.len(), 1);
    assert_eq!(
        oxideav_heif::metadata_of_blobs(&[], streams[0].params.blobs()),
        frames[0].image.metadata
    );
    let mut dec = ctx.codecs.first_decoder(&streams[0].params).unwrap();
    let mut n = 0;
    loop {
        let pkt = match demuxer.next_packet() {
            Ok(p) => p,
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        };
        dec.send_packet(&pkt).unwrap();
        while let Ok(Frame::Video(vf)) = dec.receive_frame() {
            assert_eq!(
                vf.blob(&BlobKind::EXIF).map(|b| b.data),
                Some(exif_of(n as u8)),
                "packet {n} exif"
            );
            assert_eq!(
                vf.blob(&BlobKind::XMP).map(|b| b.data),
                Some(xmp_of(n as u8)),
                "packet {n} xmp"
            );
            n += 1;
        }
    }
    assert_eq!(n, 3);
}

/// Timed metadata through the muxer (round 478, HEIF §8.3): the
/// stream's Exif / XMP blobs become the master track's `MetaBox` items,
/// `Data` streams of codec id `exif` / `xmp` become `Exif` / `metx`
/// metadata tracks with `cdsc` references, and the demuxer reads all of
/// it back — the blobs on the track's stream parameters, the tracks as
/// `Data` streams with the same packets, the composed `"heif"` stream's
/// frames with the per-picture blobs.
#[test]
fn metadata_tracks_round_trip_through_the_muxer() {
    use oxideav_core::{BlobKind, MetadataBlob, Packet};
    let ctx = context();
    let mut params = CodecParameters::video(CodecId::new("h265"));
    params.width = Some(32);
    params.height = Some(32);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.options = oxideav_core::CodecOptions::new().set("mode", "pcm");
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    let frames: Vec<HeifFrame> = (0..3).map(frame).collect();
    let mut packets = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let (mut vf, _) = f.to_core().unwrap();
        vf.pts = Some(i as i64);
        enc.send_frame(&Frame::Video(vf)).unwrap();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
    }
    enc.flush().unwrap();
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    assert_eq!(packets.len(), 3);
    let icc: Vec<u8> = (0..200u32).map(|i| (i * 7 % 250) as u8).collect();
    let seq_exif = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x01\0\0\0\0\0\0".to_vec();
    let seq_xmp = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>".to_vec();
    let exif_of = |i: u8| {
        let mut e = seq_exif.clone();
        e[18] = i + 2;
        e
    };
    let xmp_of =
        |i: u8| format!("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" frame=\"{i}\"/>").into_bytes();
    let mut sparams = enc.output_params().clone();
    sparams.push_blob(MetadataBlob::new(BlobKind::ICC, icc.clone()));
    sparams.push_blob(MetadataBlob::new(BlobKind::EXIF, seq_exif.clone()));
    sparams.push_blob(MetadataBlob::new(BlobKind::XMP, seq_xmp.clone()));
    let tb = TimeBase::new(1, 10);
    let streams = vec![
        StreamInfo {
            index: 0,
            time_base: tb,
            duration: Some(3),
            start_time: Some(0),
            params: sparams,
        },
        StreamInfo {
            index: 1,
            time_base: tb,
            duration: Some(3),
            start_time: Some(0),
            params: CodecParameters::data(CodecId::new("exif")),
        },
        StreamInfo {
            index: 2,
            time_base: tb,
            duration: Some(3),
            start_time: Some(0),
            params: CodecParameters::data(CodecId::new("xmp")),
        },
    ];
    let path = std::env::temp_dir().join(format!(
        "oxideav-heif-mux-metatracks-{}.heic",
        std::process::id()
    ));
    let sink: Box<dyn oxideav_core::WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
    let mut muxer = ctx.containers.open_muxer("heif", sink, &streams).unwrap();
    muxer.write_header().unwrap();
    for (i, p) in packets.iter().enumerate() {
        muxer
            .write_packet(&p.clone().with_duration(1).with_stream_index(0))
            .unwrap();
        muxer
            .write_packet(
                &Packet::new(1, tb, exif_of(i as u8))
                    .with_pts(i as i64)
                    .with_duration(1)
                    .with_keyframe(true),
            )
            .unwrap();
    }
    muxer
        .write_packet(&Packet::new(2, tb, xmp_of(0)).with_pts(0).with_duration(1))
        .unwrap();
    muxer
        .write_packet(&Packet::new(2, tb, xmp_of(1)).with_pts(1).with_duration(2))
        .unwrap();
    muxer.write_trailer().unwrap();
    drop(muxer);
    let bytes = std::fs::read(&path).unwrap();
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
    let f = oxideav_heif::HeifFile::parse(&bytes).unwrap();
    let mv = oxideav_heif::sequence::parse_movie(&f).unwrap().unwrap();
    assert_eq!(mv.tracks.len(), 3, "pict + Exif + metx");
    assert_eq!(
        mv.tracks[0].sequence_metadata(&f),
        (Some(seq_exif.clone()), Some(seq_xmp.clone()))
    );
    let rep = oxideav_heif::miaf::check(&f, oxideav_heif::miaf::MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    // Contract.
    let all = oxideav_heif::decode_all(&bytes).unwrap();
    let timed: Vec<_> = all.iter().filter(|f| f.track_id.is_some()).collect();
    assert_eq!(timed.len(), 3);
    for (i, fr) in timed.iter().enumerate() {
        assert_eq!(
            fr.image.metadata,
            oxideav_heif::Metadata::new(
                Some(icc.clone()),
                Some(exif_of(i as u8)),
                Some(xmp_of(i.min(1) as u8)),
                None
            ),
            "frame {i}"
        );
    }
    // Framework.
    let mut demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let streams = demuxer.streams().to_vec();
    let by_id = |id: &str| {
        streams
            .iter()
            .find(|s| s.params.codec_id.as_str() == id)
            .cloned()
            .unwrap()
    };
    let raw = by_id("h265");
    for kind in [BlobKind::ICC, BlobKind::EXIF, BlobKind::XMP] {
        assert!(raw.params.blob(&kind).is_some(), "track stream {kind:?}");
    }
    let exif_stream = by_id("exif");
    let xmp_stream = by_id("xmp");
    let composed = streams
        .iter()
        .find(|s| s.params.codec_id.as_str() == "heif" && s.time_base == tb)
        .cloned()
        .expect("composed stream");
    let mut dec = ctx.codecs.first_decoder(&composed.params).unwrap();
    let (mut exifs, mut xmps) = (Vec::new(), Vec::new());
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == composed.index => dec.send_packet(&p).unwrap(),
            Ok(p) if p.stream_index == exif_stream.index => exifs.push(p.data.clone()),
            Ok(p) if p.stream_index == xmp_stream.index => xmps.push((p.data.clone(), p.duration)),
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(exifs, (0..3u8).map(exif_of).collect::<Vec<_>>());
    assert_eq!(xmps, vec![(xmp_of(0), Some(1)), (xmp_of(1), Some(2))]);
    dec.flush().unwrap();
    let mut i = 0;
    while let Ok(Frame::Video(v)) = dec.receive_frame() {
        let blobs = v.blobs();
        assert_eq!(
            blobs
                .iter()
                .find(|b| b.is(&BlobKind::EXIF))
                .map(|b| b.data.clone()),
            Some(exif_of(i as u8))
        );
        assert_eq!(
            blobs
                .iter()
                .find(|b| b.is(&BlobKind::XMP))
                .map(|b| b.data.clone()),
            Some(xmp_of(i.min(1) as u8))
        );
        let back = HeifFrame::from_core(&v, 32, 32, PixelFormat::Yuv420P).unwrap();
        assert_eq!(back, frames[i], "frame {i}");
        i += 1;
    }
    assert_eq!(i, 3);
}

/// `Muxer::set_metadata` (core 0.1.41): the sequence muxer keeps the
/// `copyright` / `copyright:<lang>` pairs as `cprt` boxes and drops
/// every other key (no ISOBMFF / HEIF carriage exists for free-form
/// pairs); `carries_metadata_key` says so beforehand; a still stream
/// (the encoder's whole file, written through) stores nothing.
#[test]
fn muxer_set_metadata_carries_copyright_as_cprt() {
    let ctx = context();
    let mut params = CodecParameters::video(CodecId::new("h265"));
    params.width = Some(32);
    params.height = Some(32);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.options = oxideav_core::CodecOptions::new().set("mode", "pcm");
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    let (mut vf, _) = frame(0).to_core().unwrap();
    vf.pts = Some(0);
    enc.send_frame(&Frame::Video(vf)).unwrap();
    enc.flush().unwrap();
    let packet = enc
        .receive_packet()
        .unwrap()
        .with_duration(1)
        .with_stream_index(0);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 10),
        duration: Some(1),
        start_time: Some(0),
        params: enc.output_params().clone(),
    };
    let path = std::env::temp_dir().join(format!("oxideav-heif-cprt-{}.heic", std::process::id()));
    let sink: Box<dyn oxideav_core::WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
    let mut muxer = ctx
        .containers
        .open_muxer("heif", sink, std::slice::from_ref(&stream))
        .unwrap();
    assert!(muxer.carries_metadata_key("copyright"));
    assert!(muxer.carries_metadata_key("copyright:jpn"));
    assert!(!muxer.carries_metadata_key("copyright:en"));
    assert!(!muxer.carries_metadata_key("title"));
    let pairs = vec![
        ("title".to_string(), "dropped".to_string()),
        ("copyright".to_string(), "© Example".to_string()),
        ("copyright:jpn".to_string(), "© 例".to_string()),
    ];
    muxer.set_metadata(&pairs).unwrap();
    muxer.write_header().unwrap();
    assert!(matches!(
        muxer.set_metadata(&pairs),
        Err(Error::InvalidData(_))
    ));
    muxer.write_packet(&packet).unwrap();
    muxer.write_trailer().unwrap();
    drop(muxer);
    let bytes = std::fs::read(&path).unwrap();
    // The `udta` / `cprt` boxes do not disturb the black-box readers
    // (asserted when the binary is present, SKIP otherwise).
    for (bin, args) in [
        ("heif-info", vec![path.to_string_lossy().to_string()]),
        (
            "ffprobe",
            vec![
                "-v".into(),
                "error".into(),
                "-show_streams".into(),
                path.to_string_lossy().to_string(),
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
                "{bin} refused the cprt file: {}",
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(_) => eprintln!("SKIP {bin}: not installed"),
        }
    }
    let _ = std::fs::remove_file(&path);
    let demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let md: Vec<(String, String)> = demuxer
        .metadata()
        .iter()
        .filter(|(k, _)| k.starts_with("copyright") || k == "title")
        .cloned()
        .collect();
    assert_eq!(md, pairs[1..].to_vec());
    // A still stream: nothing to store.
    let still = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1),
        duration: None,
        start_time: None,
        params: CodecParameters::video(CodecId::new("heif")),
    };
    let sink: Box<dyn oxideav_core::WriteSeek> = Box::new(Cursor::new(Vec::new()));
    let mut muxer = ctx
        .containers
        .open_muxer("heif", sink, std::slice::from_ref(&still))
        .unwrap();
    assert!(!muxer.carries_metadata_key("copyright"));
    assert!(matches!(
        muxer.set_metadata(&pairs),
        Err(Error::Unsupported(_))
    ));
}

/// The still encoder declares the blob kinds it embeds (core 0.1.41
/// `CodecCapabilities::embedded_blob_kinds`): ICC, Exif and XMP — the
/// three the HEIF path carries (`colr` of type `prof`, `Exif` and
/// `mime` items) — and nothing else.
#[test]
fn encoder_declares_its_embedded_blob_kinds() {
    use oxideav_core::BlobKind;
    let ctx = context();
    let imp = &ctx.codecs.implementations(&CodecId::new("heif"))[0];
    assert!(imp.caps.declares_embedded_blobs());
    assert_eq!(
        imp.caps.embedded_blob_kinds,
        vec![BlobKind::ICC, BlobKind::EXIF, BlobKind::XMP]
    );
    assert!(!imp.caps.embeds_blob(&BlobKind::IPTC));
}
