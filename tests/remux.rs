//! Re-muxing without re-encoding: `HeifWriter::import_item` carries an
//! item — with its derivations, auxiliaries, thumbnails, metadata items
//! and `altr` alternates — from a parsed file into a new one, byte for
//! byte. The oracle is the corpus: for every HEIF-family file the
//! checkout can see (vendored fixtures, the staged corpus, the device
//! samples, the conformance set), the primary image of
//! `HeifWriter::from_files(&[file])` decodes to the same pixels, colour
//! and metadata as the original's, through the contract path, and the
//! written file passes the MIAF checks whenever the original did.
#![cfg(feature = "registry")]

mod common;

use std::path::{Path, PathBuf};

use oxideav_heif::miaf::{MiafProfile, check};
use oxideav_heif::{DecodeOptions, HeifFile, HeifWriter, decode_with, info};

fn is_heif_name(p: &Path) -> bool {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "heic" | "heif" | "heics" | "heifs" | "hif" | "avif" | "avifs" | "hmg"
    )
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else if is_heif_name(&p) {
            out.push(p);
        }
    }
}

fn inputs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        &mut v,
    );
    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/image");
    for sub in [
        "heif/fixtures",
        "heif/device-samples",
        "heif/conformance/heic",
        "heif/conformance/cenc",
        "avif",
    ] {
        let d = docs.join(sub);
        if d.is_dir() {
            walk(&d, &mut v);
        }
    }
    // A debug build keeps the work under 4 M samples per file.
    if cfg!(debug_assertions) {
        v.retain(|p| {
            let bytes = std::fs::read(p).unwrap_or_default();
            match info(&bytes) {
                Ok(i) => (i.frames as u64) * (i.width as u64) * (i.height as u64) <= 4 << 20,
                Err(_) => true,
            }
        });
    }
    v
}

#[test]
fn every_corpus_primary_remuxes_to_the_same_image() {
    let opts = DecodeOptions::default().with_strict(false);
    let (mut remuxed, mut refused, mut undecodable) = (0usize, Vec::new(), 0usize);
    for path in inputs() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        let Ok(src) = HeifFile::parse(&bytes) else {
            continue;
        };
        let Ok(original) = decode_with(&bytes, &opts) else {
            undecodable += 1;
            continue;
        };
        let out = match HeifWriter::from_files(&[&bytes]) {
            Ok(w) => w
                .write_to_vec()
                .unwrap_or_else(|e| panic!("{name}: write: {e}")),
            Err(e) => {
                // Protected items and `tili` tiled items are typed refusals.
                refused.push(format!("{name}: {e}"));
                continue;
            }
        };
        let dst = HeifFile::parse(&out).unwrap_or_else(|e| panic!("{name}: re-parse: {e}"));
        let back = decode_with(&out, &opts).unwrap_or_else(|e| panic!("{name}: decode: {e}"));
        assert_eq!(
            (back.width, back.height, back.format),
            (original.width, original.height, original.format),
            "{name}: geometry / layout"
        );
        assert_eq!(back.planes, original.planes, "{name}: pixels");
        assert_eq!(back.color, original.color, "{name}: colour");
        assert_eq!(back.metadata, original.metadata, "{name}: metadata");
        // The re-muxed file is as conformant as the source (its brands
        // are decided truthfully, so `miaf` appears iff the checks pass).
        let was = check(&src, MiafProfile::Miaf)
            .map(|r| r.is_conformant())
            .unwrap_or(false);
        let now = check(&dst, MiafProfile::Miaf)
            .map(|r| r.is_conformant())
            .unwrap_or(false);
        assert_eq!(
            now,
            dst.file_type.has_brand(b"miaf"),
            "{name}: miaf brand truth"
        );
        if was {
            assert!(now, "{name}: the re-mux lost MIAF conformance");
        }
        remuxed += 1;
    }
    eprintln!(
        "re-muxed {remuxed} files, {} refused ({:?}), {undecodable} undecodable sources",
        refused.len(),
        refused
    );
    // Every decodable input re-muxes (nothing is refused), and the
    // vendored set alone (57 files on CI, without the docs corpus)
    // clears the floor.
    assert!(refused.is_empty(), "refused: {refused:?}");
    assert!(remuxed >= 50, "only {remuxed} files re-muxed");
}

/// Reader / writer symmetry, item by item: every image item of every
/// file — grid tiles, thumbnails, auxiliaries, hidden items, tile
/// items, not only what a viewer shows — imported in one call
/// (`import_items`: shared inputs once) decodes from the written file
/// to the picture it decodes to in its source. An item the source
/// refuses (a protected item without its key, a tile item without
/// `rloc`) is carried all the same and refused the same way.
#[test]
fn every_image_item_remuxes_to_the_same_picture() {
    use oxideav_heif::decode::{ItemDecoder, decode_item};
    let (mut files, mut items, mut same_refusal) = (0usize, 0usize, 0usize);
    for path in inputs() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        let Ok(src) = HeifFile::parse(&bytes) else {
            continue;
        };
        let Ok(meta) = src.meta() else {
            continue;
        };
        let Ok(primary) = src.primary_item().map(|p| p.id) else {
            continue;
        };
        let ids: Vec<u32> = meta
            .items
            .iter()
            .filter(|i| i.is_image())
            .map(|i| i.id)
            .collect();
        let mut w = HeifWriter::new();
        let new_ids = w
            .import_items(&src, &ids)
            .unwrap_or_else(|e| panic!("{name}: import: {e}"));
        let at = ids.iter().position(|i| *i == primary).unwrap();
        w.set_primary(new_ids[at]);
        let out = w
            .write_to_vec()
            .unwrap_or_else(|e| panic!("{name}: write: {e}"));
        let dst = HeifFile::parse(&out).unwrap_or_else(|e| panic!("{name}: re-parse: {e}"));
        // Nothing imported twice: at most one item per source item.
        assert!(
            dst.meta().unwrap().items.len() <= meta.items.len(),
            "{name}: {} items written for {} in the source",
            dst.meta().unwrap().items.len(),
            meta.items.len()
        );
        for (old, new) in ids.iter().zip(&new_ids) {
            let a = decode_item(&src, *old, ItemDecoder::direct());
            let b = decode_item(&dst, *new, ItemDecoder::direct());
            match (a, b) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a.frame, b.frame, "{name}: item {old} → {new}: pixels");
                    items += 1;
                }
                (Err(_), Err(_)) => same_refusal += 1,
                (a, b) => panic!(
                    "{name}: item {old} → {new}: source {:?}, re-mux {:?}",
                    a.map(|i| (i.width(), i.height()))
                        .map_err(|e| e.to_string()),
                    b.map(|i| (i.width(), i.height()))
                        .map_err(|e| e.to_string())
                ),
            }
        }
        files += 1;
    }
    eprintln!(
        "item re-mux: {files} files, {items} image items decode to the same picture, \
         {same_refusal} refused on both sides"
    );
    assert!(items >= 60, "only {items} items compared");
}

/// The whole collection of a file (`from_collection`: every
/// displayable item, shared inputs once, the groups among them) shows
/// what the source shows, and writing it is a fixed point: the
/// collection of the written file writes the same bytes.
#[test]
fn a_collection_remux_is_a_fixed_point() {
    use oxideav_heif::decode_all_partial;
    let opts = DecodeOptions::default().with_strict(false);
    let (mut files, mut frames) = (0usize, 0usize);
    for path in inputs() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        let Ok(src) = HeifFile::parse(&bytes) else {
            continue;
        };
        if src.primary_item().is_err() {
            continue;
        }
        let once = HeifWriter::from_collection(&bytes)
            .unwrap_or_else(|e| panic!("{name}: collection: {e}"))
            .write_to_vec()
            .unwrap_or_else(|e| panic!("{name}: write: {e}"));
        let twice = HeifWriter::from_collection(&once)
            .unwrap_or_else(|e| panic!("{name}: collection of the re-mux: {e}"))
            .write_to_vec()
            .unwrap();
        assert!(once == twice, "{name}: the re-mux of the re-mux differs");
        // The stills a viewer is shown, in order (a source's tracks
        // are not items and stay behind).
        let stills = |b: &[u8]| -> Vec<_> {
            let Ok(p) = decode_all_partial(b, &opts) else {
                return Vec::new();
            };
            p.frames
                .into_iter()
                .filter(|f| f.track_id.is_none())
                .map(|f| {
                    (
                        f.image.width,
                        f.image.height,
                        f.image.format,
                        f.image.planes,
                    )
                })
                .collect()
        };
        let (a, b) = (stills(&bytes), stills(&once));
        assert_eq!(a.len(), b.len(), "{name}: stills shown");
        assert!(a == b, "{name}: the collection shows other pictures");
        frames += a.len();
        files += 1;
    }
    eprintln!("collection re-mux: {files} files, {frames} stills, byte-stable on the second pass");
    assert!(files >= 50, "only {files} collections re-muxed");
}
