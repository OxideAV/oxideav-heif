//! Shared fixture plumbing for the integration tests.
//!
//! Two corpora feed the tests:
//!
//! * the **vendored** corpus under `tests/fixtures/` (always present, so
//!   every pixel / structure assertion runs on CI): `corpus/` holds 13
//!   of the 14 staged bundles (all but the 98 KB `single-image-512x512-q60`
//!   photo and its 400 KB oracle), `interop/` the black-box producer
//!   matrix (Apple ImageIO `sips`, `heif-enc`, `magick`) with the
//!   black-box decoder fingerprints in `manifest.tsv`;
//! * the optional **docs superset** (`docs/image/heif/fixtures/` in the
//!   umbrella workspace, or wherever `OXIDEAV_HEIF_FIXTURES` points),
//!   which adds the big bundle when checked out.
//!
//! [`all_bundles`] yields every `(root, bundle)` pair across both.
#![allow(dead_code)]

pub mod png;

use std::path::{Path, PathBuf};

/// The bundles of the vendored corpus, in the order the corpus README
/// lists them.
pub const BUNDLES: &[&str] = &[
    "single-image-1x1",
    "single-image-with-thumbnail",
    "still-image-with-alpha",
    "still-image-grid-2x2",
    "still-image-overlay",
    "still-image-with-icc",
    "still-image-with-exif",
    "still-image-with-xmp",
    "multi-image-burst-3",
    "still-monochrome",
    "still-10bit-main10",
    "still-yuv444",
    "image-sequence-3frame",
];

/// Bundles only the docs superset carries.
pub const DOCS_ONLY_BUNDLES: &[&str] = &["single-image-512x512-q60"];

/// The vendored corpus root (always present).
pub fn vendored_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/corpus")
}

/// The vendored interop matrix root (always present).
pub fn interop_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/interop")
}

/// The docs superset root, when checked out.
pub fn docs_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OXIDEAV_HEIF_FIXTURES") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Some(p);
        }
    }
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let candidate = here.join("../../docs/image/heif/fixtures");
    if candidate.is_dir() {
        return Some(candidate);
    }
    None
}

/// The corpus root tests that name a bundle explicitly read from: the
/// vendored corpus. Kept as an `Option` for the historical call shape;
/// it is always `Some`.
pub fn fixture_root() -> Option<PathBuf> {
    Some(vendored_root())
}

/// Every `(root, bundle)` pair: the vendored bundles, then every
/// bundle of the docs superset (when present) so the local run also
/// covers the big photo.
pub fn all_bundles() -> Vec<(PathBuf, &'static str)> {
    let mut v: Vec<(PathBuf, &'static str)> =
        BUNDLES.iter().map(|b| (vendored_root(), *b)).collect();
    if let Some(docs) = docs_root() {
        for b in BUNDLES.iter().chain(DOCS_ONLY_BUNDLES) {
            if docs.join(b).is_dir() {
                v.push((docs.clone(), b));
            }
        }
    }
    v
}

/// `input.heic` bytes of a bundle.
pub fn fixture_bytes(root: &Path, bundle: &str) -> Vec<u8> {
    let dir = root.join(bundle);
    let heic = dir.join("input.heic");
    let heif = dir.join("input.heif");
    let path = if heic.is_file() { heic } else { heif };
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// One event of a `trace.txt`: tag + ordered `key=value` fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceEvent {
    pub tag: String,
    pub fields: Vec<(String, String)>,
}

impl TraceEvent {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn render(&self) -> String {
        let mut s = self.tag.clone();
        for (k, v) in &self.fields {
            s.push('\t');
            s.push_str(k);
            s.push('=');
            s.push_str(v);
        }
        s
    }
}

/// Parse a bundle's `trace.txt`.
pub fn read_trace(root: &Path, bundle: &str) -> Vec<TraceEvent> {
    let path = root.join(bundle).join("trace.txt");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut parts = l.split('\t');
            let tag = parts.next().unwrap_or("").to_string();
            let fields = parts
                .map(|kv| {
                    let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                    (k.to_string(), v.to_string())
                })
                .collect();
            TraceEvent { tag, fields }
        })
        .collect()
}

/// Compare the events of `tags` in both lists, in order, rendering a
/// readable diff on mismatch.
pub fn assert_trace_subset_eq(
    bundle: &str,
    tags: &[&str],
    expected: &[TraceEvent],
    got: &[TraceEvent],
) {
    let exp: Vec<String> = expected
        .iter()
        .filter(|e| tags.contains(&e.tag.as_str()))
        .map(TraceEvent::render)
        .collect();
    let got: Vec<String> = got
        .iter()
        .filter(|e| tags.contains(&e.tag.as_str()))
        .map(TraceEvent::render)
        .collect();
    if exp != got {
        let mut msg = format!("trace mismatch for {bundle} (tags {tags:?})\n--- expected\n");
        for l in &exp {
            msg.push_str(l);
            msg.push('\n');
        }
        msg.push_str("--- got\n");
        for l in &got {
            msg.push_str(l);
            msg.push('\n');
        }
        panic!("{msg}");
    }
}

/// `true` when `bin` runs (`--version` / `-version` / `--help`).
pub fn have_binary(bin: &str) -> bool {
    for flag in ["--version", "-version", "--help"] {
        if let Ok(out) = std::process::Command::new(bin).arg(flag).output() {
            if out.status.success() {
                return true;
            }
        }
    }
    false
}

/// A scratch directory unique to this test process.
pub fn scratch_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("oxideav-heif-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// FNV-1a 64 over `bytes` (the fingerprint used by `interop/manifest.tsv`).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
