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
