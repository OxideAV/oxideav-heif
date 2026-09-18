//! Container-level trace equivalence against the staged corpus
//! (`docs/image/heif/heif-fixtures-and-traces.md` §2.3 vocabulary).
//!
//! Each bundle's `trace.txt` is an observation log of the container
//! surface; this crate's model must reproduce the same events, in the
//! same order, with the same values. Tags are added per layer as the
//! crate grows (box walk + meta model here; `hvcC` / derived-image /
//! alpha events in `tests/props.rs` and `tests/derived.rs`).

mod common;

use common::{
    assert_trace_subset_eq, fixture_bytes, fixture_root, read_trace, TraceEvent, BUNDLES,
};
use oxideav_heif::boxes::{fourcc_str, iter_boxes, parse_full_box, payload, FourCc};
use oxideav_heif::HeifFile;

/// Containers the corpus trace descends into.
const DESCEND: &[&[u8; 4]] = &[
    b"meta", b"iprp", b"ipco", b"iinf", b"iref", b"moov", b"trak", b"mdia", b"minf", b"dinf",
    b"stbl",
];

fn ev(tag: &str, fields: &[(&str, String)]) -> TraceEvent {
    TraceEvent {
        tag: tag.to_string(),
        fields: fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    }
}

fn box_events(file: &[u8], start: usize, end: usize, out: &mut Vec<TraceEvent>) {
    let region = &file[..end];
    let mut cursor = start;
    while cursor < end {
        let h = match oxideav_heif::boxes::parse_box_header(region, cursor) {
            Ok(h) => h,
            Err(_) => return,
        };
        out.push(ev(
            "BOX",
            &[
                ("type", fourcc_str(&h.box_type)),
                ("size", h.total_len().to_string()),
                ("offset", h.start.to_string()),
            ],
        ));
        if DESCEND.contains(&&h.box_type) {
            let child_start = match &h.box_type {
                b"meta" | b"iref" => h.payload_start + 4,
                b"iinf" => {
                    let p = payload(file, &h);
                    match parse_full_box(p) {
                        Ok((0, _, _)) => h.payload_start + 6,
                        _ => h.payload_start + 8,
                    }
                }
                _ => h.payload_start,
            };
            box_events(file, child_start, h.end(), out);
        }
        cursor = h.end();
    }
}

/// Render the container-model events this test layer owns.
pub fn model_events(f: &HeifFile) -> Vec<TraceEvent> {
    let mut out = Vec::new();
    box_events(f.bytes(), 0, f.bytes().len(), &mut out);
    let Some(meta) = &f.meta else {
        return out;
    };
    if let Some(p) = meta.primary_item_id {
        out.push(ev("PITM", &[("item_id", p.to_string())]));
    }
    for it in &meta.items {
        out.push(ev(
            "ITEM_INFO",
            &[
                ("item_id", it.id.to_string()),
                ("type", fourcc_str(&it.item_type)),
                ("name", format!("'{}'", it.name)),
            ],
        ));
    }
    for r in &meta.references {
        for to in &r.to_item_ids {
            out.push(ev(
                "IREF",
                &[
                    ("from_id", r.from_item_id.to_string()),
                    ("to_id", to.to_string()),
                    ("ref_type", fourcc_str(&r.reference_type)),
                ],
            ));
        }
    }
    for (i, p) in meta.properties.iter().enumerate() {
        out.push(ev(
            "IPRP_PROP",
            &[
                ("index", (i + 1).to_string()),
                ("type", fourcc_str(&p.box_type)),
                ("size", p.box_size.to_string()),
            ],
        ));
    }
    for a in &meta.associations {
        let idx: Vec<String> = a.entries.iter().map(|e| e.index.to_string()).collect();
        out.push(ev(
            "IPRP_ASSOC",
            &[
                ("item_id", a.item_id.to_string()),
                ("property_indices", idx.join(",")),
            ],
        ));
    }
    out
}

const TAGS: &[&str] = &[
    "BOX",
    "PITM",
    "ITEM_INFO",
    "IREF",
    "IPRP_PROP",
    "IPRP_ASSOC",
];

#[test]
fn container_model_matches_corpus_traces() {
    let Some(root) = fixture_root() else {
        eprintln!("fixture corpus not present; skipping");
        return;
    };
    for bundle in BUNDLES {
        let bytes = fixture_bytes(&root, bundle);
        let f = HeifFile::parse(&bytes).unwrap_or_else(|e| panic!("{bundle}: {e}"));
        let expected = read_trace(&root, bundle);
        let got = model_events(&f);
        assert_trace_subset_eq(bundle, TAGS, &expected, &got);
    }
}

#[test]
fn every_bundle_declares_heif_brands_and_a_pict_handler() {
    let Some(root) = fixture_root() else {
        return;
    };
    for bundle in BUNDLES {
        let f = HeifFile::parse(&fixture_bytes(&root, bundle)).unwrap();
        assert!(f.file_type.is_heif_family(), "{bundle}");
        let c = f.file_type.classify();
        assert!(c.image_collection, "{bundle}");
        assert_eq!(
            c.image_sequence,
            *bundle == "image-sequence-3frame",
            "{bundle}"
        );
        let meta = f.meta().unwrap();
        assert_eq!(
            meta.handler.as_ref().unwrap().handler_type,
            *b"pict",
            "{bundle}"
        );
        let primary = f.primary_item().unwrap();
        assert!(
            primary.is_image(),
            "{bundle}: primary {:?}",
            primary.item_type
        );
        // Every coded image item resolves to a non-empty payload.
        for it in meta.items.iter().filter(|i| i.is_coded_image()) {
            let data = f.item_data(it.id).unwrap();
            assert!(!data.is_empty(), "{bundle}: item {} empty", it.id);
        }
        assert_eq!(f.has_moov(), c.image_sequence, "{bundle}");
    }
}

#[test]
fn derived_payloads_and_metadata_items_resolve() {
    let Some(root) = fixture_root() else {
        return;
    };
    let f = HeifFile::parse(&fixture_bytes(&root, "still-image-grid-2x2")).unwrap();
    let grid = f.item_data(1).unwrap();
    assert_eq!(grid.len(), 8, "grid payload with 16-bit dims");
    assert_eq!(&grid[..4], &[0, 0, 1, 1]);
    assert_eq!(f.meta().unwrap().derivation_inputs(1), vec![2, 3, 4, 5]);

    let f = HeifFile::parse(&fixture_bytes(&root, "still-image-overlay")).unwrap();
    let meta = f.meta().unwrap();
    assert_eq!(meta.derivation_inputs(4), vec![1, 2]);
    assert_eq!(meta.auxiliaries_of(2), vec![3]);
    let iovl = f.item_data(4).unwrap();
    assert_eq!(iovl.len(), 2 + 8 + 4 + 2 * 4);

    let f = HeifFile::parse(&fixture_bytes(&root, "still-image-with-exif")).unwrap();
    let meta = f.meta().unwrap();
    let exif_ids = meta.metadata_of(1);
    assert_eq!(exif_ids.len(), 1);
    let exif = f.item_data(exif_ids[0]).unwrap();
    let off = u32::from_be_bytes([exif[0], exif[1], exif[2], exif[3]]) as usize;
    let tiff = &exif[4 + off..];
    assert!(
        tiff.starts_with(b"II") || tiff.starts_with(b"MM"),
        "TIFF header after offset word"
    );

    let f = HeifFile::parse(&fixture_bytes(&root, "still-image-with-xmp")).unwrap();
    let meta = f.meta().unwrap();
    let xmp_id = meta.metadata_of(1)[0];
    assert!(meta.item(xmp_id).unwrap().is_xmp());
    let xmp = f.item_data(xmp_id).unwrap();
    assert!(std::str::from_utf8(&xmp)
        .unwrap()
        .contains("OxideAV HEIF test"));

    let f = HeifFile::parse(&fixture_bytes(&root, "single-image-with-thumbnail")).unwrap();
    let meta = f.meta().unwrap();
    let primary = meta.primary_item_id.unwrap();
    assert_eq!(meta.thumbnails_of(primary).len(), 1);

    let f = HeifFile::parse(&fixture_bytes(&root, "still-image-with-alpha")).unwrap();
    let meta = f.meta().unwrap();
    let alpha = meta.auxiliaries_of(meta.primary_item_id.unwrap());
    assert_eq!(alpha.len(), 1);
    let auxc = meta.property_of(alpha[0], b"auxC").unwrap();
    assert!(auxc.body.windows(4).any(|w| w == b"urn:"));
}

#[test]
fn ftyp_round_trips_byte_exact() {
    let Some(root) = fixture_root() else {
        return;
    };
    for bundle in BUNDLES {
        let bytes = fixture_bytes(&root, bundle);
        let f = HeifFile::parse(&bytes).unwrap();
        let ftyp_len = f.top_level[0].total_len();
        assert_eq!(f.file_type.to_box(), &bytes[..ftyp_len], "{bundle}");
        let _: FourCc = f.file_type.major_brand;
    }
}

#[test]
fn top_level_walk_is_consistent_with_iter_boxes() {
    let Some(root) = fixture_root() else {
        return;
    };
    for bundle in BUNDLES {
        let bytes = fixture_bytes(&root, bundle);
        let f = HeifFile::parse(&bytes).unwrap();
        let direct: Vec<_> = iter_boxes(&bytes).map(|h| h.unwrap()).collect();
        assert_eq!(direct, f.top_level, "{bundle}");
        let walk = f.box_walk().unwrap();
        let tops: Vec<_> = walk
            .iter()
            .filter(|e| e.depth == 0)
            .map(|e| e.header.clone())
            .collect();
        assert_eq!(tops, f.top_level, "{bundle}");
    }
}
