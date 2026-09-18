//! End-to-end: every corpus bundle decoded through the framework codecs
//! and compared with its `expected*.png` oracle (rendered by a
//! black-box HEIF reader) and, where the primary is a plain YCbCr
//! picture, byte-exact with a black-box video decoder.
#![cfg(feature = "registry")]

mod common;

use common::png::{read_png, Png};
use common::{fixture_bytes, fixture_root, BUNDLES};
use oxideav_heif::decode::{decode_item, decode_primary, ItemDecoder};
use oxideav_heif::image::Chroma;
use oxideav_heif::props::Colr;
use oxideav_heif::{HeifFile, HeifFrame};

/// Convert one output pixel to 8-bit (or 16-bit for >8-bit sources)
/// RGB with the item's `nclx` matrix, nearest chroma siting. The
/// oracle used its own conversion, so comparisons carry a tolerance.
fn to_rgb(frame: &HeifFrame, nclx: &Colr, x: u32, y: u32) -> [f64; 3] {
    let depth = frame.format.bit_depth as u32;
    let max = ((1u32 << depth) - 1) as f64;
    let (matrix, full) = match nclx {
        Colr::Nclx {
            matrix, full_range, ..
        } => (*matrix, *full_range),
        _ => (6, true),
    };
    let yv = frame.sample(0, x, y) as f64;
    if frame.format.chroma == Chroma::Mono {
        let g = if full {
            yv / max
        } else {
            (yv - 16.0 * (1 << (depth - 8)) as f64) / (219.0 * (1 << (depth - 8)) as f64)
        };
        return [g, g, g];
    }
    let (sx, sy) = frame.format.chroma.shift();
    let cb = frame.sample(1, x >> sx, y >> sy) as f64;
    let cr = frame.sample(2, x >> sx, y >> sy) as f64;
    let mid = (1u32 << (depth - 1)) as f64;
    let (yn, cbn, crn) = if full {
        (yv / max, (cb - mid) / max, (cr - mid) / max)
    } else {
        let s = (1 << (depth - 8)) as f64;
        (
            (yv - 16.0 * s) / (219.0 * s),
            (cb - 128.0 * s) / (224.0 * s),
            (cr - 128.0 * s) / (224.0 * s),
        )
    };
    let (kr, kb) = match matrix {
        1 => (0.2126, 0.0722),
        9 => (0.2627, 0.0593),
        _ => (0.299, 0.114),
    };
    let r = yn + 2.0 * (1.0 - kr) * crn;
    let b = yn + 2.0 * (1.0 - kb) * cbn;
    let g = (yn - kr * r - kb * b) / (1.0 - kr - kb);
    [r, g, b]
}

struct Diff {
    max: f64,
    mean: f64,
    pixels: u64,
}

/// Compare a decoded frame with a PNG oracle, in the oracle's sample
/// scale. Returns per-channel statistics over colour (and alpha when
/// both carry one).
fn compare(frame: &HeifFrame, nclx: &Colr, png: &Png) -> Diff {
    assert_eq!(
        (png.width, png.height),
        (frame.width, frame.height),
        "geometry"
    );
    let scale = ((1u32 << png.bit_depth) - 1) as f64;
    let mut max: f64 = 0.0;
    let mut sum = 0.0;
    let mut n = 0u64;
    for y in 0..frame.height {
        for x in 0..frame.width {
            let rgb = to_rgb(frame, nclx, x, y);
            let expect: Vec<f64> = (0..png.channels.min(3).max(1))
                .map(|c| {
                    let c = if png.channels < 3 { 0 } else { c };
                    png.sample(x, y, c) as f64
                })
                .collect();
            for (c, e) in expect.iter().enumerate() {
                let got = (rgb[c].clamp(0.0, 1.0) * scale).round();
                let d = (got - e).abs();
                max = max.max(d);
                sum += d;
                n += 1;
            }
            if let (Some(ap), true) = (
                frame.format.alpha_plane(),
                png.channels == 2 || png.channels == 4,
            ) {
                let a = frame.sample(ap, x, y) as f64
                    / ((1u32 << frame.format.bit_depth) - 1) as f64
                    * scale;
                let e = png.sample(x, y, png.channels - 1) as f64;
                let d = (a.round() - e).abs();
                max = max.max(d);
                sum += d;
                n += 1;
            }
        }
    }
    Diff {
        max,
        mean: sum / n as f64,
        pixels: n,
    }
}

fn oracle(root: &std::path::Path, bundle: &str, name: &str) -> Png {
    read_png(&std::fs::read(root.join(bundle).join(name)).unwrap())
}

/// Tolerances in oracle sample units (8-bit unless the oracle is
/// 16-bit): the oracle's YCbCr→RGB rounding and chroma upsampling
/// differ from this test's nearest-sited conversion, so a small mean
/// and a bounded maximum at chroma edges are expected; anything larger
/// means a composition error (a misplaced tile, a wrong offset, a
/// missing crop, a swapped alpha) rather than colour-math noise.
const MEAN_TOL_8: f64 = 1.5;
const MAX_TOL_8: f64 = 12.0;

/// Bundles whose oracle this crate reproduces sample-exact through the
/// RGB conversion (no chroma upsampling ambiguity: 4:4:4, monochrome,
/// the 1×1 aperture, the alpha-attached and grid bundles).
const EXACT: &[&str] = &[
    "single-image-1x1",
    "still-image-with-alpha",
    "still-image-grid-2x2",
    "still-monochrome",
    "still-yuv444",
];

#[test]
fn every_bundle_matches_its_png_oracle() {
    let Some(root) = fixture_root() else {
        return;
    };
    let mut report = Vec::new();
    for bundle in BUNDLES {
        let f = HeifFile::parse(&fixture_bytes(&root, bundle)).unwrap();
        let img =
            decode_primary(&f, ItemDecoder::direct()).unwrap_or_else(|e| panic!("{bundle}: {e}"));
        let png = oracle(&root, bundle, "expected.png");
        let d = compare(&img.frame, &img.nclx, &png);
        let scale = if png.bit_depth == 16 { 257.0 } else { 1.0 };
        report.push(format!(
            "{bundle}: {}x{} {:?} mean {:.3} max {:.1} over {} samples",
            img.width(),
            img.height(),
            img.frame.format,
            d.mean / scale,
            d.max / scale,
            d.pixels
        ));
        assert!(
            d.mean / scale <= MEAN_TOL_8 && d.max / scale <= MAX_TOL_8,
            "{bundle}: mean {:.3} max {:.1} (8-bit units) — {}",
            d.mean / scale,
            d.max / scale,
            report.last().unwrap()
        );
        if EXACT.contains(bundle) {
            assert_eq!(d.max, 0.0, "{bundle}: expected sample-exact agreement");
        }
        // Alpha bundles: the oracle PNG carries an alpha channel and so
        // must the decoded frame.
        if *bundle == "still-image-with-alpha" {
            assert!(img.frame.format.has_alpha, "{bundle}: alpha attached");
            assert_eq!(png.channels, 4);
        }
    }
    for l in &report {
        eprintln!("{l}");
    }
}

#[test]
fn burst_and_sequence_stills_match_their_per_item_oracles() {
    let Some(root) = fixture_root() else {
        return;
    };
    let f = HeifFile::parse(&fixture_bytes(&root, "multi-image-burst-3")).unwrap();
    let meta = f.meta().unwrap();
    let ids: Vec<u32> = meta
        .items
        .iter()
        .filter(|i| i.is_coded_image())
        .map(|i| i.id)
        .collect();
    assert_eq!(ids.len(), 3);
    for (i, id) in ids.iter().enumerate() {
        let img = decode_item(&f, *id, ItemDecoder::direct()).unwrap();
        let png = oracle(&root, "multi-image-burst-3", &format!("expected_{i}.png"));
        let d = compare(&img.frame, &img.nclx, &png);
        assert!(
            d.mean <= MEAN_TOL_8 && d.max <= MAX_TOL_8,
            "burst item {id}: mean {:.3} max {:.1}",
            d.mean,
            d.max
        );
    }
}

#[test]
fn thumbnails_and_metadata_are_surfaced() {
    let Some(root) = fixture_root() else {
        return;
    };
    let f = HeifFile::parse(&fixture_bytes(&root, "single-image-with-thumbnail")).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.thumbnail_ids.len(), 1);
    let thumb = decode_item(&f, img.thumbnail_ids[0], ItemDecoder::direct()).unwrap();
    assert_eq!((thumb.width(), thumb.height()), (96, 96));

    let f = HeifFile::parse(&fixture_bytes(&root, "still-image-with-exif")).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    let exif = img.exif.expect("Exif surfaced");
    assert!(exif.starts_with(b"II") || exif.starts_with(b"MM"));
    assert!(exif.windows(16).any(|w| w == b"OxideAV-test-fix"));

    let f = HeifFile::parse(&fixture_bytes(&root, "still-image-with-xmp")).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert!(img.xmp.as_deref().unwrap().contains("OxideAV HEIF test"));

    let f = HeifFile::parse(&fixture_bytes(&root, "still-image-with-icc")).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.icc_profile.as_ref().map(Vec::len), Some(2576));
    assert!(!img.nclx_explicit, "ICC bundle carries only a prof colr");
    assert_eq!(img.nclx, Colr::MIAF_DEFAULT);

    let f = HeifFile::parse(&fixture_bytes(&root, "still-image-overlay")).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert!(
        !img.nclx_explicit,
        "overlay bundle has no colr: MIAF default"
    );
    assert_eq!(img.nclx, Colr::MIAF_DEFAULT);
    assert!(
        !img.frame.format.has_alpha,
        "opaque canvas → no output alpha"
    );
}

#[test]
fn grid_composition_matches_black_box_decoder_byte_exact() {
    let Some(root) = fixture_root() else {
        return;
    };
    let out = std::env::temp_dir().join(format!("oxideav-heif-grid-{}.raw", std::process::id()));
    let status = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(root.join("still-image-grid-2x2").join("input.heic"))
        .args(["-f", "rawvideo"])
        .arg(&out)
        .status();
    let Ok(st) = status else {
        eprintln!("ffmpeg not installed; skipping");
        return;
    };
    if !st.success() {
        eprintln!("black-box decoder refused the grid; skipping");
        return;
    }
    let raw = std::fs::read(&out).unwrap();
    let _ = std::fs::remove_file(&out);
    let f = HeifFile::parse(&fixture_bytes(&root, "still-image-grid-2x2")).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!((img.width(), img.height()), (256, 256));
    let mut ours = Vec::new();
    for p in &img.frame.tight().planes {
        ours.extend_from_slice(&p.data);
    }
    assert_eq!(ours.len(), raw.len());
    assert_eq!(
        ours.iter().zip(&raw).filter(|(a, b)| a != b).count(),
        0,
        "grid bytes differ"
    );
}
