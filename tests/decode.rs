//! Coded-item decode through the codec crates (`registry` feature),
//! cross-checked byte-exact against a black-box decoder (`ffmpeg`
//! rawvideo output of the primary item) when one is installed.
#![cfg(feature = "registry")]

mod common;

use std::path::Path;
use std::process::Command;

use common::{all_bundles, fixture_bytes, fixture_root};
use oxideav_heif::decode::ItemDecoder;
use oxideav_heif::derived::build_primary_graph;
use oxideav_heif::image::Chroma;
use oxideav_heif::HeifFile;

/// Decode the primary coded item of `bundle` (no composition).
fn decode_primary_coded(root: &Path, bundle: &str) -> oxideav_heif::HeifFrame {
    let f = HeifFile::from_vec(fixture_bytes(root, bundle)).unwrap();
    let node = build_primary_graph(&f).unwrap();
    ItemDecoder::direct().decode_coded(&f, &node).unwrap()
}

/// Black-box oracle: the primary item decoded by `ffmpeg` to raw
/// planar samples in its native layout. `None` when ffmpeg is absent
/// or refuses the file.
fn ffmpeg_raw(root: &Path, bundle: &str, out: &Path) -> Option<Vec<u8>> {
    let input = root.join(bundle).join("input.heic");
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(&input)
        .args(["-f", "rawvideo"])
        .arg(out)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    std::fs::read(out).ok()
}

const PLAIN_CODED: &[&str] = &[
    "single-image-512x512-q60",
    "single-image-with-thumbnail",
    "still-image-with-icc",
    "still-image-with-exif",
    "still-image-with-xmp",
    "multi-image-burst-3",
    "still-monochrome",
    "still-10bit-main10",
    "still-yuv444",
];

#[test]
fn every_coded_item_decodes_to_its_announced_layout() {
    for (root, bundle) in all_bundles() {
        let f = HeifFile::from_vec(fixture_bytes(&root, bundle)).unwrap();
        let node = build_primary_graph(&f).unwrap();
        for coded in node.coded_items() {
            let frame = ItemDecoder::direct().decode_coded(&f, coded).unwrap();
            let hv = coded.properties.hvcc().unwrap();
            assert_eq!(
                frame.format.bit_depth,
                hv.bit_depth_luma(),
                "{bundle} item {}",
                coded.item.id
            );
            assert_eq!(frame.format.chroma.idc(), hv.chroma_format_idc, "{bundle}");
            assert_eq!(Some((frame.width, frame.height)), coded.ispe(), "{bundle}");
            frame.validate().unwrap();
            let params = ItemDecoder::codec_parameters(coded).unwrap();
            assert_eq!(params.codec_id.as_str(), "h265");
            assert_eq!(params.extradata, hv.raw);
            assert!(params.pixel_format.is_some());
        }
        // Thumbnails and alpha auxiliaries decode too.
        for t in &node.thumbnails {
            let frame = ItemDecoder::direct().decode_coded(&f, t).unwrap();
            assert_eq!(Some((frame.width, frame.height)), t.ispe());
        }
        if let Some(a) = &node.alpha {
            let frame = ItemDecoder::direct().decode_coded(&f, a).unwrap();
            assert_eq!(
                frame.format.chroma,
                Chroma::Mono,
                "{bundle}: alpha is 4:0:0"
            );
        }
    }
}

#[test]
fn coded_primaries_match_black_box_decoder_byte_exact() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("ffmpeg not installed; skipping black-box cross-check");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("oxideav-heif-decode-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut checked = 0;
    for (root, bundle) in all_bundles()
        .into_iter()
        .filter(|(_, b)| PLAIN_CODED.contains(b))
    {
        let frame = decode_primary_coded(&root, bundle).tight();
        let Some(raw) = ffmpeg_raw(&root, bundle, &tmp.join(format!("{bundle}.raw"))) else {
            eprintln!("{bundle}: black-box decoder refused the file; skipping");
            continue;
        };
        let mut ours = Vec::new();
        for p in &frame.planes {
            ours.extend_from_slice(&p.data);
        }
        assert_eq!(ours.len(), raw.len(), "{bundle}: plane byte count");
        let mismatches = ours.iter().zip(&raw).filter(|(a, b)| a != b).count();
        assert_eq!(
            mismatches,
            0,
            "{bundle}: {mismatches} of {} bytes differ",
            raw.len()
        );
        checked += 1;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(checked >= 1, "no bundle was cross-checked");
}

#[test]
fn registry_path_decodes_through_a_codec_registry() {
    let Some(root) = fixture_root() else {
        return;
    };
    let mut reg = oxideav_core::CodecRegistry::new();
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    std::mem::swap(&mut reg, &mut ctx.codecs);
    let f = HeifFile::from_vec(fixture_bytes(&root, "still-yuv444")).unwrap();
    let node = build_primary_graph(&f).unwrap();
    let via_registry = ItemDecoder::with_registry(&reg)
        .decode_coded(&f, &node)
        .unwrap();
    let direct = ItemDecoder::direct().decode_coded(&f, &node).unwrap();
    assert_eq!(via_registry, direct);
    // An empty registry cannot decode.
    let empty = oxideav_core::CodecRegistry::new();
    assert!(ItemDecoder::with_registry(&empty)
        .decode_coded(&f, &node)
        .is_err());
}
