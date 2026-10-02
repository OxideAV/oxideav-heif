//! Low-overhead image files (ISO/IEC 23008-12:2025/Amd 2:2026 Annex
//! O): `encode_still_minimized` writes `ftyp` (`mif3`) + `mini`, the
//! reader expands the box to its O.4 equivalent `meta` + `mdat`, and
//! the pictures decode to exactly what the regular writer's file of
//! the same picture decodes to.
#![cfg(feature = "registry")]

mod common;

use oxideav_heif::decode::{decode_primary, ItemDecoder};
use oxideav_heif::encode::{encode_still, encode_still_minimized, EncodeOptions, StillCodec};
use oxideav_heif::image::Chroma;
use oxideav_heif::miaf::{check, MiafProfile};
use oxideav_heif::mini::MinimizedImage;
use oxideav_heif::props::{Imir, Irot, Property};
use oxideav_heif::{HeifFile, HeifFrame, HeifPixelFormat};

fn picture(w: u32, h: u32, chroma: Chroma, depth: u8) -> HeifFrame {
    let mut f =
        HeifFrame::zeroed(w, h, HeifPixelFormat::new(chroma, depth, false).unwrap()).unwrap();
    let max = (1u32 << depth) - 1;
    for y in 0..h {
        for x in 0..w {
            f.set_sample(0, x, y, ((x * 7 + y * 3) % (max + 1)) as u16);
        }
    }
    if chroma != Chroma::Mono {
        let (cw, ch) = f.plane_dims(1);
        for y in 0..ch {
            for x in 0..cw {
                f.set_sample(1, x, y, ((x * 5 + 40) % (max + 1)) as u16);
                f.set_sample(2, x, y, ((y * 9 + 90) % (max + 1)) as u16);
            }
        }
    }
    f
}

fn alpha_plane(w: u32, h: u32) -> HeifFrame {
    let mut a =
        HeifFrame::zeroed(w, h, HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap()).unwrap();
    for y in 0..h {
        for x in 0..w {
            a.set_sample(0, x, y, if x < w / 2 { 255 } else { (y * 4 % 256) as u16 });
        }
    }
    a
}

/// Every shape the low-overhead writer carries decodes byte-identically
/// to the regular writer's file of the same picture, and the box
/// round-trips through its own parser.
#[test]
fn minimized_files_decode_like_the_regular_writer() {
    let cases: Vec<(&str, StillCodec, HeifFrame, EncodeOptions)> = vec![
        (
            "hevc-pcm",
            StillCodec::Hevc,
            picture(64, 48, Chroma::Yuv420, 8),
            EncodeOptions::default().with_hevc_mode("pcm".into()),
        ),
        (
            "hevc-intra-alpha-meta",
            StillCodec::Hevc,
            picture(96, 80, Chroma::Yuv420, 8)
                .with_alpha_plane(&alpha_plane(96, 80))
                .unwrap(),
            EncodeOptions::default()
                .with_exif(Some(b"II*\0\x08\0\0\0\0\0".to_vec()))
                .with_xmp(Some("<x:xmpmeta>mini</x:xmpmeta>".into()))
                .with_icc_profile(Some(vec![0x42; 64]))
                .with_transforms(vec![
                    Property::Irot(Irot::new(1)),
                    Property::Imir(Imir::new(1)),
                ]),
        ),
        (
            "av1-444-10bit",
            StillCodec::Av1,
            picture(40, 24, Chroma::Yuv444, 10),
            EncodeOptions::default().with_codec(StillCodec::Av1),
        ),
        (
            "av1-mono-large",
            StillCodec::Av1,
            picture(200, 136, Chroma::Mono, 8),
            EncodeOptions::default()
                .with_codec(StillCodec::Av1)
                .with_transforms(vec![Property::Irot(Irot::new(2))]),
        ),
    ];
    for (name, codec, frame, mut opts) in cases {
        opts.codec = codec;
        let mini = encode_still_minimized(&frame, &opts).unwrap();
        assert_eq!(&mini[4..12], b"ftypmif3", "{name}");
        let minor: &[u8; 4] = if codec == StillCodec::Hevc {
            b"heic"
        } else {
            b"avif"
        };
        assert_eq!(
            &mini[12..16],
            minor,
            "{name}: equivalent brand as minor_version"
        );
        let regular = encode_still(&frame, &opts).unwrap();
        assert!(
            mini.len() < regular.len(),
            "{name}: low-overhead {} bytes vs regular {}",
            mini.len(),
            regular.len()
        );
        let f = HeifFile::parse(&mini).unwrap();
        let m = f.minimized.as_ref().expect("mini parsed");
        // The box re-serializes to the same bytes.
        let top = f.original_bytes();
        let mini_box = &top[20..];
        assert_eq!(m.to_box().unwrap(), mini_box, "{name}: box fixed point");
        assert_eq!(MinimizedImage::parse(&mini_box[8..]).unwrap(), *m);
        assert!(
            f.file_type.has_brand(b"mif1"),
            "{name}: O.2.1.2 implied mif1"
        );
        assert_eq!(
            &f.file_type.major_brand, minor,
            "{name}: O.2.1.2 equivalent major"
        );
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{name}: {:#?}", rep.violations);
        let a = decode_primary(&f, ItemDecoder::direct()).unwrap();
        let b = decode_primary(&HeifFile::parse(&regular).unwrap(), ItemDecoder::direct()).unwrap();
        assert_eq!(a.frame, b.frame, "{name}: pixels");
        assert_eq!(a.nclx, b.nclx, "{name}: colour");
        assert_eq!(a.icc_profile, b.icc_profile, "{name}: icc");
        assert_eq!(a.exif, b.exif, "{name}: exif");
        assert_eq!(a.xmp, b.xmp, "{name}: xmp");
        assert_eq!(a.frame.format.has_alpha, frame.format.has_alpha, "{name}");
    }
}

/// A gain map (AV1, single-channel map) travels through the box as the
/// fixed item 3 / 4 pair with the altr group, and reads back with the
/// same metadata and alternate colour.
#[test]
fn minimized_gain_map_round_trips() {
    use oxideav_heif::encode::GainMapSpec;
    use oxideav_heif::gainmap::{GainMapChannel, GainMapMetadata, Rational};
    use oxideav_heif::props::{Clli, Colr};
    let base = picture(64, 48, Chroma::Yuv420, 8);
    let map = picture(32, 24, Chroma::Mono, 8);
    let r = |num: i64, den: u32| Rational { num, den };
    let ch = GainMapChannel::new(r(0, 1), r(2, 1), r(1, 1), r(1, 64), r(1, 64));
    let metadata = GainMapMetadata::new(0, 0, false, true, r(0, 1), r(2, 1), vec![ch]);
    let alternate = Colr::Nclx {
        primaries: 9,
        transfer: 16,
        matrix: 9,
        full_range: true,
    };
    let opts = EncodeOptions::default()
        .with_codec(StillCodec::Av1)
        .with_gain_map(Some(GainMapSpec::new(
            map.clone(),
            metadata.clone(),
            alternate.clone(),
            2,
            true,
            Some(Clli::new(1000, 200)),
            10,
        )));
    let bytes = encode_still_minimized(&base, &opts).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert!(f.file_type.has_brand(b"tmap"), "O.2.1.2 implied tmap");
    let meta = f.meta().unwrap();
    assert_eq!(meta.derivation_inputs(3), vec![1, 4]);
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    let gm = img.gain_map.as_ref().expect("gain map");
    assert_eq!(gm.metadata, metadata);
    assert_eq!(gm.alternate_colr.as_ref(), Some(&alternate));
    assert_eq!(gm.frame.without_alpha(), map);
    let tprops = oxideav_heif::props::ItemProperties::resolve(meta, 3).unwrap();
    assert_eq!(tprops.clli().map(|c| c.max_content_light_level), Some(1000));
    // The regular writer's file of the same inputs decodes the same base
    // and gain map.
    let regular = encode_still(&base, &opts).unwrap();
    let r = decode_primary(&HeifFile::parse(&regular).unwrap(), ItemDecoder::direct()).unwrap();
    assert_eq!(r.frame, img.frame);
    assert_eq!(r.gain_map.unwrap().frame, gm.frame);
    // HEVC codes a luma-only map as 4:2:0: refused, not mislabelled.
    let hevc = opts.clone().with_codec(StillCodec::Hevc);
    assert!(encode_still_minimized(&base, &hevc).is_err());
}

/// Compressed Exif (`dExf`) and deflated XMP (O.4.3) inflate on read;
/// sizes the box cannot carry are refused.
#[test]
fn minimized_compressed_metadata_and_limits() {
    let frame = picture(32, 32, Chroma::Yuv420, 8);
    let opts = EncodeOptions::default().with_hevc_mode("pcm".into());
    let bytes = encode_still_minimized(&frame, &opts).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let mut m = f.minimized.clone().unwrap();
    let tiff = b"MM\0*\0\0\0\x08\0\0".to_vec();
    let mut block = 0u32.to_be_bytes().to_vec();
    block.extend_from_slice(&tiff);
    let xmp = "<x:xmpmeta>deflated</x:xmpmeta>".repeat(20);
    m.exif_xmp_compressed = true;
    m.exif = Some(compcol::vec::compress_to_vec::<compcol::deflate::Deflate>(&block).unwrap());
    m.xmp =
        Some(compcol::vec::compress_to_vec::<compcol::deflate::Deflate>(xmp.as_bytes()).unwrap());
    let file = m.to_file().unwrap();
    let g = HeifFile::parse(&file).unwrap();
    let meta = g.meta().unwrap();
    assert_eq!(meta.item(6).unwrap().item_type, *b"dExf");
    assert_eq!(
        meta.item(7).unwrap().content_encoding.as_deref(),
        Some("deflate")
    );
    let img = decode_primary(&g, ItemDecoder::direct()).unwrap();
    assert_eq!(img.exif.as_deref(), Some(&tiff[..]));
    assert_eq!(img.xmp.as_deref(), Some(xmp.as_str()));
    // No clap slot: a picture off the coding grid is refused.
    assert!(encode_still_minimized(&picture(30, 32, Chroma::Yuv420, 8), &opts).is_err());
    // No grid / thumbnail slot.
    let grid = opts.clone().with_grid_tile(Some(64));
    assert!(encode_still_minimized(&frame, &grid).is_err());
}

/// Third-party readers open the low-overhead file and render it the
/// way they render the regular writer's file of the same picture
/// (`heif-convert`, both codecs; skipped where absent).
#[test]
fn minimized_file_in_black_box_readers() {
    if !common::have_binary("heif-convert") {
        eprintln!("SKIP heif-convert: not installed");
        return;
    }
    let dir = common::scratch_dir("mini");
    for (codec, tag) in [(StillCodec::Hevc, "hevc"), (StillCodec::Av1, "av1")] {
        let frame = picture(64, 48, Chroma::Yuv420, 8);
        let opts = EncodeOptions::default().with_codec(codec);
        let mini = encode_still_minimized(&frame, &opts).unwrap();
        let regular = encode_still(&frame, &opts).unwrap();
        let render = |bytes: &[u8], name: &str| -> Option<Vec<u8>> {
            let src = dir.join(format!("{tag}_{name}.hmg"));
            let png = dir.join(format!("{tag}_{name}.png"));
            std::fs::write(&src, bytes).unwrap();
            let out = std::process::Command::new("heif-convert")
                .arg(&src)
                .arg(&png)
                .output()
                .unwrap();
            if !out.status.success() {
                eprintln!(
                    "heif-convert {name}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                return None;
            }
            Some(std::fs::read(&png).unwrap())
        };
        let a = render(&mini, "mini").expect("the reader opens the low-overhead file");
        let b = render(&regular, "regular").expect("the reader opens the regular file");
        let (pa, pb) = (common::png::read_png(&a), common::png::read_png(&b));
        assert_eq!((pa.width, pa.height), (64, 48), "{tag}");
        assert_eq!(
            pa.samples, pb.samples,
            "{tag}: same render as the regular file"
        );
    }
}
