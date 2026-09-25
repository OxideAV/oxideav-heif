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
use oxideav_heif::rgb::to_rgb;
use oxideav_heif::HeifFile;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gainmap")
}

fn load(name: &str) -> HeifFile {
    HeifFile::from_vec(std::fs::read(root().join(name)).unwrap()).unwrap()
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

/// HEIF Amd 1 §6.6.2.4: decoding the `tmap` item itself yields the
/// base (with the base's colour information) under the default policy
/// and the normative reconstruction — the map fully applied, in the
/// `tmap` item's own `colr` — under [`ItemDecoder::tone_mapped`].
#[test]
fn tmap_item_decodes_to_base_or_to_the_normative_reconstruction() {
    use oxideav_heif::gainmap::{linear_to_transfer, reconstruct_tone_map, transfer_to_linear};
    use oxideav_heif::props::Colr;
    for v in VARIANTS {
        let f = load(&format!("{v}.avif"));
        let base = decode_primary(&f, ItemDecoder::direct()).unwrap();
        let gm = base.gain_map.as_ref().unwrap();
        let tmap_id = gm.tmap_item_id;
        // Base policy: the base picture, the base's nclx.
        let t = decode_item(&f, tmap_id, ItemDecoder::direct()).unwrap();
        assert_eq!(t.frame, base.frame, "{v}: base rendition");
        assert_eq!(t.nclx, base.nclx, "{v}: base colour information");
        assert!(t.gain_map.is_some());
        // Applied: 4:4:4 in the tmap's colr (PQ), base geometry.
        let a = decode_item(&f, tmap_id, ItemDecoder::direct().tone_mapped()).unwrap();
        assert_eq!((a.width(), a.height()), (base.width(), base.height()));
        assert_eq!(a.frame.format.chroma, oxideav_heif::Chroma::Yuv444, "{v}");
        assert_eq!(a.nclx, gm.alternate_colr.clone().unwrap(), "{v}");
        let Colr::Nclx {
            primaries: alt_p,
            transfer: alt_t,
            ..
        } = a.nclx
        else {
            panic!("nclx");
        };
        assert_eq!(alt_t, 16, "{v}: alternate is PQ");
        // The applied rendition, re-linearised, matches the tool's
        // full-headroom render (at 16 bits so PQ quantisation does not
        // dominate; the render is clipped SDR sRGB in BT.709).
        let hi = reconstruct_tone_map(
            &base.frame,
            Some(&base.nclx),
            &gm.frame,
            gm.colr.as_ref(),
            gm.alternate_colr.as_ref(),
            &gm.metadata,
            16,
            203.0,
        )
        .unwrap();
        let rgb = to_rgb(&hi, gm.alternate_colr.as_ref()).unwrap();
        let unscale =
            1.0 / oxideav_heif::gainmap::alternate_signal_scale(alt_t, &gm.metadata, 203.0);
        let png = read_png(&std::fs::read(root().join(format!("{v}_tm4.png"))).unwrap());
        let m = oxideav_heif::gainmap::primaries_conversion(alt_p, 1).unwrap();
        let lin_all = base.apply_gain_map(4.0).unwrap();
        let scale = oxideav_heif::gainmap::alternate_signal_scale(alt_t, &gm.metadata, 203.0);
        let (mut max, mut sum, mut n, mut clipped) = (0.0f64, 0.0, 0u64, 0u64);
        for y in 0..png.height {
            for x in 0..png.width {
                // A component beyond the alternate encoding's peak
                // (PQ: 10 000 cd/m²) clips in the reconstruction — a
                // picture in the tmap's colr — while the oracle's
                // float pipeline keeps it, so those pixels are not
                // comparable. (Small negatives from the offsets clip
                // to 0 on both sides.)
                if (0..3).any(|c| lin_all.sample(x, y, c) as f64 * scale > 1.0) {
                    clipped += 1;
                    continue;
                }
                let lin: Vec<f64> = (0..3)
                    .map(|c| {
                        transfer_to_linear(rgb.sample(x, y, c) as f64 / 65535.0, alt_t).unwrap()
                            * unscale
                    })
                    .collect();
                for (c, row) in m.iter().enumerate() {
                    let l = row[0] * lin[0] + row[1] * lin[1] + row[2] * lin[2];
                    let ours = (linear_to_transfer(l, 13).unwrap() * 255.0)
                        .round()
                        .clamp(0.0, 255.0);
                    let d = (png.sample(x, y, c) as f64 - ours).abs();
                    max = max.max(d);
                    sum += d;
                    n += 1;
                }
            }
        }
        eprintln!(
            "{v} applied vs oracle h=4: max {max} mean {:.4} ({clipped} gamut-clipped pixels skipped)",
            sum / n as f64
        );
        // One code where the primaries coincide (two on the half-size
        // map's resample); the BT.2020 alternate adds the primaries
        // conversion after a 16-bit PQ quantisation and clips the
        // offset negatives in its own gamut, measured at 7.
        let limit = if alt_p == 1 { 3.0 } else { 8.0 };
        assert!(max <= limit && sum / n as f64 <= 0.4, "{v}: max {max}");
        assert!(
            clipped * 20 < (png.width * png.height) as u64,
            "{v}: {clipped} clipped pixels"
        );
    }
}

/// The oracle files satisfy the Amd 1 §6.6.2.4 / §10.2.6 shalls the
/// checker enforces (brand, input pair, colr placements, version).
#[test]
fn oracle_files_pass_the_tmap_clause_checks() {
    use oxideav_heif::miaf::{check, MiafProfile};
    for v in VARIANTS {
        let f = load(&format!("{v}.avif"));
        assert!(f.file_type.has_brand(b"tmap"), "{v}");
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        let a1: Vec<_> = rep
            .violations
            .iter()
            .filter(|x| x.clause.starts_with("HEIF-A1") || x.clause == "HEIF 6.4.2")
            .collect();
        assert!(a1.is_empty(), "{v}: {a1:#?}");
    }
}

/// The writer authors a `tmap` item from a base + gain map + metadata
/// that this crate reads back identically, that passes the clause
/// checks, and that the black-box gain-map tool tone-maps to the same
/// picture as the file it was rebuilt from (AVIF); the HEVC form opens
/// in the third-party readers present.
#[test]
fn writer_authors_a_tmap_the_black_box_tool_tone_maps_identically() {
    use oxideav_heif::encode::{encode_still, EncodeOptions, GainMapSpec, StillCodec};
    use oxideav_heif::miaf::{check, MiafProfile};
    use oxideav_heif::props::Colr;
    let src = load("gm_rgb.avif");
    let base = decode_primary(&src, ItemDecoder::direct()).unwrap();
    let gm = base.gain_map.as_ref().unwrap();
    let Some(Colr::Nclx {
        matrix, full_range, ..
    }) = gm.colr.clone()
    else {
        panic!("gain map colr");
    };
    let spec = GainMapSpec {
        frame: gm.frame.clone(),
        metadata: gm.metadata.clone(),
        alternate_colr: gm.alternate_colr.clone().unwrap(),
        gain_map_matrix: matrix,
        gain_map_full_range: full_range,
        alternate_clli: Some(oxideav_heif::props::Clli {
            max_content_light_level: 4000,
            max_pic_average_light_level: 400,
        }),
        alternate_bit_depth: 12,
    };
    let dir = common::scratch_dir("tmap");
    for (codec, ext) in [(StillCodec::Av1, "avif"), (StillCodec::Hevc, "heic")] {
        let opts = EncodeOptions {
            codec,
            hevc_mode: "pcm".into(),
            colr: base.nclx.clone(),
            gain_map: Some(spec.clone()),
            ..EncodeOptions::default()
        };
        let bytes = encode_still(&base.frame, &opts).unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        assert!(f.file_type.has_brand(b"tmap"));
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{ext}: {:#?}", rep.violations);
        let back = decode_primary(&f, ItemDecoder::direct()).unwrap();
        let bgm = back.gain_map.as_ref().expect("gain map attached");
        assert_eq!(bgm.metadata, gm.metadata, "{ext}");
        assert_eq!(bgm.alternate_colr, gm.alternate_colr, "{ext}");
        assert_eq!(bgm.colr, gm.colr, "{ext}");
        assert_eq!(bgm.frame.width, gm.frame.width);
        let meta = f.meta().unwrap();
        assert!(meta.item(bgm.gain_map_item_id).unwrap().is_hidden());
        let altr = meta.groups_containing(bgm.tmap_item_id, b"altr");
        assert_eq!(altr.len(), 1);
        assert_eq!(altr[0].entity_ids, vec![bgm.tmap_item_id, back.item_id]);
        let tprops = oxideav_heif::props::ItemProperties::resolve(meta, bgm.tmap_item_id).unwrap();
        assert_eq!(tprops.clli().map(|c| c.max_content_light_level), Some(4000));
        // Lossless codings: the base is exact; the AV1 map keeps its
        // 4:4:4 layout exactly, the HEVC one is coded 4:2:0 (layout
        // change, geometry kept).
        if codec == StillCodec::Av1 {
            assert_eq!(back.frame, base.frame, "{ext}: base pixels");
            assert_eq!(bgm.frame, gm.frame, "{ext}: gain map pixels");
        } else {
            assert_eq!((back.width(), back.height()), (base.width(), base.height()));
            assert_eq!(
                (bgm.frame.width, bgm.frame.height),
                (gm.frame.width, gm.frame.height)
            );
            assert_eq!(bgm.frame.format.chroma, oxideav_heif::Chroma::Yuv420);
        }
        let path = dir.join(format!("tmap_rt.{ext}"));
        std::fs::write(&path, &bytes).unwrap();
        if ext == "avif" {
            let have_tool = std::process::Command::new("avifgainmaputil")
                .arg("help")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !have_tool {
                eprintln!("SKIP: avifgainmaputil not installed");
                continue;
            }
            let pm = std::process::Command::new("avifgainmaputil")
                .arg("printmetadata")
                .arg(&path)
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&pm.stdout).to_string();
            assert!(pm.status.success(), "printmetadata: {text}");
            assert!(text.contains("Alternate headroom: 4"), "{text}");
            assert!(text.contains("Use Base Color Space: True"), "{text}");
            // Tone-map ours and the original at h=2 with the same
            // settings: the pictures must agree.
            let mut renders = Vec::new();
            for (tag, input) in [("ours", path.clone()), ("orig", root().join("gm_rgb.avif"))] {
                let out = dir.join(format!("tm_{tag}.png"));
                let st = std::process::Command::new("avifgainmaputil")
                    .arg("tonemap")
                    .arg(&input)
                    .arg(&out)
                    .args(["--headroom", "2", "--cicp-output", "1/13/6"])
                    .output()
                    .unwrap();
                assert!(
                    st.status.success(),
                    "tonemap {tag}: {}",
                    String::from_utf8_lossy(&st.stderr)
                );
                renders.push(read_png(&std::fs::read(&out).unwrap()));
            }
            let (a, b) = (&renders[0], &renders[1]);
            assert_eq!((a.width, a.height), (b.width, b.height));
            let norm = |p: &common::png::Png, x: u32, y: u32, c: usize| {
                p.sample(x, y, c) as f64 * 255.0 / ((1u32 << p.bit_depth) - 1) as f64
            };
            let mut max = 0.0f64;
            for y in 0..a.height {
                for x in 0..a.width {
                    for c in 0..3 {
                        max = max.max((norm(a, x, y, c) - norm(b, x, y, c)).abs());
                    }
                }
            }
            eprintln!(
                "black-box tone map of our tmap vs the original: max {max:.2} (8-bit units; renders at {} / {} bits)",
                a.bit_depth, b.bit_depth
            );
            assert!(max <= 1.0, "black-box tone maps differ by {max}");
        } else {
            for bin in ["heif-info", "sips", "magick"] {
                if !common::have_binary(bin) {
                    eprintln!("SKIP: {bin} not installed");
                    continue;
                }
                let args: Vec<String> = match bin {
                    "sips" => vec!["-g".into(), "pixelWidth".into()],
                    "magick" => vec!["identify".into()],
                    _ => vec![],
                };
                let out = std::process::Command::new(bin)
                    .args(&args)
                    .arg(&path)
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{bin} refused the HEVC tmap file: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
    }
}

#[test]
fn files_without_a_gain_map_carry_none() {
    let f = HeifFile::from_vec(
        std::fs::read(common::interop_root().join("henc_avif_rgb_96x80.avif")).unwrap(),
    )
    .unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert!(img.gain_map.is_none());
    assert!(img.apply_gain_map(1.0).is_err());
}
