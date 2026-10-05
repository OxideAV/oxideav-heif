//! The image-crate contract surface: `probe` / `info` standalone,
//! `decode*` / `decode_all` / `encode*` with the `registry` feature,
//! checked against the corpus oracles and for byte-identity with the
//! framework adapters.

mod common;

use common::{all_bundles, fixture_bytes, fixture_root, interop_root};
use oxideav_heif::{info, probe, HeifError, PixelFormat};

#[test]
fn probe_accepts_every_fixture_and_rejects_junk() {
    for (root, bundle) in all_bundles() {
        let bytes = fixture_bytes(&root, bundle);
        assert!(probe(&bytes), "{bundle}");
        assert!(probe(&bytes[..32]), "{bundle}: the ftyp alone suffices");
    }
    for entry in std::fs::read_dir(interop_root()).unwrap() {
        let p = entry.unwrap().path();
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "heic" || ext == "avif" {
            assert!(probe(&std::fs::read(&p).unwrap()), "{}", p.display());
        }
    }
    assert!(!probe(b""));
    assert!(!probe(b"\x89PNG\r\n\x1a\n"));
    assert!(!probe(&[0u8; 64]));
}

#[test]
fn info_reads_the_header_without_a_codec() {
    let Some(root) = fixture_root() else {
        return;
    };
    for (root, bundle) in all_bundles() {
        let bytes = fixture_bytes(&root, bundle);
        let i = info(&bytes).unwrap_or_else(|e| panic!("{bundle}: {e}"));
        let png =
            common::png::read_png(&std::fs::read(root.join(bundle).join("expected.png")).unwrap());
        assert_eq!((i.width, i.height), (png.width, png.height), "{bundle}");
        let frames = match bundle {
            "multi-image-burst-3" => 3,
            "image-sequence-3frame" => 4,
            _ => 1,
        };
        assert_eq!(i.frames, frames, "{bundle}: frames");
        assert_eq!(i.has_alpha, bundle == "still-image-with-alpha", "{bundle}");
        assert_eq!(i.has_exif, bundle == "still-image-with-exif", "{bundle}");
        assert_eq!(i.has_xmp, bundle == "still-image-with-xmp", "{bundle}");
        assert_eq!(i.has_icc, bundle == "still-image-with-icc", "{bundle}");
        assert!(i.primary_item_id.is_some(), "{bundle}");
        assert!(!i.has_gain_map, "{bundle}");
    }
    let i = info(&fixture_bytes(&root, "still-10bit-main10")).unwrap();
    assert_eq!(i.format, PixelFormat::Yuv420P10Le);
    assert_eq!(
        info(&fixture_bytes(&root, "still-monochrome"))
            .unwrap()
            .format,
        PixelFormat::Gray8
    );
    assert_eq!(
        info(&fixture_bytes(&root, "still-yuv444")).unwrap().format,
        PixelFormat::Yuv444P
    );
    assert_eq!(
        info(&fixture_bytes(&root, "still-image-with-alpha"))
            .unwrap()
            .format,
        PixelFormat::Yuva420P
    );
    // The header never panics on truncation.
    let full = fixture_bytes(&root, "still-image-grid-2x2");
    for cut in (0..full.len()).step_by(37) {
        let _ = info(&full[..cut]);
    }
}

#[test]
fn error_type_has_the_contract_variants() {
    let e: oxideav_heif::Error = HeifError::limit("x");
    assert!(matches!(e, HeifError::LimitExceeded(_)));
    let e: HeifError = std::io::Error::other("disk").into();
    assert!(matches!(&e, HeifError::Io(io) if io.kind() == std::io::ErrorKind::Other));
    assert!(std::error::Error::source(&e).is_some());
    assert_eq!(e.to_string(), "heif: i/o: disk");
}

#[cfg(feature = "registry")]
mod registry {
    use super::*;
    use std::io::Cursor;

    use common::png::Png;
    use oxideav_heif::{
        decode, decode_all, decode_from, decode_rgb8, decode_rgba8, decode_with, encode,
        encode_all, encode_rgb8, encode_rgba8, encode_to, ColorInfo, ColorRange, DecodeOptions,
        EncodeOptions, Frame, HeifImage, Metadata, Plane,
    };

    const MEAN_TOL: f64 = 1.5;
    const MAX_TOL: f64 = 12.0;

    /// Compare tightly packed RGBA8 against a PNG oracle (8- or 16-bit,
    /// 1–4 channels), in 8-bit units.
    fn compare_rgba8(rgba: &[u8], w: u32, h: u32, png: &Png) -> (f64, f64) {
        assert_eq!((png.width, png.height), (w, h), "geometry");
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        let to8 = |v: u16| -> f64 {
            if png.bit_depth == 16 {
                (v as f64 / 257.0).round()
            } else {
                v as f64
            }
        };
        let (mut max, mut sum, mut n) = (0f64, 0f64, 0u64);
        for y in 0..h {
            for x in 0..w {
                let px = &rgba[((y * w + x) * 4) as usize..][..4];
                let want: [f64; 4] = match png.channels {
                    1 => {
                        let g = to8(png.sample(x, y, 0));
                        [g, g, g, 255.0]
                    }
                    2 => {
                        let g = to8(png.sample(x, y, 0));
                        [g, g, g, to8(png.sample(x, y, 1))]
                    }
                    3 => [
                        to8(png.sample(x, y, 0)),
                        to8(png.sample(x, y, 1)),
                        to8(png.sample(x, y, 2)),
                        255.0,
                    ],
                    _ => [
                        to8(png.sample(x, y, 0)),
                        to8(png.sample(x, y, 1)),
                        to8(png.sample(x, y, 2)),
                        to8(png.sample(x, y, 3)),
                    ],
                };
                for c in 0..4 {
                    let d = (px[c] as f64 - want[c]).abs();
                    max = max.max(d);
                    sum += d;
                    n += 1;
                }
            }
        }
        (sum / n as f64, max)
    }

    fn oracle(root: &std::path::Path, bundle: &str, name: &str) -> Png {
        common::png::read_png(&std::fs::read(root.join(bundle).join(name)).unwrap())
    }

    #[test]
    fn decode_rgba8_matches_every_oracle() {
        for (root, bundle) in all_bundles() {
            let bytes = fixture_bytes(&root, bundle);
            let img = decode_rgba8(&bytes).unwrap_or_else(|e| panic!("{bundle}: {e}"));
            let png = oracle(&root, bundle, "expected.png");
            let (mean, max) = compare_rgba8(&img.data, img.width, img.height, &png);
            assert!(
                mean <= MEAN_TOL && max <= MAX_TOL,
                "{bundle}: mean {mean:.3} max {max:.1}"
            );
            // The RGB path is the RGBA path minus alpha.
            let rgb = decode_rgb8(&bytes).unwrap();
            assert_eq!(rgb.data.len(), (img.width * img.height * 3) as usize);
            for (p3, p4) in rgb.data.chunks_exact(3).zip(img.data.chunks_exact(4)) {
                assert_eq!(p3, &p4[..3], "{bundle}");
            }
            // The native image carries the header's layout and metadata.
            let native = decode(&bytes).unwrap();
            let i = info(&bytes).unwrap();
            assert_eq!(native.format, i.format, "{bundle}");
            assert_eq!(
                (native.width, native.height),
                (i.width, i.height),
                "{bundle}"
            );
            assert_eq!(native.color, i.color, "{bundle}");
            assert_eq!(native.metadata.exif.is_some(), i.has_exif, "{bundle}");
            assert_eq!(native.metadata.xmp.is_some(), i.has_xmp, "{bundle}");
            assert_eq!(native.metadata.icc.is_some(), i.has_icc, "{bundle}");
            assert_eq!(native.format.has_alpha(), i.has_alpha, "{bundle}");
            assert!(native.palette.is_none());
            assert_eq!(
                native.to_rgba8(),
                img.data,
                "{bundle}: to_rgba8 is the one-call path"
            );
            native.validate().unwrap();
        }
    }

    #[test]
    fn decode_all_yields_bursts_then_sequences() {
        let Some(root) = fixture_root() else {
            return;
        };
        let burst = decode_all(&fixture_bytes(&root, "multi-image-burst-3")).unwrap();
        assert_eq!(burst.len(), 3);
        assert_eq!(
            burst.iter().map(|f| f.item_id).collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3)]
        );
        for (i, f) in burst.iter().enumerate() {
            assert!(f.delay.is_none() && f.track_id.is_none());
            let png = oracle(&root, "multi-image-burst-3", &format!("expected_{i}.png"));
            let (mean, max) =
                compare_rgba8(&f.image.to_rgba8(), f.image.width, f.image.height, &png);
            assert!(
                mean <= MEAN_TOL && max <= MAX_TOL,
                "burst {i}: {mean:.3} / {max:.1}"
            );
        }

        let seq = decode_all(&fixture_bytes(&root, "image-sequence-3frame")).unwrap();
        assert_eq!(seq.len(), 4, "the still item, then the three samples");
        assert_eq!(seq[0].item_id, Some(1));
        assert!(seq[0].delay.is_none());
        for (i, f) in seq[1..].iter().enumerate() {
            assert!(f.item_id.is_none());
            assert!(f.track_id.is_some());
            let delay = f.delay.expect("sample duration");
            assert!(delay > std::time::Duration::ZERO);
            assert_eq!(delay, seq[1].delay.unwrap(), "constant frame rate");
            let png = oracle(&root, "image-sequence-3frame", &format!("expected_{i}.png"));
            let (mean, max) =
                compare_rgba8(&f.image.to_rgba8(), f.image.width, f.image.height, &png);
            assert!(
                mean <= MEAN_TOL && max <= MAX_TOL,
                "sample {i}: {mean:.3} / {max:.1}"
            );
        }
        // A plain still is one frame, and it is the primary.
        let one = decode_all(&fixture_bytes(&root, "still-yuv444")).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(
            one[0].image,
            decode(&fixture_bytes(&root, "still-yuv444")).unwrap()
        );
    }

    #[test]
    fn decode_options_limits_strictness_and_selection() {
        let Some(root) = fixture_root() else {
            return;
        };
        let bytes = fixture_bytes(&root, "single-image-with-thumbnail");
        let i = info(&bytes).unwrap();
        let too_narrow = DecodeOptions::default().with_max_width(Some(i.width - 1));
        assert!(matches!(
            decode_with(&bytes, &too_narrow),
            Err(HeifError::LimitExceeded(_))
        ));
        let too_many = DecodeOptions::default().with_max_pixels(Some(1));
        assert!(matches!(
            decode_with(&bytes, &too_many),
            Err(HeifError::LimitExceeded(_))
        ));
        let too_big = DecodeOptions::default().with_max_bytes(Some(bytes.len() as u64 - 1));
        let unlimited = DecodeOptions::default()
            .with_max_width(None)
            .with_max_height(None)
            .with_max_pixels(None)
            .with_max_bytes(None);
        assert!(decode_with(&bytes, &unlimited).is_ok());
        assert!(matches!(
            decode_with(&bytes, &too_big),
            Err(HeifError::LimitExceeded(_))
        ));
        assert!(matches!(
            oxideav_heif::decode_all_with(&bytes, &too_big),
            Err(HeifError::LimitExceeded(_))
        ));
        // Item selection: the thumbnail decodes as its own image.
        let primary = decode(&bytes).unwrap();
        let thumb_id = oxideav_heif::HeifFile::parse_borrowed(&bytes)
            .unwrap()
            .meta()
            .unwrap()
            .thumbnails_of(i.primary_item_id.unwrap())[0];
        let thumb = decode_with(
            &bytes,
            &DecodeOptions::default().with_item_id(Some(thumb_id)),
        )
        .unwrap();
        assert!(thumb.width < primary.width);
        // Strict refuses a file whose ftyp carries only generic brands;
        // the lenient default still reads its item tree.
        let mut generic = bytes.clone();
        let ftyp_len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        generic[8..12].copy_from_slice(b"isom");
        for off in (16..ftyp_len).step_by(4) {
            generic[off..off + 4].copy_from_slice(b"mp42");
        }
        assert!(!probe(&generic));
        assert_eq!(decode(&generic).unwrap().planes, primary.planes);
        assert!(matches!(
            decode_with(&generic, &DecodeOptions::default().with_strict(true)),
            Err(HeifError::InvalidData(_))
        ));
        assert_eq!(
            decode_with(&bytes, &DecodeOptions::default().with_strict(true))
                .unwrap()
                .planes,
            primary.planes
        );
        // Streaming entry point.
        assert_eq!(decode_from(Cursor::new(&bytes)).unwrap(), primary);
        // Hostile input never panics.
        for cut in (0..bytes.len()).step_by(53) {
            let _ = decode(&bytes[..cut]);
            let _ = decode_all(&bytes[..cut]);
        }
    }

    /// Colour constant over 2×2 blocks so 4:2:0 coding is lossless.
    fn blocky_rgba(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let (bx, by) = (x / 2, y / 2);
                out.extend_from_slice(&[
                    (bx * 255 / (w / 2).max(1)) as u8,
                    (by * 255 / (h / 2).max(1)) as u8,
                    ((bx + by) * 255 / ((w + h) / 2).max(1)) as u8,
                    (64 + by * 191 / (h / 2).max(1)) as u8,
                ]);
            }
        }
        out
    }

    fn rgb_of(rgba: &[u8]) -> Vec<u8> {
        rgba.chunks_exact(4).flat_map(|p| p[..3].to_vec()).collect()
    }

    #[test]
    fn encode_rgb8_and_rgba8_round_trip_at_the_production_layout() {
        let (w, h) = (48u32, 40u32);
        let rgba = blocky_rgba(w, h);
        let rgb = rgb_of(&rgba);
        // Lossless (`pcm`): an RGB source is an identity-matrix 4:4:4
        // item — the file says what it holds and the round trip is exact.
        let pcm = EncodeOptions::default().with_hevc_mode("pcm".into());
        let bytes = encode_rgb8(w, h, &rgb, &pcm).unwrap();
        assert!(probe(&bytes));
        let i = info(&bytes).unwrap();
        assert_eq!((i.width, i.height, i.format), (w, h, PixelFormat::Gbrp8));
        assert_eq!(
            i.color,
            ColorInfo::new(ColorRange::Full, 1, 13, 0),
            "BT.709 / sRGB code points, identity matrix, full range"
        );
        let back = decode_rgb8(&bytes).unwrap();
        assert_eq!(back.data, rgb, "lossless RGB round trip is exact");
        // Alpha rides as the auxiliary item, exactly.
        let bytes = encode_rgba8(w, h, &rgba, &pcm).unwrap();
        let i = info(&bytes).unwrap();
        assert!(i.has_alpha);
        assert_eq!(i.format, PixelFormat::Gbrap8);
        let back = decode_rgba8(&bytes).unwrap();
        assert_eq!(back.data, rgba, "lossless RGBA round trip is exact");
        // The production (lossy) layout: 4:2:0 YCbCr through the MIAF
        // default colr, a smaller and decodable file.
        let lossy = encode_rgb8(w, h, &rgb, &EncodeOptions::default()).unwrap();
        assert!(lossy.len() < bytes.len());
        let i = info(&lossy).unwrap();
        assert_eq!((i.width, i.height, i.format), (w, h, PixelFormat::Yuv420P));
        assert_eq!(i.color, ColorInfo::default(), "the MIAF default colr");
        assert_eq!(decode_rgb8(&lossy).unwrap().width, w);
        let lossy = encode_rgba8(w, h, &rgba, &EncodeOptions::default()).unwrap();
        assert_eq!(info(&lossy).unwrap().format, PixelFormat::Yuva420P);
        let back = decode_rgba8(&lossy).unwrap();
        assert_eq!((back.width, back.height), (w, h));
        assert_eq!(back.data.len(), rgba.len());
        // Bad input sizes are refused, not read past.
        assert!(matches!(
            encode_rgb8(w, h, &rgb[..rgb.len() - 1], &pcm),
            Err(HeifError::InvalidData(_))
        ));
        assert!(encode_rgb8(0, 0, &[], &pcm).is_err());
    }

    #[test]
    fn encode_of_a_native_image_round_trips_planes_and_metadata() {
        let Some(root) = fixture_root() else {
            return;
        };
        let src = decode(&fixture_bytes(&root, "still-image-with-exif")).unwrap();
        let mut img = src.clone();
        img.metadata = Metadata::new(
            Some(vec![0u8; 128]),
            src.metadata.exif.clone(),
            Some(b"<x:xmpmeta>contract</x:xmpmeta>".to_vec()),
            None,
        );
        let pcm = EncodeOptions::default().with_hevc_mode("pcm".into());
        let bytes = encode(&img, &pcm).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back.format, img.format);
        assert_eq!((back.width, back.height), (img.width, img.height));
        assert_eq!(back.planes, img.planes, "lossless: planes identical");
        // With an ICC profile attached the nclx written next to it has
        // primaries / transfer 2 (HEIF §6.5.5: the ICC governs colour);
        // matrix and range are the image's.
        assert_eq!(
            back.color,
            ColorInfo::new(oxideav_heif::ColorRange::Full, 2, 2, 6),
            "nclx beside an ICC"
        );
        assert_eq!(back.metadata, img.metadata, "icc / exif / xmp carried");
        // encode_to writes the same bytes.
        let mut out = Vec::new();
        encode_to(&img, &pcm, &mut out).unwrap();
        assert_eq!(out, bytes);
        // Without an ICC the image's colour is the colr written: the
        // source's own, and a limited-range BT.709 description.
        let back = decode(&encode(&src, &pcm).unwrap()).unwrap();
        assert_eq!(
            back.color, src.color,
            "the image's colour is the colr written"
        );
        let bt709 =
            src.clone()
                .with_color(ColorInfo::new(oxideav_heif::ColorRange::Limited, 1, 1, 1));
        let back = decode(&encode(&bt709, &pcm).unwrap()).unwrap();
        assert_eq!(back.color, bt709.color);
        assert_eq!(back.planes, bt709.planes);
        // Options' metadata wins over the image's.
        let opts = pcm.clone().with_xmp(Some("<x>opts</x>".into()));
        let back = decode(&encode(&img, &opts).unwrap()).unwrap();
        assert_eq!(back.metadata.xmp.as_deref(), Some(&b"<x>opts</x>"[..]));
        // A malformed image is refused before anything is coded.
        assert!(matches!(
            HeifImage::new(4, 4, PixelFormat::Yuv420P, vec![Plane::new(4, vec![0; 16])]),
            Err(HeifError::InvalidData(_))
        ));
        assert!(matches!(
            HeifImage::from_rgb8(4, 4, vec![0; 3]),
            Err(HeifError::InvalidData(_))
        ));
        let mut bad = HeifImage::from_rgb8(4, 4, vec![0; 48]).unwrap();
        bad.planes[0].data.truncate(3);
        assert!(matches!(encode(&bad, &pcm), Err(HeifError::InvalidData(_))));
    }

    /// A planar 4:2:0 picture with a per-frame pattern, limited BT.709.
    fn yuv420_frame(w: u32, h: u32, seed: u8) -> HeifImage {
        let (wu, hu) = (w as usize, h as usize);
        let (cw, ch) = (wu.div_ceil(2), hu.div_ceil(2));
        let y: Vec<u8> = (0..wu * hu)
            .map(|i| 16 + ((i as u32 * 7 + seed as u32 * 31) % 220) as u8)
            .collect();
        let cb: Vec<u8> = (0..cw * ch)
            .map(|i| 16 + ((i * 3 + seed as usize) % 224) as u8)
            .collect();
        let cr: Vec<u8> = (0..cw * ch)
            .map(|i| 16 + ((i * 5 + seed as usize * 2) % 224) as u8)
            .collect();
        HeifImage::new(
            w,
            h,
            PixelFormat::Yuv420P,
            vec![Plane::new(wu, y), Plane::new(cw, cb), Plane::new(cw, cr)],
        )
        .unwrap()
        .with_color(ColorInfo::new(oxideav_heif::ColorRange::Limited, 1, 1, 1))
    }

    #[test]
    fn encode_all_mirrors_decode_all() {
        use std::time::Duration;
        let pcm = EncodeOptions::default().with_hevc_mode("pcm".into());
        let (w, h) = (16u32, 8u32);

        // One delay-less frame is exactly `encode`.
        let f0 = yuv420_frame(w, h, 1);
        let one = encode_all(&[Frame::new(f0.clone(), None, None, None)], &pcm).unwrap();
        assert_eq!(one, encode(&f0, &pcm).unwrap());

        // Delay-less frames: a burst of image items, the first primary;
        // the lossless mode reads back the planes, colour and ids.
        let burst: Vec<Frame> = (1..=3)
            .map(|i| Frame::new(yuv420_frame(w, h, i), None, None, None))
            .collect();
        let bytes = encode_all(&burst, &pcm).unwrap();
        let back = decode_all(&bytes).unwrap();
        assert_eq!(back.len(), 3);
        for (i, (got, want)) in back.iter().zip(&burst).enumerate() {
            assert_eq!(got.image, want.image, "burst item {i}");
            assert!(got.delay.is_none() && got.track_id.is_none());
            assert!(got.item_id.is_some());
        }
        assert_eq!(
            decode(&bytes).unwrap(),
            burst[0].image,
            "frame 0 is the primary"
        );

        // Timed frames: an image sequence; planes, colour and delays
        // read back equal (with the MIAF cover still aliasing sample 0).
        let delays = [40u64, 1500, 1];
        let seq: Vec<Frame> = delays
            .iter()
            .enumerate()
            .map(|(i, &ms)| {
                Frame::new(
                    yuv420_frame(w, h, 10 + i as u8),
                    Some(Duration::from_millis(ms)),
                    None,
                    None,
                )
            })
            .collect();
        let bytes = encode_all(&seq, &pcm).unwrap();
        let back = decode_all(&bytes).unwrap();
        assert_eq!(back.len(), 4, "the cover item, then the three samples");
        assert_eq!(back[0].image, seq[0].image, "the cover is sample 0");
        assert!(back[0].delay.is_none() && back[0].item_id.is_some());
        for (i, (got, want)) in back[1..].iter().zip(&seq).enumerate() {
            assert_eq!(got.image, want.image, "sample {i}");
            assert_eq!(got.delay, want.delay, "sample {i} delay");
            assert_eq!(got.track_id, Some(1));
            assert!(got.item_id.is_none());
        }
        let f = oxideav_heif::HeifFile::parse(&bytes).unwrap();
        let rep = oxideav_heif::miaf::check(&f, oxideav_heif::miaf::MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{:#?}", rep.violations);

        // Mixed, as `decode_all` returns them: items first, then samples.
        let mut mixed = burst[..2].to_vec();
        mixed.extend(seq.iter().cloned());
        let bytes = encode_all(&mixed, &pcm).unwrap();
        let back = decode_all(&bytes).unwrap();
        assert_eq!(back.len(), 5);
        for (i, (got, want)) in back.iter().zip(&mixed).enumerate() {
            assert_eq!(got.image, want.image, "mixed {i}");
            assert_eq!(got.delay, want.delay, "mixed {i} delay");
        }

        // Packed RGBA frames with delays: the lossy default keeps the
        // geometry, delays and alpha presence; alpha rides its own track.
        let rgba: Vec<Frame> = (0..2u8)
            .map(|i| {
                let px: Vec<u8> = (0..(w * h) as usize)
                    .flat_map(|p| {
                        let v = ((p * 13 + i as usize * 50) % 256) as u8;
                        [v, 255 - v, 128, if p % 2 == 0 { 255 } else { 64 }]
                    })
                    .collect();
                Frame::new(
                    HeifImage::from_rgba8(w, h, px).unwrap(),
                    Some(Duration::from_millis(100)),
                    None,
                    None,
                )
            })
            .collect();
        let bytes = encode_all(&rgba, &EncodeOptions::default()).unwrap();
        let back = decode_all(&bytes).unwrap();
        assert_eq!(back.len(), 3);
        for got in &back[1..] {
            assert_eq!((got.image.width, got.image.height), (w, h));
            assert!(got.image.format.has_alpha(), "{:?}", got.image.format);
            assert_eq!(got.delay, Some(Duration::from_millis(100)));
            let a: Vec<u8> = got.image.to_rgba8().chunks_exact(4).map(|p| p[3]).collect();
            assert!(
                a.iter().step_by(2).all(|&v| v >= 250),
                "alpha even pixels opaque"
            );
            assert!(a.iter().skip(1).step_by(2).all(|&v| (60..=68).contains(&v)));
        }

        // Rejections: nothing, or frames that do not share frame 0's geometry.
        assert!(matches!(
            encode_all(&[], &pcm),
            Err(HeifError::InvalidData(_))
        ));
        let odd = vec![
            seq[0].clone(),
            Frame::new(
                yuv420_frame(w * 2, h, 3),
                Some(Duration::from_millis(5)),
                None,
                None,
            ),
        ];
        assert!(matches!(
            encode_all(&odd, &pcm),
            Err(HeifError::InvalidData(_))
        ));
    }

    #[test]
    fn av1_keeps_the_requested_chroma_for_rgb_sources() {
        let (w, h) = (32u32, 24u32);
        let rgb = rgb_of(&blocky_rgba(w, h));
        // Lossless (the standalone default: no quality) codes an RGB
        // source as an identity-matrix 4:4:4 item, exactly.
        let av1 = EncodeOptions::default().with_codec(oxideav_heif::encode::StillCodec::Av1);
        let bytes = encode_rgb8(w, h, &rgb, &av1).unwrap();
        assert_eq!(info(&bytes).unwrap().format, PixelFormat::Gbrp8);
        assert_eq!(decode_rgb8(&bytes).unwrap().data, rgb);
        // Lossy coding converts to YCbCr at the requested chroma.
        let lossy = av1.with_av1_quality(Some(60));
        let bytes = encode_rgb8(w, h, &rgb, &lossy).unwrap();
        assert_eq!(info(&bytes).unwrap().format, PixelFormat::Yuv420P);
        assert_eq!(info(&bytes).unwrap().color.matrix, 6);
        assert_eq!(decode_rgb8(&bytes).unwrap().data.len(), rgb.len());
        let bytes = encode_rgb8(
            w,
            h,
            &rgb,
            &lossy.with_chroma(Some(oxideav_heif::Chroma::Yuv444)),
        )
        .unwrap();
        assert_eq!(info(&bytes).unwrap().format, PixelFormat::Yuv444P);
        assert_eq!(decode_rgb8(&bytes).unwrap().data.len(), rgb.len());
    }

    #[test]
    fn framework_adapters_are_byte_identical_to_the_contract_path() {
        use oxideav_core::{CodecId, CodecOptions, CodecParameters, Frame, VideoFrame, VideoPlane};
        let mut ctx = oxideav_core::RuntimeContext::new();
        oxideav_h265::register(&mut ctx);
        oxideav_av1::register(&mut ctx);
        oxideav_heif::register(&mut ctx);
        let (w, h) = (40u32, 32u32);
        let rgba = blocky_rgba(w, h);
        // Encoder: the "heif" codec over a packed RGBA frame == encode_rgba8.
        let mut params = CodecParameters::video(CodecId::new("heif"));
        params.width = Some(w);
        params.height = Some(h);
        params.pixel_format = Some(oxideav_core::PixelFormat::Rgba);
        params.options = CodecOptions::new().set("mode", "pcm");
        let mut enc = ctx.codecs.first_encoder(&params).unwrap();
        enc.send_frame(&Frame::Video(VideoFrame {
            pts: Some(0),
            planes: vec![VideoPlane {
                stride: w as usize * 4,
                data: rgba.clone(),
            }],
        }))
        .unwrap();
        enc.flush().unwrap();
        let via_codec = enc.receive_packet().unwrap().data;
        let pcm = EncodeOptions::default().with_hevc_mode("pcm".into());
        assert_eq!(via_codec, encode_rgba8(w, h, &rgba, &pcm).unwrap());
        // Decoder: the "heif" codec's frame == decode().into_video_frame().
        let mut dparams = CodecParameters::video(CodecId::new("heif"));
        dparams.width = Some(w);
        dparams.height = Some(h);
        let mut dec = ctx.codecs.first_decoder(&dparams).unwrap();
        dec.send_packet(
            &oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 1), via_codec.clone())
                .with_keyframe(true),
        )
        .unwrap();
        dec.flush().unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            panic!("video frame");
        };
        let img = decode(&via_codec).unwrap();
        let (want, pf) = img.clone().into_video_frame();
        assert_eq!(vf.planes.len(), want.planes.len());
        for (a, b) in vf.planes.iter().zip(&want.planes) {
            assert_eq!((a.stride, &a.data), (b.stride, &b.data));
        }
        assert_eq!(vf.color_signal(), want.color_signal());
        // Lossless RGBA: the identity-matrix 4:4:4 item, planar RGB + alpha.
        assert_eq!(pf, oxideav_core::PixelFormat::Gbrap8);
        // And back through the bridge.
        dparams.pixel_format = Some(pf);
        let back = HeifImage::from_video_frame(&vf, &dparams).unwrap();
        assert_eq!(back.planes, img.planes);
        assert_eq!(back.format, img.format);
        assert_eq!(back.color, img.color);
    }
}
