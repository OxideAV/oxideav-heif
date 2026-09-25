//! Real-world interop, reader side: every file of the vendored producer
//! matrix (`tests/fixtures/interop/`, written by Apple ImageIO `sips`,
//! `heif-enc` (x265 / aom) and `magick`) decodes through this crate
//! — directly and through the framework demuxer + `"heif"` codec — to
//! the geometry / layout the manifest records, matches the black-box
//! video decoder's raw planes fingerprint where the layouts coincide,
//! and matches the black-box HEIF reader's PNG rendering (colour within
//! rounding, alpha exact) where one is vendored.
//!
//! Everything here runs unconditionally: the fixtures and their
//! oracles live in the repository.
#![cfg(feature = "registry")]

mod common;

use std::io::Cursor;
use std::path::PathBuf;

use common::png::{read_png, Png};
use common::{fnv1a64, interop_root};
use oxideav_core::{Frame, RuntimeContext};
use oxideav_heif::decode::{decode_item, decode_primary, ItemDecoder};
use oxideav_heif::image::Chroma;
use oxideav_heif::miaf::{check, MiafProfile};
use oxideav_heif::rgb::to_rgb;
use oxideav_heif::{HeifFile, HeifFrame};

/// One manifest row.
#[derive(Debug)]
struct Row {
    file: String,
    producer: String,
    width: u32,
    height: u32,
    chroma: Chroma,
    depth: u8,
    alpha: bool,
    /// Black-box raw fingerprint, when the layouts coincide.
    bb_raw: Option<(String, usize, u64)>,
    oracle_png: bool,
}

fn manifest() -> Vec<Row> {
    let text = std::fs::read_to_string(interop_root().join("manifest.tsv")).unwrap();
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            assert_eq!(f.len(), 11, "manifest row: {l}");
            let chroma = match f[4] {
                "Mono" => Chroma::Mono,
                "Yuv420" => Chroma::Yuv420,
                "Yuv422" => Chroma::Yuv422,
                "Yuv444" => Chroma::Yuv444,
                other => panic!("chroma {other}"),
            };
            let bb_raw = if f[7] == "-" {
                None
            } else {
                Some((
                    f[7].to_string(),
                    f[8].parse().unwrap(),
                    u64::from_str_radix(f[9], 16).unwrap(),
                ))
            };
            Row {
                file: f[0].to_string(),
                producer: f[1].to_string(),
                width: f[2].parse().unwrap(),
                height: f[3].parse().unwrap(),
                chroma,
                depth: f[5].parse().unwrap(),
                alpha: f[6] == "true",
                bb_raw,
                oracle_png: f[10] == "png",
            }
        })
        .collect()
}

fn path(row: &Row) -> PathBuf {
    interop_root().join(&row.file)
}

fn concat_planes(frame: &HeifFrame) -> Vec<u8> {
    let t = frame.tight();
    let mut out = Vec::new();
    for p in &t.planes {
        out.extend_from_slice(&p.data);
    }
    out
}

/// Compare a decoded frame with the black-box reader's PNG (nearest
/// chroma upsampling on both sides). Returns `(max, mean)` colour
/// difference in 8-bit units and the number of alpha mismatches.
fn compare_png(frame: &HeifFrame, nclx: &oxideav_heif::props::Colr, png: &Png) -> (f64, f64, u64) {
    assert_eq!(
        (png.width, png.height),
        (frame.width, frame.height),
        "geometry"
    );
    let rgb = to_rgb(frame, Some(nclx)).unwrap();
    let ours_max = ((1u32 << rgb.bit_depth) - 1) as f64;
    let png_max = ((1u32 << png.bit_depth) - 1) as f64;
    let mut max: f64 = 0.0;
    let mut sum = 0.0;
    let mut n = 0u64;
    let mut alpha_bad = 0u64;
    let colour_channels = png.channels.clamp(1, 3);
    for y in 0..frame.height {
        for x in 0..frame.width {
            for c in 0..colour_channels {
                let pc = if png.channels < 3 { 0 } else { c };
                let e = png.sample(x, y, pc) as f64 / png_max * 255.0;
                let g = rgb.sample(x, y, c) as f64 / ours_max * 255.0;
                let d = (g - e).abs();
                max = max.max(d);
                sum += d;
                n += 1;
            }
            if png.channels == 2 || png.channels == 4 {
                assert_eq!(
                    rgb.channels, 4,
                    "oracle carries alpha, decoded frame does not"
                );
                let e = png.sample(x, y, png.channels - 1) as f64 / png_max;
                let g = rgb.sample(x, y, 3) as f64 / ours_max;
                if ((e - g).abs() * 255.0).round() > 0.0 {
                    alpha_bad += 1;
                }
            }
        }
    }
    (max, sum / n as f64, alpha_bad)
}

#[test]
fn every_producer_file_decodes_to_the_manifest_layout_and_fingerprint() {
    let rows = manifest();
    assert!(rows.len() >= 39, "manifest has {} rows", rows.len());
    let mut by_producer = std::collections::BTreeMap::<String, (u32, u32)>::new();
    let mut fingerprinted = 0;
    for row in &rows {
        let bytes = std::fs::read(path(row)).unwrap();
        let f = HeifFile::parse(&bytes).unwrap_or_else(|e| panic!("{}: parse: {e}", row.file));
        let img = decode_primary(&f, ItemDecoder::direct())
            .unwrap_or_else(|e| panic!("{}: decode: {e}", row.file));
        assert_eq!(
            (img.width(), img.height()),
            (row.width, row.height),
            "{}: geometry",
            row.file
        );
        assert_eq!(img.frame.format.chroma, row.chroma, "{}: chroma", row.file);
        assert_eq!(img.frame.format.bit_depth, row.depth, "{}: depth", row.file);
        assert_eq!(img.frame.format.has_alpha, row.alpha, "{}: alpha", row.file);
        img.frame.validate().unwrap();
        if let Some((pf, len, fnv)) = &row.bb_raw {
            let ours = concat_planes(&img.frame);
            assert_eq!(ours.len(), *len, "{}: raw byte count ({pf})", row.file);
            assert_eq!(
                fnv1a64(&ours),
                *fnv,
                "{}: planes differ from the black-box decoder's {pf} output",
                row.file
            );
            fingerprinted += 1;
        }
        let e = by_producer.entry(row.producer.clone()).or_insert((0, 0));
        e.0 += 1;
        e.1 += row.bb_raw.is_some() as u32;
    }
    assert!(fingerprinted >= 25, "{fingerprinted} fingerprinted");
    for (p, (n, exact)) in &by_producer {
        eprintln!("{p}: {n} files decoded, {exact} byte-exact against the black-box decoder");
    }
    assert_eq!(by_producer.len(), 3, "three producers");
}

#[test]
fn every_png_oracle_matches_within_rounding_with_exact_alpha() {
    let mut checked = 0;
    for row in manifest().iter().filter(|r| r.oracle_png) {
        let bytes = std::fs::read(path(row)).unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        let png_path = interop_root().join(format!(
            "{}.expected.png",
            row.file.rsplit_once('.').unwrap().0
        ));
        let png = read_png(&std::fs::read(&png_path).unwrap());
        let (max, mean, alpha_bad) = compare_png(&img.frame, &img.nclx, &png);
        eprintln!(
            "{}: max {max:.1} mean {mean:.3} alpha mismatches {alpha_bad}",
            row.file
        );
        // The black-box reader rounds its YCbCr→RGB differently by at
        // most one code (its integer pipeline vs this crate's exact
        // conversion); anything larger is a composition / colour bug.
        assert!(
            max <= 2.0 && mean <= 0.5,
            "{}: colour max {max:.1} mean {mean:.3}",
            row.file
        );
        assert_eq!(alpha_bad, 0, "{}: alpha plane differs", row.file);
        if row.alpha {
            assert_eq!(png.channels, 4, "{}: oracle carries alpha", row.file);
        }
        checked += 1;
    }
    assert!(checked >= 20, "{checked} PNG oracles checked");
}

#[test]
fn framework_path_agrees_with_the_direct_path() {
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    for row in manifest() {
        let bytes = std::fs::read(path(&row)).unwrap();
        let ext = row.file.rsplit_once('.').unwrap().1;
        let mut cur = Cursor::new(bytes.clone());
        let name = ctx.containers.probe_input(&mut cur, Some(ext)).unwrap();
        assert_eq!(name, "heif", "{}", row.file);
        let mut demuxer = ctx
            .containers
            .open_demuxer(&name, Box::new(Cursor::new(bytes.clone())), &ctx.codecs)
            .unwrap();
        let streams = demuxer.streams().to_vec();
        let still = &streams[0];
        assert_eq!(still.params.codec_id.as_str(), "heif", "{}", row.file);
        assert_eq!(
            (still.params.width, still.params.height),
            (Some(row.width), Some(row.height)),
            "{}: announced geometry",
            row.file
        );
        let pf = still.params.pixel_format.unwrap();
        let pkt = demuxer.next_packet().unwrap();
        let mut dec = ctx.codecs.first_decoder(&still.params).unwrap();
        dec.send_packet(&pkt).unwrap();
        dec.flush().unwrap();
        let Frame::Video(v) = dec.receive_frame().unwrap() else {
            panic!("{}: not a video frame", row.file);
        };
        let via_framework = HeifFrame::from_core(&v, row.width, row.height, pf).unwrap();
        let direct = decode_primary(&HeifFile::parse(&bytes).unwrap(), ItemDecoder::direct())
            .unwrap()
            .frame;
        assert_eq!(
            via_framework.format, direct.format,
            "{}: the demuxer announced {pf:?}",
            row.file
        );
        assert_eq!(
            concat_planes(&via_framework),
            concat_planes(&direct),
            "{}: framework planes",
            row.file
        );
        // The .heics still carries a sequence track as stream 1.
        if ext == "heics" {
            assert!(streams.len() >= 2, "{}: sequence track", row.file);
            assert_eq!(streams[1].params.codec_id.as_str(), "h265");
        }
    }
}

#[test]
fn producer_structures_are_surfaced() {
    let root = interop_root();
    // Apple ImageIO tiles anything above 512 px into a grid of hidden
    // hvc1 tiles under a `grid` primary.
    let f =
        HeifFile::from_vec(std::fs::read(root.join("sips_grid_1024x768.heic")).unwrap()).unwrap();
    let meta = f.meta().unwrap();
    let primary = f.primary_item().unwrap();
    assert_eq!(&primary.item_type, b"grid");
    let tiles = meta.derivation_inputs(primary.id);
    assert_eq!(tiles.len(), 4, "2x2 tiles of 512");
    assert!(meta
        .items
        .iter()
        .filter(|i| tiles.contains(&i.id))
        .all(|i| i.is_hidden()));
    let node = oxideav_heif::derived::build_primary_graph(&f).unwrap();
    assert_eq!(node.output_size().unwrap(), (1024, 768));
    assert!(check(&f, MiafProfile::Miaf).unwrap().is_conformant());

    // heif-enc thumbnails: a `thmb` reference to a smaller hvc1 item.
    let f =
        HeifFile::from_vec(std::fs::read(root.join("henc_thumb_rgb_96x80.heic")).unwrap()).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.thumbnail_ids.len(), 1);
    let thumb = decode_item(&f, img.thumbnail_ids[0], ItemDecoder::direct()).unwrap();
    assert!(
        thumb.width() <= 32 && thumb.height() <= 32,
        "{}x{}",
        thumb.width(),
        thumb.height()
    );

    // Alpha auxiliaries from all three producers attach as a plane.
    for name in [
        "sips_rgba_80x64.heic",
        "henc_rgba_80x64.heic",
        "magick_rgba_80x64.heic",
    ] {
        let f = HeifFile::from_vec(std::fs::read(root.join(name)).unwrap()).unwrap();
        let node = oxideav_heif::derived::build_primary_graph(&f).unwrap();
        assert!(node.alpha.is_some(), "{name}: auxl alpha");
        let a = node.alpha.as_ref().unwrap();
        assert!(
            a.properties.auxc().is_some(),
            "{name}: auxC on the alpha item"
        );
    }

    // Apple ImageIO writes only an ICC `prof` colr for gray sources: the
    // CICP falls back to the MIAF default.
    let f = HeifFile::from_vec(std::fs::read(root.join("sips_gray_96x80.heic")).unwrap()).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert!(img.icc_profile.is_some());
    assert!(!img.nclx_explicit);
    assert_eq!(img.nclx, oxideav_heif::props::Colr::MIAF_DEFAULT);
    // ... while its RGB sources carry an unspecified-primaries nclx.
    let f = HeifFile::from_vec(std::fs::read(root.join("sips_rgb_96x80.heic")).unwrap()).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert!(img.nclx_explicit);
    assert!(matches!(
        img.nclx,
        oxideav_heif::props::Colr::Nclx {
            primaries: 2,
            matrix: 6,
            full_range: true,
            ..
        }
    ));

    // Lossless heif-enc / aom output is RGB coded with the identity matrix.
    for name in [
        "henc_lossless_rgb_96x80.heic",
        "henc_avif_lossless_rgb_96x80.avif",
    ] {
        let f = HeifFile::from_vec(std::fs::read(root.join(name)).unwrap()).unwrap();
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        assert!(
            matches!(img.nclx, oxideav_heif::props::Colr::Nclx { matrix: 0, .. }),
            "{name}: identity matrix"
        );
        assert_eq!(img.frame.format.chroma, Chroma::Yuv444);
    }

    // Every file passes the MIAF structural checks.
    for row in manifest() {
        let f = HeifFile::from_vec(std::fs::read(path(&row)).unwrap()).unwrap();
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{}: {:#?}", row.file, rep.violations);
    }
}
