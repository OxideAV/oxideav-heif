//! The gateway contract of the registry encoders (round 476): the
//! `"avif"` codec id writes AV1 into an `avif`-branded file where
//! `"heif"` writes HEVC into a `heic` one, and the gateway's advisory
//! `quality` steers every codec.
#![cfg(feature = "registry")]

use oxideav_core::{CodecId, CodecOptions, CodecParameters, Decoder, Frame, RuntimeContext};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};
use oxideav_heif::HeifFile;

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_h264::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    ctx
}

/// A detailed 8-bit 4:2:0 picture (a gradient with a texture, so every
/// quantiser step changes the bytes).
fn picture(w: u32, h: u32) -> HeifFrame {
    let fmt = HeifPixelFormat::new(Chroma::Yuv420, 8, false).unwrap();
    let mut f = HeifFrame::zeroed(w, h, fmt).unwrap();
    for y in 0..h {
        for x in 0..w {
            let v = (x * 255 / w.max(1) + y * 255 / h.max(1)) / 2;
            let t = ((x * 7 + y * 13) % 23) * 4;
            f.set_sample(0, x, y, ((v + t) % 256) as u16);
        }
    }
    let (cw, ch) = f.plane_dims(1);
    for y in 0..ch {
        for x in 0..cw {
            f.set_sample(1, x, y, (96 + (x * 17 + y * 5) % 64) as u16);
            f.set_sample(2, x, y, (160 - (x * 3 + y * 11) % 64) as u16);
        }
    }
    f
}

fn encode(ctx: &RuntimeContext, id: &str, src: &HeifFrame, opts: &[(&str, &str)]) -> Vec<u8> {
    let (vf, pf) = src.to_core().unwrap();
    let mut params = CodecParameters::video(CodecId::new(id));
    params.width = Some(src.width);
    params.height = Some(src.height);
    params.pixel_format = Some(pf);
    let mut o = CodecOptions::new();
    for (k, v) in opts {
        o = o.set(*k, *v);
    }
    params.options = o;
    let mut enc = ctx
        .codecs
        .first_encoder(&params)
        .unwrap_or_else(|e| panic!("{id} {opts:?}: encoder: {e}"));
    enc.send_frame(&Frame::Video(vf))
        .unwrap_or_else(|e| panic!("{id} {opts:?}: {e}"));
    enc.flush().unwrap();
    enc.receive_packet().unwrap().data
}

fn decode_through(ctx: &RuntimeContext, id: &str, bytes: &[u8]) -> HeifFrame {
    let f = HeifFile::parse(bytes).unwrap();
    let info = oxideav_heif::info(bytes).unwrap();
    let mut params = CodecParameters::video(CodecId::new(id));
    params.width = Some(info.width);
    params.height = Some(info.height);
    let mut dec: Box<dyn Decoder> = ctx.codecs.first_decoder(&params).unwrap();
    assert_eq!(dec.codec_id().as_str(), id);
    let pkt = oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 1), bytes.to_vec())
        .with_pts(0)
        .with_keyframe(true);
    dec.send_packet(&pkt).unwrap();
    let Frame::Video(_) = dec.receive_frame().unwrap() else {
        panic!("video frame")
    };
    drop(f);
    oxideav_heif::decode(bytes).unwrap().to_frame().unwrap()
}

fn has_fourcc(bytes: &[u8], t: &[u8; 4]) -> bool {
    bytes.windows(4).any(|w| w == t)
}

fn psnr_y(a: &HeifFrame, b: &HeifFrame) -> f64 {
    let mut se = 0f64;
    for y in 0..a.height {
        for x in 0..a.width {
            let d = a.sample(0, x, y) as f64 - b.sample(0, x, y) as f64;
            se += d * d;
        }
    }
    let mse = se / (a.width as f64 * a.height as f64);
    10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
}

/// `"avif"` → AV1 in an `avif`-branded file; `"heif"` → HEVC in a
/// `heic` one; each id's decoder opens both.
#[test]
fn avif_codec_id_writes_av1_into_an_avif_branded_file() {
    let ctx = context();
    let src = picture(64, 48);
    let avif = encode(&ctx, "avif", &src, &[]);
    assert_eq!(&avif[4..8], b"ftyp");
    assert_eq!(&avif[8..12], b"avif", "avif major brand");
    assert!(has_fourcc(&avif, b"av01") && !has_fourcc(&avif, b"hvc1"));
    let heic = encode(&ctx, "heif", &src, &[]);
    assert_eq!(&heic[8..12], b"heic", "heic major brand");
    assert!(has_fourcc(&heic, b"hvc1") && !has_fourcc(&heic, b"av01"));
    // The "heif" id is unchanged: codec=hevc by default, codec=av1 on
    // request yields the avif encoder's bytes.
    assert_eq!(encode(&ctx, "heif", &src, &[("codec", "av1")]), avif);
    assert_eq!(encode(&ctx, "avif", &src, &[("codec", "hevc")]), heic);
    for (id, bytes) in [
        ("avif", &avif),
        ("heif", &heic),
        ("avif", &heic),
        ("heif", &avif),
    ] {
        let got = decode_through(&ctx, id, bytes);
        assert_eq!((got.width, got.height), (64, 48), "{id}");
        assert!(psnr_y(&src, &got) > 30.0, "{id}");
    }
}

/// The gateway's `quality` steers HEVC / VVC / AVC through
/// `qp_of_quality` (lossless at 100), AV1 through its own scale: bytes
/// shrink and PSNR falls from 100 to 90 to 50; without `quality` or
/// `qp` the bytes are today's.
#[test]
fn quality_governs_every_codec() {
    let ctx = context();
    let src = picture(64, 48);
    assert_eq!(oxideav_heif::encode::qp_of_quality(100), 0);
    assert_eq!(oxideav_heif::encode::qp_of_quality(90), 5);
    assert_eq!(oxideav_heif::encode::qp_of_quality(50), 26);
    assert_eq!(oxideav_heif::encode::qp_of_quality(0), 51);
    for codec in ["hevc", "av1", "avc", "vvc"] {
        let mut sizes = Vec::new();
        let mut psnrs = Vec::new();
        for q in ["100", "90", "50"] {
            let bytes = encode(&ctx, "heif", &src, &[("codec", codec), ("quality", q)]);
            let got = oxideav_heif::decode(&bytes).unwrap().to_frame().unwrap();
            let p = psnr_y(&src, &got.tight());
            if q == "100" {
                assert_eq!(
                    got.tight().planes,
                    src.tight().planes,
                    "{codec}: quality 100 lossless"
                );
            }
            sizes.push(bytes.len());
            psnrs.push(p);
        }
        assert!(
            sizes[0] > sizes[1] && sizes[1] > sizes[2],
            "{codec}: bytes {sizes:?}"
        );
        assert!(
            psnrs[0] > psnrs[1] && psnrs[1] > psnrs[2],
            "{codec}: PSNR {psnrs:?}"
        );
        // An explicit qp wins over quality on the qp codecs (AV1 has no
        // qp: quality is its own scale); an explicit mode=intra at
        // quality 100 codes at qp 0, lossy.
        if codec != "av1" {
            let qp30 = encode(&ctx, "heif", &src, &[("codec", codec), ("qp", "30")]);
            assert_eq!(
                encode(
                    &ctx,
                    "heif",
                    &src,
                    &[("codec", codec), ("qp", "30"), ("quality", "100")]
                ),
                qp30,
                "{codec}: qp wins"
            );
            let lossy = encode(
                &ctx,
                "heif",
                &src,
                &[("codec", codec), ("mode", "intra"), ("quality", "100")],
            );
            assert_eq!(
                lossy,
                encode(&ctx, "heif", &src, &[("codec", codec), ("qp", "0")]),
                "{codec}: mode=intra at quality 100 is qp 0"
            );
        }
        // Neither given: today's bytes (qp 18 / AV1 quality 60).
        let default = encode(&ctx, "heif", &src, &[("codec", codec)]);
        let explicit = if codec == "av1" {
            encode(&ctx, "heif", &src, &[("codec", codec), ("quality", "60")])
        } else {
            encode(&ctx, "heif", &src, &[("codec", codec), ("qp", "18")])
        };
        assert_eq!(default, explicit, "{codec}: default bytes");
    }
}
