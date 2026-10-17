//! The gateway contract of the registry encoder (round 476): the
//! gateway's advisory `quality` steers every codec of the `"heif"`
//! encoder, and the `"heif"` muxer writes a foreign `avif` encoder's
//! still stream through like its own (`.avif` encoding goes through
//! oxideav-avif's `avif` encoder on this muxer).
#![cfg(feature = "registry")]

use oxideav_core::{CodecId, CodecOptions, CodecParameters, Frame, RuntimeContext};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};

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

/// The gateway's write path through the muxer: a `"heif"` encoder
/// packet (a `heic` / `heix` file with `hvc1`) and a foreign `avif`
/// encoder's packet — one `avif`-branded file, built here with the
/// contract API the way oxideav-avif's encoder emits it, on a stream
/// whose codec id is `avif` — both pass through the `"heif"` muxer
/// unchanged.
#[test]
fn still_muxer_passes_heif_and_avif_still_streams_through() {
    let ctx = context();
    let src = picture(64, 48);
    let (vf, pf) = src.to_core().unwrap();
    // The heif encoder's packet.
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(src.width);
    params.height = Some(src.height);
    params.pixel_format = Some(pf);
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    enc.send_frame(&Frame::Video(vf)).unwrap();
    enc.flush().unwrap();
    let heif_pkt = enc.receive_packet().unwrap();
    let heif_params = enc.output_params().clone();
    // A foreign avif encoder's packet: one avif-branded file.
    let avif_bytes = oxideav_heif::encode_still(
        &src,
        &oxideav_heif::encode::EncodeOptions::default()
            .with_codec(oxideav_heif::encode::StillCodec::Av1),
    )
    .unwrap();
    let avif_pkt = oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 1), avif_bytes)
        .with_pts(0)
        .with_keyframe(true);
    let mut avif_params = CodecParameters::video(CodecId::new("avif"));
    avif_params.width = Some(src.width);
    avif_params.height = Some(src.height);
    avif_params.pixel_format = Some(pf);
    for (id, params, pkt, brands, item) in [
        (
            "heif",
            heif_params,
            heif_pkt,
            &[b"heic", b"heix"][..],
            b"hvc1",
        ),
        ("avif", avif_params, avif_pkt, &[b"avif"][..], b"av01"),
    ] {
        let stream = oxideav_core::StreamInfo {
            index: 0,
            time_base: oxideav_core::TimeBase::new(1, 1),
            duration: Some(1),
            start_time: Some(0),
            params,
        };
        let path = std::env::temp_dir().join(format!(
            "oxideav-heif-gateway-{id}-{}.bin",
            std::process::id()
        ));
        let sink: Box<dyn oxideav_core::WriteSeek> =
            Box::new(std::fs::File::create(&path).unwrap());
        let mut mx = ctx
            .containers
            .open_muxer("heif", sink, std::slice::from_ref(&stream))
            .unwrap_or_else(|e| panic!("{id}: open_muxer: {e}"));
        mx.write_header().unwrap();
        mx.write_packet(&pkt.clone().with_stream_index(0))
            .unwrap_or_else(|e| panic!("{id}: write_packet: {e}"));
        mx.write_trailer().unwrap();
        drop(mx);
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes, pkt.data, "{id}: the muxer writes the packet through");
        assert_eq!(&bytes[4..8], b"ftyp", "{id}");
        assert!(
            brands.iter().any(|b| &bytes[8..12] == *b),
            "{id}: major brand {:?}",
            std::str::from_utf8(&bytes[8..12])
        );
        assert!(has_fourcc(&bytes, item), "{id}: {item:?} item");
        let got = oxideav_heif::decode(&bytes).unwrap().to_frame().unwrap();
        assert!(psnr_y(&src, &got.tight()) > 30.0, "{id}");
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

/// The AV1 rule of the gateway `quality` (round 476): an explicit
/// `quality` < 100 with no explicit `mode` codes `mode=intra` at that
/// quality — a 16x16 RGB picture through `heif` + `codec=av1` shrinks
/// and loses PSNR from 100 to 90 to 50;
/// `quality` 100 is the lossless coding (`mode=pcm`'s bytes); absent,
/// today's default (quality 60).
#[test]
fn av1_quality_selects_a_lossy_intra_coding() {
    let ctx = context();
    let mut rgb = Vec::new();
    for y in 0..16u32 {
        for x in 0..16u32 {
            rgb.extend_from_slice(&[(x * 16) as u8, (y * 16) as u8, ((x * y) % 256) as u8]);
        }
    }
    let truth = oxideav_heif::HeifImage::new(
        16,
        16,
        oxideav_heif::PixelFormat::Rgb24,
        vec![oxideav_heif::Plane::new(48, rgb.clone())],
    )
    .unwrap();
    let encode_rgb = |id: &str, opts: &[(&str, &str)]| -> Vec<u8> {
        let mut params = CodecParameters::video(CodecId::new(id));
        params.width = Some(16);
        params.height = Some(16);
        params.pixel_format = Some(oxideav_core::PixelFormat::Rgb24);
        let mut o = CodecOptions::new();
        for (k, v) in opts {
            o = o.set(*k, *v);
        }
        params.options = o;
        let mut enc = ctx.codecs.first_encoder(&params).unwrap();
        let vf = oxideav_core::VideoFrame {
            pts: Some(0),
            planes: vec![oxideav_core::VideoPlane {
                stride: 48,
                data: rgb.clone(),
            }],
        };
        enc.send_frame(&Frame::Video(vf)).unwrap();
        enc.flush().unwrap();
        enc.receive_packet().unwrap().data
    };
    let psnr_rgb = |bytes: &[u8]| -> f64 {
        let img = oxideav_heif::decode(bytes).unwrap();
        let (a, b) = (img.to_rgb8(), truth.to_rgb8());
        let se: f64 = a
            .iter()
            .zip(&b)
            .map(|(x, y)| (*x as f64 - *y as f64).powi(2))
            .sum();
        10.0 * (255.0f64 * 255.0 / (se / a.len() as f64).max(1e-9)).log10()
    };
    {
        let (id, base) = ("heif", &[("codec", "av1")][..]);
        let mut sizes = Vec::new();
        let mut psnrs = Vec::new();
        for q in ["100", "90", "50"] {
            let mut opts = base.to_vec();
            opts.push(("quality", q));
            let bytes = encode_rgb(id, &opts);
            assert_eq!(&bytes[8..12], b"avif", "{id} quality {q}: brand");
            assert!(has_fourcc(&bytes, b"av01"), "{id} quality {q}");
            sizes.push(bytes.len());
            psnrs.push(psnr_rgb(&bytes));
        }
        assert!(
            sizes[0] > sizes[1] && sizes[1] > sizes[2],
            "{id}: bytes {sizes:?}"
        );
        assert!(
            psnrs[0] > psnrs[1] && psnrs[1] > psnrs[2],
            "{id}: PSNR {psnrs:?}"
        );
        assert!(
            psnrs[0] > 90.0,
            "{id}: quality 100 is lossless ({:.1} dB)",
            psnrs[0]
        );
        eprintln!("{id}: bytes {sizes:?}, PSNR {psnrs:?}");
        // quality 100 = the lossless mode's bytes; absent = quality 60.
        let mut pcm = base.to_vec();
        pcm.push(("mode", "pcm"));
        let mut q100 = base.to_vec();
        q100.push(("quality", "100"));
        assert_eq!(
            encode_rgb(id, &pcm),
            encode_rgb(id, &q100),
            "{id}: 100 = pcm"
        );
        let mut q60 = base.to_vec();
        q60.push(("quality", "60"));
        assert_eq!(
            encode_rgb(id, base),
            encode_rgb(id, &q60),
            "{id}: default = 60"
        );
    }
}

/// Through `oxideav_core`'s registry alone (no gateway): the `heif`
/// implementation declares its 26 accepted layouts, lossy + lossless,
/// and `params.options` `quality` with `codec=av1` drives the AV1
/// bytes on a 16x16 `Yuv420P` / `Yuv444P` / `Rgb24` frame — 100
/// (lossless, the largest) > none (quality 60) > 50; an `Rgb24` frame
/// at quality 50 comes back as lossy YCbCr, at 100 as the lossless
/// identity `Gbrp8` item.
#[test]
fn registry_heif_capabilities_and_av1_quality_bytes() {
    let ctx = context();
    let impls = ctx.codecs.implementations(&CodecId::new("heif"));
    assert_eq!(impls.len(), 1, "heif: one implementation");
    let heif = impls[0].caps.clone();
    assert_eq!(
        heif.accepted_pixel_formats.len(),
        26,
        "heif accepted layouts"
    );
    assert!(heif.lossy && heif.lossless);
    assert!(
        !ctx.codecs.has_encoder(&CodecId::new("avif")),
        "avif is oxideav-avif's id"
    );
    for pf in [
        oxideav_core::PixelFormat::Yuv420P,
        oxideav_core::PixelFormat::Yuv444P,
        oxideav_core::PixelFormat::Rgb24,
    ] {
        let fmt = oxideav_heif::PixelFormat::try_from(pf).unwrap();
        let planes = match fmt.packed_bytes_per_pixel() {
            Some(b) => vec![oxideav_core::VideoPlane {
                stride: 16 * b,
                data: (0..16 * 16 * b).map(|i| (i * 7 % 251) as u8).collect(),
            }],
            None => (0..fmt.plane_count())
                .map(|p| {
                    let (w, h) = fmt.plane_dims(p, 16, 16);
                    oxideav_core::VideoPlane {
                        stride: w as usize,
                        data: (0..(w * h) as usize).map(|i| (i * 7 % 251) as u8).collect(),
                    }
                })
                .collect(),
        };
        {
            let (id, base) = ("heif", &[("codec", "av1")][..]);
            let encode = |q: Option<&str>| -> (Vec<u8>, oxideav_heif::PixelFormat) {
                let mut params = CodecParameters::video(CodecId::new(id));
                params.width = Some(16);
                params.height = Some(16);
                params.pixel_format = Some(pf);
                let mut o = CodecOptions::new();
                for (k, v) in base {
                    o = o.set(*k, *v);
                }
                if let Some(q) = q {
                    o = o.set("quality", q);
                }
                params.options = o;
                let mut enc = ctx.codecs.first_encoder(&params).unwrap();
                enc.send_frame(&Frame::Video(oxideav_core::VideoFrame {
                    pts: Some(0),
                    planes: planes.clone(),
                }))
                .unwrap();
                enc.flush().unwrap();
                let bytes = enc.receive_packet().unwrap().data;
                assert_eq!(&bytes[8..12], b"avif", "{id} {pf:?} quality {q:?}: brand");
                let label = oxideav_heif::decode(&bytes).unwrap().format;
                (bytes, label)
            };
            let (b100, l100) = encode(Some("100"));
            let (bnone, lnone) = encode(None);
            let (b50, l50) = encode(Some("50"));
            assert!(
                b100.len() > bnone.len() && bnone.len() > b50.len(),
                "{id} {pf:?}: bytes 100 / none / 50 = {} / {} / {}",
                b100.len(),
                bnone.len(),
                b50.len()
            );
            if pf == oxideav_core::PixelFormat::Rgb24 {
                assert_eq!(
                    l100,
                    oxideav_heif::PixelFormat::Gbrp8,
                    "{id}: lossless RGB identity"
                );
                assert_eq!(
                    l50,
                    oxideav_heif::PixelFormat::Yuv420P,
                    "{id}: lossy RGB as YCbCr"
                );
                assert_eq!(lnone, oxideav_heif::PixelFormat::Yuv420P, "{id}");
            } else {
                assert_eq!(
                    (l100, lnone, l50),
                    (fmt, fmt, fmt),
                    "{id} {pf:?}: layout kept"
                );
            }
        }
    }
}
