//! Shared fixture plumbing for the integration tests.
//!
//! The corpus lives outside the crate (`docs/image/heif/fixtures/` in
//! the umbrella workspace, or wherever `OXIDEAV_HEIF_FIXTURES` points).
//! Every test that needs it calls [`fixture_root`] and returns early
//! when the corpus is absent, so the crate's CI (which has no `docs/`)
//! stays green while the local run exercises every bundle.
#![allow(dead_code)]

pub mod png;

use std::path::{Path, PathBuf};

/// The 14 bundles of the staged corpus, in the order the corpus README
/// lists them.
pub const BUNDLES: &[&str] = &[
    "single-image-512x512-q60",
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

/// Locate the fixture corpus, or `None` when it is not checked out.
pub fn fixture_root() -> Option<PathBuf> {
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
