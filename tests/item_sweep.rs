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

use oxideav_heif::HeifFile;
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
    const GATES: [(&str, &str); 10] = [
        ("encrypted", "protected item, no cipher given"),
        (
            "clean aperture",
            "clap larger than the item's picture (a thumbnail carrying its master's)",
        ),
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
    let (mut items, mut decoded, mut skipped) = (0usize, 0usize, 0usize);
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
            match decode_item(&file, item.id, ItemDecoder::direct()) {
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
        "item sweep: {} files, {items} image items, {decoded} decode, {} refused, {skipped} left to a release run",
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
