//! Property-surface checks against the corpus: `hvcC` field trace
//! equivalence (`HVCC` / `HEVC_FRAME_FOR_ITEM` events), typed property
//! assertions per bundle, derivation-graph shape and MIAF conformance.

mod common;

use common::{
    all_bundles, assert_trace_subset_eq, fixture_bytes, fixture_root, read_trace, TraceEvent,
};
use oxideav_heif::derived::{build_primary_graph, ImageKind};
use oxideav_heif::meta::ITEM_TYPE_HVC1;
use oxideav_heif::miaf::{check, MiafProfile};
use oxideav_heif::props::{AuxKind, Colr};
use oxideav_heif::{HeifFile, ItemProperties};

fn ev(tag: &str, fields: &[(&str, String)]) -> TraceEvent {
    TraceEvent {
        tag: tag.to_string(),
        fields: fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    }
}

/// `HVCC` + `HEVC_FRAME_FOR_ITEM` events, per `hvc1` item in `iinf` order.
fn hvcc_events(f: &HeifFile) -> Vec<TraceEvent> {
    let meta = f.meta().unwrap();
    let mut out = Vec::new();
    for it in meta.items.iter().filter(|i| i.item_type == ITEM_TYPE_HVC1) {
        let props = ItemProperties::resolve(meta, it.id).unwrap();
        let h = props.hvcc().expect("hvc1 item carries hvcC");
        out.push(ev(
            "HVCC",
            &[
                ("item_id", it.id.to_string()),
                ("profile", h.general_profile_idc.to_string()),
                ("tier", (h.general_tier_flag as u8).to_string()),
                ("level", h.general_level_idc.to_string()),
                ("chroma", h.chroma_name().to_string()),
                ("bd_luma", h.bit_depth_luma().to_string()),
                ("bd_chroma", h.bit_depth_chroma().to_string()),
                ("num_arrays", h.arrays.len().to_string()),
                ("num_nals", h.nal_count().to_string()),
            ],
        ));
        let ispe = props.ispe().expect("hvc1 item carries ispe");
        out.push(ev(
            "HEVC_FRAME_FOR_ITEM",
            &[
                ("item_id", it.id.to_string()),
                ("dims", format!("{}x{}", ispe.width, ispe.height)),
                ("qp", "N/A".to_string()),
            ],
        ));
    }
    out
}

#[test]
fn hvcc_fields_match_corpus_traces() {
    for (root, bundle) in all_bundles() {
        let f = HeifFile::parse(&fixture_bytes(&root, bundle)).unwrap();
        let expected = read_trace(&root, bundle);
        let got = hvcc_events(&f);
        assert_trace_subset_eq(bundle, &["HVCC", "HEVC_FRAME_FOR_ITEM"], &expected, &got);
    }
}

#[test]
fn hvcc_records_reserialize_byte_exact_and_carry_parameter_sets() {
    for (root, bundle) in all_bundles() {
        let f = HeifFile::parse(&fixture_bytes(&root, bundle)).unwrap();
        let meta = f.meta().unwrap();
        for it in meta.items.iter().filter(|i| i.item_type == ITEM_TYPE_HVC1) {
            let props = ItemProperties::resolve(meta, it.id).unwrap();
            let h = props.hvcc().unwrap();
            assert_eq!(h.serialize(), h.raw, "{bundle} item {}", it.id);
            assert_eq!(h.nal_units_of_type(oxideav_heif::hvcc::NAL_VPS).len(), 1);
            assert_eq!(h.nal_units_of_type(oxideav_heif::hvcc::NAL_SPS).len(), 1);
            assert_eq!(h.nal_units_of_type(oxideav_heif::hvcc::NAL_PPS).len(), 1);
            assert_eq!(h.length_size, 4);
            // The item payload is a run of length-prefixed NAL units and
            // holds exactly one access unit (Annex B.2.2.1.2): at least
            // one VCL NAL unit, no parameter sets required in band.
            let data = f.item_data(it.id).unwrap();
            let nals = oxideav_heif::hvcc::split_length_prefixed(&data, h.length_size).unwrap();
            assert!(!nals.is_empty(), "{bundle} item {}", it.id);
            let vcl = nals
                .iter()
                .filter(|n| {
                    oxideav_heif::hvcc::nal_unit_type(n)
                        .map(|t| t < 32)
                        .unwrap_or(false)
                })
                .count();
            assert!(vcl >= 1, "{bundle} item {}: no VCL NAL unit", it.id);
            // hvcC is essential (B.2.3.1) and ispe precedes transforms.
            let e = props
                .iter()
                .find(|e| matches!(e.property, oxideav_heif::Property::HvcC(_)))
                .unwrap();
            assert!(e.essential, "{bundle} item {}: hvcC not essential", it.id);
        }
    }
}

#[test]
fn per_bundle_property_expectations() {
    let Some(root) = fixture_root() else {
        return;
    };
    let load = |b: &str| HeifFile::parse(&fixture_bytes(&root, b)).unwrap();
    let props_of = |f: &HeifFile, id: u32| ItemProperties::resolve(f.meta().unwrap(), id).unwrap();

    // 1×1: clap crops a 64×64 coded picture; essential + resolves to (0,0,1,1).
    let f = load("single-image-1x1");
    let p = props_of(&f, f.meta().unwrap().primary_item_id.unwrap());
    assert_eq!(p.ispe().map(|i| (i.width, i.height)), Some((64, 64)));
    let clap = p.clap().expect("clap");
    assert_eq!(clap.resolve(64, 64).unwrap().width, 1);
    assert_eq!(clap.resolve(64, 64).unwrap().height, 1);
    assert_eq!(p.output_size((64, 64), false).unwrap(), (1, 1));

    // ICC: a `prof` colr with the 2576-byte sRGB profile.
    let f = load("still-image-with-icc");
    let p = props_of(&f, f.meta().unwrap().primary_item_id.unwrap());
    let icc = p.icc_profile().expect("ICC profile");
    assert_eq!(icc.len(), 2576);
    assert_eq!(&icc[36..40], b"acsp", "ICC signature");

    // nclx on the typical photo (docs superset only: the bundle is not vendored).
    if let Some(docs) = common::docs_root() {
        let f = HeifFile::parse(&fixture_bytes(&docs, "single-image-512x512-q60")).unwrap();
        let p = props_of(&f, f.meta().unwrap().primary_item_id.unwrap());
        match p.nclx() {
            Some(Colr::Nclx { matrix, .. }) => assert!(*matrix <= 14),
            other => panic!("expected nclx, got {other:?}"),
        }
        // The corpus producer writes a single-channel pixi even for 4:2:0
        // colour items; only the depth is asserted.
        let pixi = p.pixi().unwrap();
        assert_eq!(pixi.max_bit_depth(), 8);
    }

    // 10-bit: pixi and hvcC agree.
    let f = load("still-10bit-main10");
    let p = props_of(&f, f.meta().unwrap().primary_item_id.unwrap());
    assert_eq!(p.pixi().unwrap().max_bit_depth(), 10);
    assert_eq!(p.hvcc().unwrap().bit_depth_luma(), 10);

    // Monochrome: one pixi channel, chroma_format_idc 0.
    let f = load("still-monochrome");
    let p = props_of(&f, f.meta().unwrap().primary_item_id.unwrap());
    assert_eq!(p.pixi().unwrap().num_channels(), 1);
    assert_eq!(p.hvcc().unwrap().chroma_format_idc, 0);

    // 4:4:4.
    let f = load("still-yuv444");
    let p = props_of(&f, f.meta().unwrap().primary_item_id.unwrap());
    assert_eq!(p.hvcc().unwrap().chroma_format_idc, 3);

    // Alpha: auxC URN classifies as alpha, monochrome coding.
    let f = load("still-image-with-alpha");
    let meta = f.meta().unwrap();
    let alpha_id = meta.auxiliaries_of(meta.primary_item_id.unwrap())[0];
    let p = props_of(&f, alpha_id);
    assert_eq!(p.auxc().unwrap().kind(), AuxKind::Alpha);
    assert_eq!(
        p.auxc().unwrap().aux_type,
        oxideav_heif::props::AUX_URN_ALPHA_HEVC
    );
    assert_eq!(p.hvcc().unwrap().chroma_format_idc, 0);
}

#[test]
fn derivation_graphs_have_the_expected_shape() {
    let Some(root) = fixture_root() else {
        return;
    };
    let load = |b: &str| HeifFile::parse(&fixture_bytes(&root, b)).unwrap();

    let g = build_primary_graph(&load("still-image-grid-2x2")).unwrap();
    match &g.kind {
        ImageKind::Grid(d) => {
            assert_eq!((d.rows, d.columns), (2, 2));
            assert_eq!((d.output_width, d.output_height), (256, 256));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(g.inputs.len(), 4);
    assert_eq!(g.coded_items().len(), 4);
    assert_eq!(g.output_size().unwrap(), (256, 256));
    assert_eq!(g.chain_types(), vec![*b"grid", *b"hvc1"]);

    let o = build_primary_graph(&load("still-image-overlay")).unwrap();
    match &o.kind {
        ImageKind::Overlay(d) => {
            assert_eq!(d.canvas_fill, [16384, 16384, 16384, 65535]);
            assert_eq!(d.offsets, vec![(0, 0), (96, 96)]);
            assert_eq!((d.output_width, d.output_height), (256, 256));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(o.inputs.len(), 2);
    assert!(
        o.inputs[1].alpha.is_some(),
        "stamp carries an alpha auxiliary"
    );
    assert!(!o.inputs[1].premultiplied_alpha);
    assert!(o.alpha.is_none());

    let a = build_primary_graph(&load("still-image-with-alpha")).unwrap();
    assert!(a.alpha.is_some());
    assert_eq!(a.alpha.as_ref().unwrap().ispe(), a.ispe());

    let t = build_primary_graph(&load("single-image-with-thumbnail")).unwrap();
    assert_eq!(t.thumbnails.len(), 1);
    assert_eq!(t.thumbnails[0].ispe(), Some((96, 96)));
    assert_eq!(t.ispe(), Some((256, 256)));

    let e = build_primary_graph(&load("still-image-with-exif")).unwrap();
    assert_eq!(e.metadata.len(), 1);
    assert_eq!(e.metadata[0].item_type, *b"Exif");

    let one = build_primary_graph(&load("single-image-1x1")).unwrap();
    assert_eq!(one.output_size().unwrap(), (1, 1));
    assert_eq!(one.reconstructed_size().unwrap(), (64, 64));
}

#[test]
fn corpus_is_miaf_conformant() {
    for (root, bundle) in all_bundles() {
        let f = HeifFile::parse(&fixture_bytes(&root, bundle)).unwrap();
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        // The corpus was written by a MIAF-aware producer; the general
        // requirements hold for every bundle. (The 1×1 bundle's grid /
        // tile rules do not apply — it has no grid.)
        assert!(rep.is_conformant(), "{bundle}: {:#?}", rep.violations);
        // Profile-level: HEVC Basic only admits 4:2:0 8-bit Main /
        // Main Still Picture; the corpus' 10-bit / mono / 4:4:4 bundles
        // must trip exactly the codec clause and nothing else.
        let basic = check(&f, MiafProfile::HevcBasic).unwrap();
        let codec_only = basic.violations.iter().all(|v| v.clause == "A.3.2");
        assert!(codec_only, "{bundle}: {:#?}", basic.violations);
        let expect_basic_ok = !matches!(
            bundle,
            "still-10bit-main10"
                | "still-monochrome"
                | "still-yuv444"
                | "still-image-with-alpha"
                | "still-image-overlay"
        );
        assert_eq!(
            basic.is_conformant(),
            expect_basic_ok,
            "{bundle}: {:#?}",
            basic.violations
        );
        let extended = check(&f, MiafProfile::HevcExtended).unwrap();
        assert!(
            extended.is_conformant(),
            "{bundle}: {:#?}",
            extended.violations
        );
        assert!(
            MiafProfile::declared_by(&f).contains(&MiafProfile::Miaf)
                || bundle == "single-image-1x1"
        );
    }
}
