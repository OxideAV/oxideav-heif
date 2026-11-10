//! Common Encryption of image items (ISO/IEC 23001-7 Amd 1 §8.3 / §8.4)
//! over the 13 staged encrypted conformance files
//! (`docs/image/heif/conformance/cenc/`): `cenc` (64- / 128-bit IVs,
//! several keys per item), `cbc1`, `cens` (patterns, 64- / 128-bit IVs)
//! and `cbcs` (per-item and constant IVs, several keys). The keys are
//! the clear-text test keys of `cenc/drm_cfg/*.xml`; AES-128 itself is
//! the `openssl` binary driven as a black box (`-aes-128-ecb -nopad`,
//! one call per key and direction) behind this crate's `BlockCipher`
//! hook. Every file decrypts and decodes, and all 13 reconstruct the
//! same picture (they protect one coded image), which the black-box
//! video decoder reproduces byte-exact from the decrypted item.
//! Without a cipher the item is the typed `encrypted_item` refusal.
//! Skips without the umbrella's `docs/` or without `openssl`.
#![cfg(feature = "registry")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::cipher::{OpenSsl, keys};
use oxideav_heif::HeifFile;
use oxideav_heif::decode::{ItemDecoder, decode_primary};

fn cenc_dir() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/image/heif/conformance/cenc");
    p.join("image_cenc_64iv.heic").is_file().then_some(p)
}

fn planar(f: &oxideav_heif::HeifFrame) -> Vec<u8> {
    let t = f.tight();
    t.planes
        .iter()
        .flat_map(|p| p.data.iter().copied())
        .collect()
}

/// The files the Amd 1 syntax decrypts as written; the others depart
/// from it (see `CencReading`) and need the compat reading.
const STRICT_OK: [&str; 4] = [
    "image_cbc1",
    "image_cenc_128iv",
    "image_cenc_64iv",
    "image_cenc_roll",
];

#[test]
fn encrypted_items_decrypt_and_decode() {
    use oxideav_heif::cenc::CencReading;
    let Some(dir) = cenc_dir() else {
        eprintln!("SKIP: docs/image/heif/conformance/cenc not checked out");
        return;
    };
    if !common::have_binary("openssl") {
        eprintln!("SKIP: openssl (the black-box AES-128) is absent");
        return;
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "heic"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 13);
    let mut pictures: Vec<(String, Vec<u8>)> = Vec::new();
    for path in &files {
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let cfg = dir
            .join("drm_cfg")
            .join(format!("{}.xml", name.trim_start_matches("image_")));
        let cipher = OpenSsl(keys(&std::fs::read_to_string(&cfg).unwrap()));
        let file = HeifFile::from_vec(std::fs::read(path).unwrap()).unwrap();
        // Without a cipher: the typed refusal naming the scheme.
        let e = decode_primary(&file, ItemDecoder::direct()).unwrap_err();
        let (id, scheme) = e
            .encrypted_item_info()
            .unwrap_or_else(|| panic!("{name}: {e}"));
        assert_eq!(id, 1, "{name}");
        assert!(name.contains(&scheme), "{name}: scheme {scheme}");
        // The Amd 1 syntax.
        let strict = decode_primary(&file, ItemDecoder::direct().with_decryptor(&cipher));
        assert_eq!(
            strict.is_ok(),
            STRICT_OK.contains(&name.as_str()),
            "{name}: strict reading {:?}",
            strict.as_ref().err()
        );
        // The compat reading decrypts every file.
        let img = decode_primary(
            &file,
            ItemDecoder::direct()
                .with_decryptor(&cipher)
                .with_cenc_reading(CencReading::compat()),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        if let Ok(s) = strict {
            assert_eq!(s.frame, img.frame, "{name}: strict vs compat");
        }
        pictures.push((name, planar(&img.frame)));
    }
    for (name, p) in &pictures[1..] {
        assert!(p == &pictures[0].1, "{name} differs from {}", pictures[0].0);
    }
    let fnv = common::fnv1a64(&pictures[0].1);
    eprintln!(
        "13 encrypted items decrypt to one picture ({} bytes, FNV {fnv:016x})",
        pictures[0].1.len()
    );
    // The black-box decoder on the decrypted item (public cenc API).
    if common::have_binary("ffmpeg") {
        let path = dir.join("image_cenc_64iv.heic");
        let cipher = OpenSsl(keys(
            &std::fs::read_to_string(dir.join("drm_cfg/cenc_64iv.xml")).unwrap(),
        ));
        let file = HeifFile::from_vec(std::fs::read(&path).unwrap()).unwrap();
        let meta = file.meta().unwrap();
        let scheme = meta.protection_scheme(1).unwrap().scheme_type.unwrap();
        let props = oxideav_heif::props::ItemProperties::resolve(meta, 1).unwrap();
        let ienc = props.ienc().unwrap();
        let aux_id = meta.references_from(1, b"auxr")[0];
        let aux = oxideav_heif::cenc::CencAuxData::parse(&file.item_data(aux_id).unwrap(), ienc, 0)
            .unwrap();
        let clear =
            oxideav_heif::cenc::decrypt(scheme, ienc, &aux, &file.item_data(1).unwrap(), &cipher)
                .unwrap();
        let hvcc = props.hvcc().unwrap();
        let mut annex_b = Vec::new();
        let units = hvcc
            .arrays
            .iter()
            .flat_map(|a| a.nal_units.iter().map(Vec::as_slice))
            .chain(oxideav_heif::hvcc::split_length_prefixed(&clear, hvcc.length_size).unwrap());
        for n in units {
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(n);
        }
        let scratch = common::scratch_dir("cenc");
        let (src, raw) = (scratch.join("item.265"), scratch.join("item.yuv"));
        std::fs::write(&src, &annex_b).unwrap();
        let ok = Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "quiet", "-y", "-f", "hevc", "-i"])
            .arg(&src)
            .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
            .arg(&raw)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "the black-box decoder refused the decrypted item");
        assert_eq!(
            common::fnv1a64(&std::fs::read(&raw).unwrap()),
            fnv,
            "decrypted item vs the black-box decoder"
        );
    }
}
