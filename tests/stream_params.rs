//! oxideav-core 0.1.43's mid-stream output-shape contract, both sides.
//!
//! * **Consumer.** Every codec driven by this crate may return
//!   `Frame::StreamParams` events and frames carrying side-channel
//!   planes (layer identity, colour signal). Through a caller-supplied
//!   `CodecRegistry` holding a stub decoder that surrounds its picture
//!   with events and tags it with a layer identity, coded items, grid
//!   tiles and the three registry-routed codecs (HEVC, AV1, AVC) decode
//!   to exactly the stub's picture — the events are neither errors nor
//!   pictures, the side-channel planes never become image planes.
//! * **Producer.** The `"heif"` decoder returns one event immediately
//!   before the first picture of a new shape (files / items of
//!   different geometry, layout or colour decoded in turn), none for a
//!   constant stream or the first shape; the payload equals
//!   `output_params()`, the generation moves with it, and the rule holds
//!   across `flush()` draining and `reset()`.
#![cfg(feature = "registry")]

mod common;

use common::stub::{self, is_flat, next_video};
use oxideav_core::{
    CodecId, CodecParameters, Decoder, Error, Frame, LayerIdentity, OutputParamsWatch, Packet,
    RuntimeContext, TimeBase,
};
use oxideav_heif::decode::{ItemDecoder, decode_primary};
use oxideav_heif::derived::build_primary_graph;
use oxideav_heif::{
    ColorInfo, ColorRange, EncodeOptions, HeifCodec, HeifFile, HeifFrame, HeifImage,
    HeifPixelFormat, PixelFormat, Plane, StillCodec, encode,
};

fn yuv420(w: u32, h: u32, seed: u8) -> HeifImage {
    let (wu, hu) = (w as usize, h as usize);
    let (cw, ch) = (wu.div_ceil(2), hu.div_ceil(2));
    let y: Vec<u8> = (0..wu * hu)
        .map(|i| 16 + ((i * 7 + seed as usize * 31) % 220) as u8)
        .collect();
    let cb: Vec<u8> = (0..cw * ch)
        .map(|i| 16 + ((i * 3 + seed as usize) % 224) as u8)
        .collect();
    let cr: Vec<u8> = (0..cw * ch)
        .map(|i| 16 + ((i * 5 + seed as usize) % 224) as u8)
        .collect();
    HeifImage::new(
        w,
        h,
        PixelFormat::Yuv420P,
        vec![Plane::new(wu, y), Plane::new(cw, cb), Plane::new(cw, cr)],
    )
    .unwrap()
}

fn gray(w: u32, h: u32) -> HeifImage {
    let y: Vec<u8> = (0..w * h).map(|i| (i * 13 % 256) as u8).collect();
    HeifImage::new(w, h, PixelFormat::Gray8, vec![Plane::new(w as usize, y)]).unwrap()
}

fn pcm() -> EncodeOptions {
    EncodeOptions::default().with_hevc_mode("pcm".into())
}

// ───────────────────────── consumer side ─────────────────────────

/// Decode `bytes`' primary through the stub registry; it must be the
/// stub's flat picture at the item's size.
fn primary_through_stub(bytes: &[u8]) -> HeifFrame {
    let reg = stub::registry(false);
    let f = HeifFile::parse(bytes).unwrap();
    decode_primary(&f, ItemDecoder::with_registry(&reg))
        .unwrap()
        .frame
}

#[test]
fn coded_item_skips_events_and_side_channel_planes() {
    let bytes = encode(&yuv420(64, 32, 1), &pcm()).unwrap();
    let reg = stub::registry(false);
    let f = HeifFile::parse(&bytes).unwrap();
    let node = build_primary_graph(&f).unwrap();
    let frame = ItemDecoder::with_registry(&reg)
        .decode_coded(&f, &node)
        .unwrap();
    assert_eq!((frame.width, frame.height), (64, 32));
    assert_eq!(
        frame.planes.len(),
        3,
        "the layer / colour side channels are not image planes"
    );
    assert!(is_flat(&frame), "the stub's picture, not an event");
    frame.validate().unwrap();
    // The whole-image driver too.
    let full = primary_through_stub(&bytes);
    assert_eq!(full, frame);
}

#[test]
fn grid_tiles_skip_events_and_side_channel_planes() {
    let opts = pcm().with_grid_tile(Some(32));
    let bytes = encode(&yuv420(64, 64, 2), &opts).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert_eq!(&f.primary_item().unwrap().item_type, b"grid");
    let frame = primary_through_stub(&bytes);
    assert_eq!((frame.width, frame.height), (64, 64));
    assert_eq!(frame.planes.len(), 3);
    assert!(is_flat(&frame), "every tile is the stub's picture");
    // Parallel tile workers see the same.
    let reg = stub::registry(false);
    let par = decode_primary(&f, ItemDecoder::with_registry(&reg).with_threads(4))
        .unwrap()
        .frame;
    assert_eq!(par, frame);
}

#[test]
fn av1_and_avc_items_skip_events_and_side_channel_planes() {
    for codec in [StillCodec::Av1, StillCodec::Avc] {
        let opts = EncodeOptions::default().with_codec(codec);
        let bytes = encode(&yuv420(32, 32, 3), &opts).unwrap();
        let frame = primary_through_stub(&bytes);
        assert_eq!((frame.width, frame.height), (32, 32), "{codec:?}");
        assert_eq!(frame.planes.len(), 3, "{codec:?}");
        assert!(is_flat(&frame), "{codec:?}");
    }
}

#[test]
fn monochrome_item_keeps_one_image_plane() {
    let bytes = encode(&gray(32, 16), &pcm()).unwrap();
    let frame = primary_through_stub(&bytes);
    assert_eq!(
        frame.format,
        HeifPixelFormat::new(oxideav_heif::Chroma::Mono, 8, false).unwrap()
    );
    assert_eq!(frame.planes.len(), 1, "the side channels stay out");
    assert!(is_flat(&frame));
}

/// Framework frames carrying a layer identity (and a colour signal)
/// convert to this crate's frames / images on their image planes only,
/// and encode as the picture they carry.
#[test]
fn layer_tagged_framework_frames_convert_on_their_image_planes() {
    let layer = LayerIdentity::new(1).with_view_id(1).with_access_unit(7);
    let vf = stub::picture(16, 8, oxideav_core::PixelFormat::Yuv420P, layer);
    assert_eq!(vf.layer(), Some(layer));
    assert_eq!(vf.planes.len(), 5, "3 image planes + 2 side channels");
    let f = HeifFrame::from_core(&vf, 16, 8, oxideav_core::PixelFormat::Yuv420P).unwrap();
    assert_eq!(f.planes.len(), 3);
    assert!(is_flat(&f));
    let img =
        HeifImage::from_video_frame_parts(&vf, 16, 8, oxideav_core::PixelFormat::Yuv420P).unwrap();
    assert_eq!(img.planes.len(), 3);
    // 16-bit samples: a 10-bit layout.
    let deep = stub::picture(8, 8, oxideav_core::PixelFormat::Yuv444P10Le, layer);
    let f = HeifFrame::from_core(&deep, 8, 8, oxideav_core::PixelFormat::Yuv444P10Le).unwrap();
    assert_eq!(f.planes.len(), 3);
    assert!(is_flat(&f));
    // The framework encoder codes the image planes (lossless PCM reads
    // back the stub's picture exactly).
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(16);
    params.height = Some(8);
    params.pixel_format = Some(oxideav_core::PixelFormat::Yuv420P);
    params.options = oxideav_core::CodecOptions::new().set("mode", "pcm");
    let mut enc = oxideav_heif::make_encoder(&params).unwrap();
    enc.send_frame(&Frame::Video(vf)).unwrap();
    enc.flush().unwrap();
    let pkt = enc.receive_packet().unwrap();
    let back = oxideav_heif::decode(&pkt.data).unwrap();
    let back = HeifFrame::from_core(
        &back.into_video_frame().0,
        16,
        8,
        oxideav_core::PixelFormat::Yuv420P,
    )
    .unwrap();
    assert!(is_flat(&back));
}

#[test]
fn encoder_refuses_a_stream_params_event_as_unsupported() {
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(16);
    params.height = Some(8);
    params.pixel_format = Some(oxideav_core::PixelFormat::Yuv420P);
    let mut enc = oxideav_heif::make_encoder(&params).unwrap();
    let ev = Frame::StreamParams(Box::new(params.clone()));
    assert!(matches!(enc.send_frame(&ev), Err(Error::Unsupported(_))));
}

// ───────────────────────── producer side ─────────────────────────

fn packet(bytes: &[u8]) -> Packet {
    Packet::new(0, TimeBase::new(1, 1), bytes.to_vec())
}

/// What a pull returned: a picture of `(w, h)`, or an event.
#[derive(Debug, PartialEq)]
enum Out {
    Pic(u32, u32),
    Event(u32, u32, oxideav_core::PixelFormat),
}

/// Pull until `NeedMore` / `Eof`, checking every event against the
/// decoder's `output_params()` / generation and every picture against
/// the current `output_params()`.
fn pull(dec: &mut dyn Decoder, generation: &mut u64) -> Vec<Out> {
    let mut out = Vec::new();
    loop {
        match dec.receive_frame() {
            Ok(Frame::Video(v)) => {
                assert_eq!(dec.output_params_generation(), *generation);
                let p = dec.output_params().expect("output_params after a picture");
                // The picture really has the announced shape.
                let img = HeifImage::from_video_frame(&v, &p).unwrap();
                assert_eq!(
                    (img.width, img.height),
                    (p.width.unwrap(), p.height.unwrap())
                );
                out.push(Out::Pic(img.width, img.height));
            }
            Ok(Frame::StreamParams(p)) => {
                *generation = generation.wrapping_add(1);
                assert_eq!(
                    dec.output_params_generation(),
                    *generation,
                    "one bump per event"
                );
                let now = dec.output_params().unwrap();
                assert_eq!((now.width, now.height), (p.width, p.height));
                assert_eq!(now.pixel_format, p.pixel_format);
                assert_eq!(now.color_signal, p.color_signal);
                assert_eq!(p.codec_id, *dec.codec_id());
                out.push(Out::Event(
                    p.width.unwrap(),
                    p.height.unwrap(),
                    p.pixel_format.unwrap(),
                ));
            }
            Ok(other) => panic!("unexpected frame {other:?}"),
            Err(Error::NeedMore) | Err(Error::Eof) => return out,
            Err(e) => panic!("{e}"),
        }
    }
}

/// Four files: A 64×32 4:2:0, B 32×16 4:2:0, C 32×16 grey, D = A's
/// geometry and layout in another colour description. The pictures
/// carry no explicit range, so the 8-bit YCbCr ones come out under the
/// MIAF default (full range) as `YuvJ420P`.
fn files() -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let a = encode(&yuv420(64, 32, 1), &pcm()).unwrap();
    let b = encode(&yuv420(32, 16, 2), &pcm()).unwrap();
    let c = encode(&gray(32, 16), &pcm()).unwrap();
    // A's geometry and layout in another colour description.
    let d = encode(
        &yuv420(64, 32, 4).with_color(ColorInfo::new(ColorRange::Full, 9, 16, 9)),
        &pcm(),
    )
    .unwrap();
    (a, b, c, d)
}

#[test]
fn constant_stream_has_no_event() {
    let (a, ..) = files();
    let mut dec = HeifCodec::new(CodecId::new("heif"));
    assert!(
        dec.output_params().is_none(),
        "nothing known before a packet"
    );
    let mut g = 0;
    for _ in 0..3 {
        dec.send_packet(&packet(&a)).unwrap();
        assert_eq!(pull(&mut dec, &mut g), vec![Out::Pic(64, 32)]);
    }
    assert_eq!(dec.output_params_generation(), 0);
}

#[test]
fn shapes_decoded_in_turn_announce_each_change_once() {
    let (a, b, c, d) = files();
    let mut dec = HeifCodec::new(CodecId::new("heif"));
    let mut g = 0;
    // The first shape: announced up front, no event.
    dec.send_packet(&packet(&a)).unwrap();
    let first = dec.output_params().unwrap();
    assert_eq!((first.width, first.height), (Some(64), Some(32)));
    assert_eq!(pull(&mut dec, &mut g), vec![Out::Pic(64, 32)]);
    // Same shape: nothing.
    dec.send_packet(&packet(&a)).unwrap();
    assert_eq!(pull(&mut dec, &mut g), vec![Out::Pic(64, 32)]);
    // Geometry change.
    dec.send_packet(&packet(&b)).unwrap();
    assert_eq!(
        pull(&mut dec, &mut g),
        vec![
            Out::Event(32, 16, oxideav_core::PixelFormat::YuvJ420P),
            Out::Pic(32, 16)
        ]
    );
    // Layout change at the same geometry.
    dec.send_packet(&packet(&c)).unwrap();
    assert_eq!(
        pull(&mut dec, &mut g),
        vec![
            Out::Event(32, 16, oxideav_core::PixelFormat::Gray8),
            Out::Pic(32, 16)
        ]
    );
    dec.send_packet(&packet(&c)).unwrap();
    assert_eq!(pull(&mut dec, &mut g), vec![Out::Pic(32, 16)]);
    // Back to A, then A's geometry and layout in another colour
    // description (same label, BT.2020 / PQ code points): a change of
    // the colour signal alone is a change too.
    dec.send_packet(&packet(&a)).unwrap();
    assert_eq!(pull(&mut dec, &mut g).len(), 2);
    let before = dec.output_params().unwrap().color_signal;
    dec.send_packet(&packet(&d)).unwrap();
    let got = pull(&mut dec, &mut g);
    assert_eq!(
        got,
        vec![
            Out::Event(64, 32, oxideav_core::PixelFormat::YuvJ420P),
            Out::Pic(64, 32)
        ]
    );
    assert_ne!(dec.output_params().unwrap().color_signal, before);
    assert_eq!(dec.output_params_generation(), 4);
    // A refused packet changes nothing.
    assert!(dec.send_packet(&packet(&[0, 0, 0, 8])).is_err());
    assert_eq!(pull(&mut dec, &mut g), vec![]);
    assert_eq!(dec.output_params_generation(), 4);
}

#[test]
fn queued_pictures_keep_their_order_through_flush() {
    let (a, b, ..) = files();
    let mut dec = HeifCodec::new(CodecId::new("heif"));
    let mut g = 0;
    for f in [&a, &b, &b, &a] {
        dec.send_packet(&packet(f)).unwrap();
    }
    // Before the first picture: the first queued picture's shape.
    assert_eq!(dec.output_params().unwrap().width, Some(64));
    dec.flush().unwrap();
    assert_eq!(
        pull(&mut dec, &mut g),
        vec![
            Out::Pic(64, 32),
            Out::Event(32, 16, oxideav_core::PixelFormat::YuvJ420P),
            Out::Pic(32, 16),
            Out::Pic(32, 16),
            Out::Event(64, 32, oxideav_core::PixelFormat::YuvJ420P),
            Out::Pic(64, 32),
        ]
    );
    assert!(matches!(dec.receive_frame(), Err(Error::Eof)));
    assert_eq!(dec.output_params_generation(), 2);
}

#[test]
fn reset_keeps_the_last_returned_shape_as_the_reference() {
    let (a, b, ..) = files();
    let mut dec = HeifCodec::new(CodecId::new("heif"));
    let mut g = 0;
    dec.send_packet(&packet(&a)).unwrap();
    assert_eq!(pull(&mut dec, &mut g), vec![Out::Pic(64, 32)]);
    // A queued B picture dropped by the seek: no event for it.
    dec.send_packet(&packet(&b)).unwrap();
    dec.reset().unwrap();
    assert_eq!(pull(&mut dec, &mut g), vec![]);
    assert_eq!(dec.output_params().unwrap().width, Some(64));
    // The same shape as before the seek: nothing; another: one event.
    dec.send_packet(&packet(&a)).unwrap();
    assert_eq!(pull(&mut dec, &mut g), vec![Out::Pic(64, 32)]);
    dec.reset().unwrap();
    dec.send_packet(&packet(&b)).unwrap();
    assert_eq!(
        pull(&mut dec, &mut g),
        vec![
            Out::Event(32, 16, oxideav_core::PixelFormat::YuvJ420P),
            Out::Pic(32, 16)
        ]
    );
    assert_eq!(dec.output_params_generation(), 1);
}

/// Through the registry: the `"heif"` decoder a `RuntimeContext`
/// hands out, with `OutputParamsWatch` — the counter and the event
/// report each change exactly once, and `receive_arena_frame` (which
/// skips the event) still sees the pictures.
#[test]
fn registry_decoder_events_agree_with_the_watch() {
    let (a, b, c, _) = files();
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let params = CodecParameters::video(CodecId::new("heif"));
    let mut dec = ctx.codecs.first_decoder(&params).unwrap();
    let mut stream = params.clone();
    let mut by_counter = OutputParamsWatch::new();
    let mut by_event = OutputParamsWatch::new();
    let mut stream_ev = params.clone();
    let (mut counter_changes, mut event_changes, mut pictures) = (0, 0, 0);
    for f in [&a, &a, &b, &c, &c, &a] {
        dec.send_packet(&packet(f)).unwrap();
        loop {
            match dec.receive_frame() {
                Ok(frame) => {
                    if by_event.observe_frame(&frame, &mut stream_ev) {
                        event_changes += 1;
                    }
                    if frame.is_stream_params() {
                        continue;
                    }
                    pictures += 1;
                    if by_counter.observe(&*dec, &mut stream) {
                        counter_changes += 1;
                    }
                }
                Err(Error::NeedMore) => break,
                Err(e) => panic!("{e}"),
            }
        }
    }
    assert_eq!(pictures, 6);
    // The counter also reports the first shape's resolution.
    assert_eq!(counter_changes, 1 + 3);
    assert_eq!(event_changes, 3);
    assert_eq!(stream.width, stream_ev.width);
    assert_eq!(stream.pixel_format, stream_ev.pixel_format);
    // The arena path skips the events.
    dec.send_packet(&packet(&b)).unwrap();
    let arena = dec.receive_arena_frame().unwrap();
    assert_eq!(arena.header().width, 32);
    assert!(next_video(&mut *dec).is_none());
}
