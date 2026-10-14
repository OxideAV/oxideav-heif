#![no_main]

//! The framework path on arbitrary bytes — what `oxideav-image` /
//! `oxideav-io` / the CLI run: the registry probe, the `"heif"`
//! demuxer (stream description, packet walk, seeks, active-stream
//! selection), the `"heif"` decoder fed those packets under tight
//! decoder options, and the `"heif"` muxer fed the still packet back.
//! The contract is "the call returns" — no panic, abort or unbounded
//! allocation. Coded-track packets are not decoded here (the codec
//! crates fuzz their own decoders); the `"heif"` still stream is.

use std::io::Cursor;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{CodecOptions, Decoder, Demuxer, Muxer, ProbeData, RuntimeContext};

static CTX: OnceLock<RuntimeContext> = OnceLock::new();

fn context() -> &'static RuntimeContext {
    CTX.get_or_init(|| {
        let mut ctx = RuntimeContext::new();
        oxideav_h265::register(&mut ctx);
        oxideav_av1::register(&mut ctx);
        oxideav_h264::register(&mut ctx);
        oxideav_heif::register(&mut ctx);
        ctx
    })
}

fuzz_target!(|data: &[u8]| {
    let ctx = context();
    let _ = ctx.containers.probe_candidates(&ProbeData {
        buf: data,
        ext: Some("heic"),
    });
    if !oxideav_heif::probe(data) {
        return;
    }
    let Ok(mut demuxer) = ctx.containers.open_demuxer(
        "heif",
        Box::new(Cursor::new(data.to_vec())),
        &ctx.codecs,
    ) else {
        return;
    };
    let streams = demuxer.streams().to_vec();
    let _ = demuxer.metadata().len();
    let _ = demuxer.duration_micros();
    let _ = demuxer.format_name();
    // Tight decoder limits: small pictures, few bytes, serial.
    let limits = CodecOptions::new()
        .set("max_width", "512")
        .set("max_height", "512")
        .set("max_pixels", "65536")
        .set("max_bytes", "262144");
    let mut decoders: Vec<Option<Box<dyn Decoder>>> = streams
        .iter()
        .map(|s| {
            if s.params.codec_id.as_str() != "heif" {
                return None;
            }
            let mut p = s.params.clone();
            p.options = limits.clone();
            ctx.codecs.first_decoder(&p).ok()
        })
        .collect();
    let mut first_still = None;
    for _ in 0..16 {
        let Ok(p) = demuxer.next_packet() else { break };
        let i = p.stream_index as usize;
        if let Some(Some(dec)) = decoders.get_mut(i) {
            if first_still.is_none() {
                first_still = Some((i, p.clone()));
            }
            if dec.send_packet(&p).is_ok() {
                while let Ok(f) = dec.receive_frame() {
                    if let oxideav_core::Frame::Video(v) = f {
                        let _ = v.image_planes().len();
                    }
                }
            }
            let _ = dec.flush();
            let _ = dec.reset();
        }
    }
    for s in &streams {
        let _ = demuxer.seek_to(s.index, 1);
        let _ = demuxer.seek_to(s.index, i64::MAX / 2);
    }
    if let Some(s) = streams.last() {
        demuxer.set_active_streams(&[s.index]);
        let _ = demuxer.next_packet();
    }
    // The still packet back through the muxer (the pass-through of a
    // `"heif"` stream) — on a bounded output.
    if let Some((i, p)) = first_still {
        let out: Vec<u8> = Vec::new();
        if let Ok(mut m) = ctx
            .containers
            .open_muxer("heif", Box::new(Cursor::new(out)), &streams[i..=i])
        {
            let _ = m.write_header();
            let _ = m.write_packet(&p);
            let _ = m.write_trailer();
        }
    }
});
