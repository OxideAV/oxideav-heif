//! Every image item of every HEIF-family file this checkout can see —
//! the vendored fixtures (always) and, when the umbrella's `docs/` is
//! checked out, the staged corpus bundles, the device samples and the
//! MPEG / Nokia conformance set — decoded on its own with
//! `decode_item`: not only what a viewer shows (`decode_all`), but
//! each grid tile, thumbnail, auxiliary and hidden item as the image
//! item it is.
//!
//! An item either decodes or is refused with a typed error; the
//! refusals are counted by what gates them, and no gate outside the
//! known ones may appear.
#![cfg(feature = "registry")]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::cipher::{OpenSsl, keys};
use oxideav_heif::HeifFile;
use oxideav_heif::cenc::CencReading;
use oxideav_heif::decode::{ItemDecoder, decode_item};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|e| {
            matches!(
                e.to_string_lossy().to_ascii_lowercase().as_str(),
                "heic" | "heif" | "heics" | "heifs" | "hif" | "avif" | "avifs"
            )
        }) {
            out.push(p);
        }
    }
}

/// What gates a refused item, from its error.
fn gate(message: &str) -> &'static str {
    const GATES: [(&str, &str); 9] = [
        ("encrypted", "protected item, no cipher given"),
        ("has no rloc", "hvt1 tile item without the mandatory rloc"),
        (
            "external",
            "tili item with external tiles, no resolver given",
        ),
        (
            "pred",
            "predictively coded item whose references do not decode",
        ),
        (
            "essential properties not recognised",
            "unrecognised essential property",
        ),
        (
            "no reference picture",
            "inter-coded picture without its reference",
        ),
        (
            "cexg",
            "constrained extents without a decoder configuration",
        ),
        ("Unsupported", "layout the codec crate refuses"),
        ("unsupported", "layout the codec crate refuses"),
    ];
    GATES
        .iter()
        .find(|(needle, _)| message.contains(needle))
        .map_or("other", |(_, g)| g)
}

#[test]
fn every_image_item_decodes_or_is_a_known_refusal() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    walk(&root.join("tests/fixtures"), &mut files);
    for sub in [
        "heif/fixtures",
        "heif/device-samples",
        "heif/conformance/heic",
        "heif/conformance/cenc",
    ] {
        walk(&root.join("../../docs/image").join(sub), &mut files);
    }
    // A debug build leaves the pictures above 4 MP to a release run
    // (every tile of a 12 MP grid, unoptimised, is minutes).
    let full = !cfg!(debug_assertions) || std::env::var("OXIDEAV_HEIF_MATRIX_FULL").is_ok();
    let (mut items, mut decoded, mut skipped, mut keyed) = (0usize, 0usize, 0usize, 0usize);
    let have_cipher = common::have_binary("openssl");
    let mut refused: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let Ok(file) = HeifFile::parse(&bytes) else {
            continue;
        };
        let Ok(meta) = file.meta() else {
            continue;
        };
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for item in meta.items.iter().filter(|i| i.is_image()) {
            if !full
                && oxideav_heif::derived::build_graph(&file, item.id)
                    .ok()
                    .and_then(|n| n.output_size().ok())
                    .is_some_and(|(w, h)| u64::from(w) * u64::from(h) > 4 << 20)
            {
                skipped += 1;
                continue;
            }
            items += 1;
            // A protected item is decoded under its test keys when the
            // black-box AES-128 is there (the crate ships no cipher).
            let cipher = (meta.protection_scheme(item.id).is_some() && have_cipher)
                .then(|| {
                    let cfg = path.parent()?.join("drm_cfg").join(format!(
                        "{}.xml",
                        path.file_stem()?
                            .to_string_lossy()
                            .trim_start_matches("image_")
                    ));
                    Some(OpenSsl(keys(&std::fs::read_to_string(cfg).ok()?)))
                })
                .flatten();
            let decoder = match &cipher {
                Some(c) => {
                    keyed += 1;
                    ItemDecoder::direct()
                        .with_decryptor(c)
                        .with_cenc_reading(CencReading::compat())
                }
                None => ItemDecoder::direct(),
            };
            match decode_item(&file, item.id, decoder) {
                Ok(img) => {
                    assert!(
                        img.width() > 0 && img.height() > 0,
                        "{name} item {}",
                        item.id
                    );
                    decoded += 1;
                }
                Err(e) => refused
                    .entry(gate(&format!("{e:?} {e}")))
                    .or_default()
                    .push(format!("{name} item {}: {e}", item.id)),
            }
        }
    }
    eprintln!(
        "item sweep: {} files, {items} image items, {decoded} decode ({keyed} under their test keys), {} refused, {skipped} left to a release run",
        files.len(),
        items - decoded
    );
    for (g, list) in &refused {
        eprintln!("  {} — {g}", list.len());
        for l in list.iter().take(if g == &"other" { usize::MAX } else { 3 }) {
            eprintln!("    {}", l.chars().take(220).collect::<String>());
        }
    }
    assert!(items > 0, "the vendored fixtures hold image items");
    assert!(
        !refused.contains_key("other"),
        "items refused outside the known gates: {:#?}",
        refused.get("other")
    );
}

/// MIAF007's thumbnail (128×72) is associated with its 1280×720
/// master's non-essential `clap` (640×360), `irot` and `imir`. §6.3
/// lets a reader apply any set of the non-essential transformative
/// properties: the aperture that does not fit the thumbnail is left
/// out, the rotation applies — 72×128, the master's 360×640 aspect.
#[test]
fn a_non_essential_clap_larger_than_its_picture_is_left_out() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/image/heif/conformance/heic/MIAF007.heic");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("SKIP: docs/image/heif/conformance not checked out");
        return;
    };
    let file = HeifFile::parse(&bytes).unwrap();
    let node = oxideav_heif::derived::build_graph(&file, 1005).unwrap();
    assert_eq!(node.output_size().unwrap(), (72, 128));
    let thumb = decode_item(&file, 1005, ItemDecoder::direct()).unwrap();
    assert_eq!((thumb.width(), thumb.height()), (72, 128));
    let master = decode_item(&file, 1002, ItemDecoder::direct()).unwrap();
    assert_eq!((master.width(), master.height()), (360, 640));
}
