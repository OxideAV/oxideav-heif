//! Predictively coded image items (HEIF §6.4.9, the `pred` brand): the
//! Nokia conformance files C043 (an IRAP primary + two `TRAIL_N` items
//! predicted from it) and C044 (a predicted item as the primary), built
//! from the elementary stream `conformance/nokia/bitstreams/B010.265`.
//! Each item decodes from the bitstream its `pred` references form with
//! its own data and is byte-exact against the black-box decoder's
//! output picture of the same access unit of B010 (re-derived when
//! `ffmpeg` runs, else the pinned FNV-1a 64 values taken from it).
//! Skips without the umbrella's `docs/`.
#![cfg(feature = "registry")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_heif::HeifFile;
use oxideav_heif::decode::{ItemDecoder, decode_item, decode_primary};

/// FNV-1a 64 of B010's first three output pictures (`yuv420p`).
const B010: [u64; 3] = [
    0xbd81_bc75_d57f_0c08,
    0x3f6d_19e9_70c1_cd10,
    0x7753_33df_91aa_c2f7,
];

fn conformance() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/image/heif/conformance");
    p.join("heic/C043.heic").is_file().then_some(p)
}

fn oracle(root: &Path) -> Vec<u64> {
    let len = 1280 * 720 * 3 / 2;
    if common::have_binary("ffmpeg") {
        let raw = common::scratch_dir("pred").join("b010.yuv");
        let ok = Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "quiet", "-y", "-i"])
            .arg(root.join("nokia/bitstreams/B010.265"))
            .args(["-frames:v", "3", "-f", "rawvideo", "-pix_fmt", "yuv420p"])
            .arg(&raw)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            let got: Vec<u64> = std::fs::read(&raw)
                .unwrap()
                .chunks(len)
                .map(common::fnv1a64)
                .collect();
            if got != B010 {
                eprintln!("B010: black-box output {got:x?} differs from the pin {B010:x?}");
            }
            return got;
        }
    }
    eprintln!("B010: black-box decoder absent, using the pinned values");
    B010.to_vec()
}

fn fnv(f: &oxideav_heif::HeifFrame) -> u64 {
    let t = f.tight();
    let all: Vec<u8> = t
        .planes
        .iter()
        .flat_map(|p| p.data.iter().copied())
        .collect();
    common::fnv1a64(&all)
}

#[test]
fn predicted_items_decode_byte_exact() {
    let Some(root) = conformance() else {
        eprintln!("SKIP: docs/image/heif/conformance not checked out");
        return;
    };
    let want = oracle(&root);
    let c043 = HeifFile::from_vec(std::fs::read(root.join("heic/C043.heic")).unwrap()).unwrap();
    for (k, id) in [1002u32, 1004, 1006].into_iter().enumerate() {
        let f = decode_item(&c043, id, ItemDecoder::direct()).unwrap().frame;
        assert_eq!(fnv(&f), want[k], "C043 item {id} vs B010 picture {k}");
    }
    // The predicted primary of C044 (pred → 1002, the IRAP item).
    let c044 = HeifFile::from_vec(std::fs::read(root.join("heic/C044.heic")).unwrap()).unwrap();
    let p = decode_primary(&c044, ItemDecoder::direct()).unwrap();
    assert_eq!(p.item_id, 1004);
    assert_eq!(fnv(&p.frame), want[1], "C044 primary vs B010 picture 1");
    // The whole-file contract decodes every item of both files.
    for f in ["C043.heic", "C044.heic"] {
        let bytes = std::fs::read(root.join("heic").join(f)).unwrap();
        let frames = oxideav_heif::decode_all_with(&bytes, &Default::default()).unwrap();
        assert!(!frames.is_empty(), "{f}");
    }
}
