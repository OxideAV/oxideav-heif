//! Framework muxer round trip: frames → `oxideav-h265` encoder packets
//! → `"heif"` muxer (image sequence) → `"heif"` demuxer → decoder →
//! the same frames.
#![cfg(feature = "registry")]

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Error, Frame, PixelFormat, RuntimeContext, StreamInfo, TimeBase,
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
    let _ = std::fs::remove_file(&path);
    assert!(!bytes.is_empty());
    // Demux + decode.
    let mut cur = Cursor::new(bytes.clone());
    assert_eq!(ctx.containers.probe_input(&mut cur, None).unwrap(), "heif");
    let f = oxideav_heif::HeifFile::parse(&bytes).unwrap();
    assert!(f.file_type.has_brand(b"msf1") && f.file_type.has_brand(b"hevc"));
    let rep = oxideav_heif::miaf::check(&f, oxideav_heif::miaf::MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let mut demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let streams = demuxer.streams().to_vec();
    assert_eq!(streams.len(), 2, "still + track");
    let track = &streams[1];
    assert_eq!(track.params.codec_id.as_str(), "h265");
    assert_eq!(track.time_base, TimeBase::new(1, 10));
    let mut dec = ctx.codecs.first_decoder(&track.params).unwrap();
    let mut got = 0;
    loop {
        match demuxer.next_packet() {
            Ok(p) if p.stream_index == 1 => {
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
fn has_box(bytes: &[u8], t: &[u8; 4]) -> bool {
    bytes.windows(4).any(|w| w == t)
}

/// A `"heif"` still stream (the `"heif"` encoder's whole-file packets)
/// passes through the muxer as the file: exactly one packet.
#[test]
fn still_stream_passes_through_as_the_file() {
    let ctx = context();
    let src = frame(0);
    let bytes = oxideav_heif::encode_still(
        &src,
        &oxideav_heif::EncodeOptions {
            hevc_mode: "pcm".into(),
            ..Default::default()
        },
    )
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
    // A second still packet is a typed refusal, not a silent append.
    let err = muxer.write_packet(&pkt).unwrap_err();
    assert!(err.to_string().contains("exactly one packet"), "{err}");
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
