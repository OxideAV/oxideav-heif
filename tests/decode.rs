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

/// Parallel grid-tile decode is byte-identical to the serial decode for
/// every thread budget, on every grid of the vendored corpora (Apple's
/// 512-px tiling included) and through the framework decoder's
/// `set_execution_context`.
#[test]
fn parallel_grid_decode_is_byte_identical_to_serial() {
    use oxideav_core::{Decoder, ExecutionContext, Frame, Packet, TimeBase};
    use oxideav_heif::decode::decode_primary;
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for (root, bundle) in all_bundles() {
        files.push((bundle.to_string(), fixture_bytes(&root, bundle)));
    }
    for e in std::fs::read_dir(common::interop_root()).unwrap() {
        let p = e.unwrap().path();
        if matches!(
            p.extension().and_then(|x| x.to_str()),
            Some("heic") | Some("avif")
        ) {
            files.push((p.display().to_string(), std::fs::read(&p).unwrap()));
        }
    }
    let mut grids = 0;
    for (name, bytes) in &files {
        let f = HeifFile::parse(bytes).unwrap();
        let Ok(primary) = f.primary_item() else {
            continue;
        };
        if primary.item_type != *b"grid" {
            continue;
        }
        grids += 1;
        let serial = decode_primary(&f, ItemDecoder::direct()).unwrap();
        for threads in [2, 3, 8] {
            let dec = ItemDecoder::direct()
                .with_execution_context(&ExecutionContext::with_threads(threads));
            let par = decode_primary(&f, dec).unwrap();
            assert_eq!(par.frame, serial.frame, "{name}: {threads} threads");
        }
        let mut codec = oxideav_heif::HeifCodec::new(oxideav_core::CodecId::new("heif"));
        codec.set_execution_context(&ExecutionContext::with_threads(4));
        codec
            .send_packet(&Packet::new(0, TimeBase::new(1, 1), bytes.clone()))
            .unwrap();
        let Frame::Video(v) = codec.receive_frame().unwrap() else {
            panic!("{name}: video frame");
        };
        let (want, _) = serial.frame.to_core_signalled(&serial.nclx).unwrap();
        assert_eq!(v.planes.len(), want.planes.len(), "{name}");
        for (a, b) in v.planes.iter().zip(&want.planes) {
            assert_eq!(
                (a.stride, &a.data),
                (b.stride, &b.data),
                "{name}: framework decoder"
            );
        }
    }
    assert!(grids >= 2, "grid fixtures present ({grids})");
}

/// The Fuzz workflow's `heif_decode` crash unit (r473): an image
/// sequence whose `hvc1` sample entry declares a 5×0 picture — the
/// output-plane size arithmetic underflowed. Every decode path must
/// return an error (or frames), never panic; the unit is a tracked
/// corpus seed.
#[test]
fn fuzz_unit_zero_height_sample_entry_returns() {
    let bytes = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fuzz/corpus/heif_decode/minimal.track-entry-zero-height.heic"),
    )
    .unwrap();
    let _ = oxideav_heif::decode(&bytes);
    let _ = oxideav_heif::decode_all(&bytes);
    let _ = oxideav_heif::decode_rgba8(&bytes);
    let _ = oxideav_heif::info(&bytes);
}
