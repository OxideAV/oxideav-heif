//! ISO 21496-1 gain maps on real producer files (`tests/fixtures/gainmap/`,
//! written by a black-box AVIF gain-map tool from a base / alternate
//! pair: monochrome, RGB, half-size and BT.2020-application-space gain
//! maps) against that tool's own tone-mapped renditions at several HDR
//! headrooms. Everything runs unconditionally.
#![cfg(feature = "registry")]

mod common;

use std::path::PathBuf;

use common::png::read_png;
use oxideav_heif::decode::{decode_item, decode_primary, ItemDecoder};
use oxideav_heif::gainmap::Rational;
use oxideav_heif::HeifFile;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gainmap")
}

fn load(name: &str) -> HeifFile {
    HeifFile::parse(&std::fs::read(root().join(name)).unwrap()).unwrap()
}

const VARIANTS: &[&str] = &["gm_mono", "gm_rgb", "gm_half", "gm_2020"];

#[test]
fn tmap_metadata_matches_the_producer_report() {
    // Values the producer prints for its own file (fractions).
    let f = load("gm_mono.avif");
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    let gm = img
        .gain_map
        .as_ref()
        .expect("gain map attached to the primary");
    let m = &gm.metadata;
    assert_eq!((m.minimum_version, m.writer_version), (0, 0));
    assert!(!m.is_multichannel);
    assert!(m.use_base_colour_space);
    assert_eq!(m.base_hdr_headroom, Rational { num: 0, den: 1 });
    assert_eq!(m.alternate_hdr_headroom, Rational { num: 4, den: 1 });
    let c = m.channel(0);
    assert_eq!(
        c.gain_map_min,
        Rational {
            num: -249497,
            den: 131072
        }
    );
    assert_eq!(
        c.gain_map_max,
        Rational {
            num: 14163867,
            den: 2097152
        }
    );
    assert_eq!(c.base_offset, Rational { num: 1, den: 64 });
    assert_eq!(c.alternate_offset, Rational { num: 1, den: 64 });
    assert_eq!(c.gamma, Rational { num: 1, den: 1 });
    assert_eq!(gm.frame.format.chroma, oxideav_heif::Chroma::Mono);
    assert_eq!((gm.frame.width, gm.frame.height), (96, 80));
    // The base rendition is untouched by default.
    assert_eq!((img.width(), img.height()), (96, 80));
    assert_eq!(gm.tmap_item_id, 2);
    assert_eq!(gm.gain_map_item_id, 3);
    // The 2020 variant applies in the alternate's primaries.
    let f = load("gm_2020.avif");
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    let gm = img.gain_map.as_ref().unwrap();
    assert!(!gm.metadata.use_base_colour_space);
    assert!(matches!(
        gm.alternate_colr,
        Some(oxideav_heif::props::Colr::Nclx {
            primaries: 9,
            transfer: 16,
            ..
        })
    ));
    let out = img.apply_gain_map(4.0).unwrap();
    assert_eq!(out.primaries, 9);
    // The RGB variant carries a three-channel gain map.
    let f = load("gm_rgb.avif");
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(
        img.gain_map.as_ref().unwrap().frame.format.chroma,
        oxideav_heif::Chroma::Yuv444
    );
    // Decoding the tmap item itself yields the base with the same attachment.
    let t = decode_item(&f, 2, ItemDecoder::direct()).unwrap();
    assert_eq!(t.frame, img.frame);
    assert_eq!(t.gain_map.as_ref().unwrap().gain_map_item_id, 3);
}

/// Compare our application (encoded back to 8-bit sRGB in BT.709,
/// clipping the HDR headroom like the oracle's SDR output) with the
/// producer's tone-mapped PNGs. `(max, mean)` in 8-bit units.
fn diff_against_oracle(variant: &str, headroom: f64) -> (f64, f64) {
    let f = load(&format!("{variant}.avif"));
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    let lin = img.apply_gain_map(headroom).unwrap();
    let ours = lin.encode(1, 13, 8).unwrap();
    let png = read_png(
        &std::fs::read(root().join(format!("{variant}_tm{}.png", headroom as u32))).unwrap(),
    );
    assert_eq!((png.width, png.height), (ours.width, ours.height));
    let (mut max, mut sum, mut n) = (0.0f64, 0.0, 0u64);
    for y in 0..png.height {
        for x in 0..png.width {
            for c in 0..3 {
                let d = (png.sample(x, y, c) as f64 - ours.sample(x, y, c) as f64).abs();
                max = max.max(d);
                sum += d;
                n += 1;
            }
        }
    }
    (max, sum / n as f64)
}

#[test]
fn base_headroom_reproduces_the_base_and_full_headroom_matches_the_oracle() {
    for v in VARIANTS {
        // W = 0: the linearised base re-encoded is the base (the
        // oracle's headroom-0 render is the base image itself).
        let (max0, mean0) = diff_against_oracle(v, 0.0);
        eprintln!("{v} h=0: max {max0} mean {mean0:.4}");
        assert!(
            max0 <= 1.0 && mean0 <= 0.05,
            "{v} h=0: max {max0} mean {mean0}"
        );
        // Half and full application: the oracle's float pipeline vs
        // this crate's per-sample double precision — one code (two on
        // the half-size map, where the resampling phase differs).
        for h in [2.0, 4.0] {
            let (max, mean) = diff_against_oracle(v, h);
            eprintln!("{v} h={h}: max {max} mean {mean:.4}");
            assert!(
                max <= 2.0 && mean <= 0.3,
                "{v} h={h}: max {max} mean {mean}"
            );
        }
    }
}

#[test]
fn files_without_a_gain_map_carry_none() {
    let f = HeifFile::parse(
        &std::fs::read(common::interop_root().join("henc_avif_rgb_96x80.avif")).unwrap(),
    )
    .unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert!(img.gain_map.is_none());
    assert!(img.apply_gain_map(1.0).is_err());
}
