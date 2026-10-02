//! L-HEVC (`lhv1`) items with enhancement layers through oxideav-h265's
//! multi-layer decoder: an MV-HEVC stereo access unit from a black-box
//! producer (`tests/fixtures/layered/`) wrapped by this crate's writer
//! as the HEIF stereo-pair shape (two `lhv1` items with `lsel` 0 / 1 in a
//! `ster` group) and as a bare two-output-layer item; both views pinned
//! to the black-box decoder's per-view output.
#![cfg(feature = "registry")]

mod common;

use std::io::Cursor;

use oxideav_core::{Frame, RuntimeContext};
use oxideav_heif::decode::{decode_item, decode_primary, ItemDecoder};
use oxideav_heif::lhvc::{
    LayerDependency, LhevcConfig, OperatingPoint, OperatingPointLayer, OperatingPointPtl,
    OperatingPoints,
};
use oxideav_heif::meta::ITEM_TYPE_LHV1;
use oxideav_heif::miaf::{check, MiafProfile};
use oxideav_heif::props::{Colr, Ispe, Lsel, Pixi, Property};
use oxideav_heif::{HeifFile, HeifFrame, HeifWriter, HevcConfig};

const MOV: &str = "tests/fixtures/layered/mvhevc_960x960_3f.mov";
/// FNV-1a 64 of the black-box decoder's `yuv420p` planes of access unit
/// 0, view 0 / view 1 (`ffmpeg -view_ids N -frames:v 1 -f rawvideo`).
const VIEW0: u64 = 0x5e6f_0def_9e4d_8c8b;
const VIEW1: u64 = 0x05c6_86a6_c497_b28b;

fn fnv(frame: &HeifFrame) -> u64 {
    let t = frame.tight();
    let mut all = Vec::new();
    for p in &t.planes {
        all.extend_from_slice(&p.data);
    }
    common::fnv1a64(&all)
}

/// The base `hvcC`, the `lhvC` and access unit 0 of the movie's track.
fn source() -> (HevcConfig, LhevcConfig, Vec<u8>) {
    let bytes = std::fs::read(MOV).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let mv = oxideav_heif::sequence::parse_movie(&f).unwrap().unwrap();
    let t = &mv.tracks[0];
    let e = &t.sample_entries[0];
    let s = &t.samples[0];
    let au = bytes[s.offset as usize..(s.offset + s.size as u64) as usize].to_vec();
    (e.hvcc.clone().unwrap(), e.lhvc.clone().unwrap(), au)
}

/// An `oinf` for a two-view stream: output layer set 0 = the base, set
/// 1 = both layers output (HEIF B.2.3.3).
fn oinf_for(hvcc: &HevcConfig) -> OperatingPoints {
    let layer = |id: u8, out: bool| OperatingPointLayer::new(1, id, out, false);
    let op = |ols: u16, layers: Vec<OperatingPointLayer>| {
        OperatingPoint::new(ols, 0, layers, (960, 960), (960, 960), 1, 0, None, None)
    };
    OperatingPoints::new(
        0b1_0000, // view order index dimension
        vec![OperatingPointPtl::new(
            0,
            false,
            hvcc.general_profile_idc,
            hvcc.general_profile_compatibility_flags,
            hvcc.general_constraint_indicator_flags,
            hvcc.general_level_idc,
        )],
        vec![
            op(0, vec![layer(0, true)]),
            op(1, vec![layer(0, true), layer(1, true)]),
        ],
        vec![
            LayerDependency::new(0, vec![], vec![(4, 0)]),
            LayerDependency::new(1, vec![0], vec![(4, 1)]),
        ],
        Vec::new(),
    )
}

fn lhv1_props(hvcc: &HevcConfig, lhvc: &LhevcConfig, lsel: Option<u16>) -> Vec<(Property, bool)> {
    let mut v = vec![
        (Property::HvcC(hvcc.clone()), true),
        (Property::LhvC(lhvc.clone()), true),
        (Property::Oinf(oinf_for(hvcc)), false),
        (Property::Tols(1), true),
        (Property::Ispe(Ispe::new(960, 960)), false),
        (Property::Pixi(Pixi::new(vec![8, 8, 8])), false),
        (Property::Colr(Colr::MIAF_DEFAULT), false),
    ];
    if let Some(l) = lsel {
        v.push((Property::Lsel(Lsel::new(l)), true));
    }
    v
}

/// The stereo-pair file: two `lhv1` items over the same access unit
/// (`lsel` 0 and 1), a `ster` group, the left view primary.
fn stereo_file() -> Vec<u8> {
    let (hvcc, lhvc, au) = source();
    let mut w = HeifWriter::new();
    let left = w.add_coded_item(
        ITEM_TYPE_LHV1,
        au.clone(),
        lhv1_props(&hvcc, &lhvc, Some(0)),
    );
    let right = w.add_coded_item(ITEM_TYPE_LHV1, au, lhv1_props(&hvcc, &lhvc, Some(1)));
    w.set_primary(left);
    w.add_entity_group(*b"ster", 10, vec![left, right]);
    w.write_to_vec().unwrap()
}

/// The black-box decoder's per-view planes, re-derived when it is
/// present (else the pinned values).
fn oracle_views() -> (u64, u64) {
    if !common::have_binary("ffmpeg") {
        return (VIEW0, VIEW1);
    }
    let dir = common::scratch_dir("layered");
    let mut out = [VIEW0, VIEW1];
    for (v, slot) in out.iter_mut().enumerate() {
        let raw = dir.join(format!("view{v}.yuv"));
        let ok = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-y", "-view_ids"])
            .arg(v.to_string())
            .args([
                "-i",
                MOV,
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&raw)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            *slot = common::fnv1a64(&std::fs::read(&raw).unwrap());
        } else {
            eprintln!("ffmpeg could not decode view {v}; using the pinned value");
        }
    }
    (out[0], out[1])
}

/// Each `lsel` item decodes to its view, byte-exact against the
/// black-box decoder's per-view output; the file is MIAF-conformant.
#[test]
fn stereo_pair_items_decode_each_view_byte_exact() {
    let (view0, view1) = oracle_views();
    assert_eq!((view0, view1), (VIEW0, VIEW1), "black-box oracle moved");
    let bytes = stereo_file();
    let f = HeifFile::parse(&bytes).unwrap();
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let meta = f.meta().unwrap();
    let pair = meta.entity_groups[0].stereo_pair().unwrap();
    let left = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(left.item_id, pair.0);
    assert_eq!((left.width(), left.height()), (960, 960));
    assert!(left.layers.is_empty(), "lsel: one image");
    assert_eq!(fnv(&left.frame), view0, "left view");
    let right = decode_item(&f, pair.1, ItemDecoder::direct()).unwrap();
    assert_eq!(fnv(&right.frame), view1, "right view");
    assert_ne!(view0, view1);
    // Third-party readers (reported, not asserted: none of them
    // documents lhv1 support).
    let dir = common::scratch_dir("layered");
    let path = dir.join("stereo.heic");
    std::fs::write(&path, &bytes).unwrap();
    for (bin, args) in [("heif-info", vec![]), ("sips", vec!["-g", "pixelWidth"])] {
        if !common::have_binary(bin) {
            continue;
        }
        let out = std::process::Command::new(bin)
            .args(&args)
            .arg(&path)
            .output()
            .unwrap();
        eprintln!(
            "{bin}: exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .chain(String::from_utf8_lossy(&out.stderr).lines())
                .take(3)
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
}

/// An `lhv1` item without `lsel` whose output layer set has two output
/// layers yields both (base first) as `DecodedImage::layers`, each the
/// black-box view; the base-layer fallback keeps the base only.
#[test]
fn two_output_layer_item_yields_both_views() {
    let (hvcc, lhvc, au) = source();
    let mut w = HeifWriter::new();
    let id = w.add_coded_item(ITEM_TYPE_LHV1, au, lhv1_props(&hvcc, &lhvc, None));
    w.set_primary(id);
    let bytes = w.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.layers.len(), 2);
    assert_eq!(img.layers[0].layer_id, 0);
    assert_eq!(img.layers[1].layer_id, 1);
    assert_eq!(fnv(&img.frame), VIEW0);
    assert_eq!(fnv(&img.layers[0].frame), VIEW0);
    assert_eq!(fnv(&img.layers[1].frame), VIEW1);
    let base = decode_primary(&f, ItemDecoder::direct().base_layer_fallback()).unwrap();
    assert!(base.layers.is_empty());
    assert_eq!(fnv(&base.frame), VIEW0);
    // Through the framework: two tagged frames, layers announced.
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let mut demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let still = demuxer.streams()[0].clone();
    assert_eq!(
        still
            .params
            .layers
            .iter()
            .map(|l| l.layer_id)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    let pkt = demuxer.next_packet().unwrap();
    let mut dec = ctx.codecs.first_decoder(&still.params).unwrap();
    dec.send_packet(&pkt).unwrap();
    let mut layers = Vec::new();
    while let Ok(Frame::Video(v)) = dec.receive_frame() {
        layers.push(v.layer().map(|l| l.layer_id));
    }
    assert_eq!(layers, vec![Some(0), Some(1)]);
}

/// The stereo pair through the framework: the still stream announces
/// two views and the decoder emits both, tagged view 0 (left) / 1
/// (right) with their `lsel` layers.
#[test]
fn stereo_pair_streams_both_views_through_the_framework() {
    let bytes = stereo_file();
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let mut demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes.clone())), &ctx.codecs)
        .unwrap();
    let still = demuxer.streams()[0].clone();
    assert_eq!(
        still
            .params
            .layers
            .iter()
            .map(|l| (l.layer_id, l.view_id))
            .collect::<Vec<_>>(),
        vec![(0, Some(0)), (1, Some(1))]
    );
    let pkt = demuxer.next_packet().unwrap();
    let mut dec = ctx.codecs.first_decoder(&still.params).unwrap();
    dec.send_packet(&pkt).unwrap();
    let mut views = Vec::new();
    while let Ok(Frame::Video(v)) = dec.receive_frame() {
        let tag = v.layer().unwrap();
        let mut all = Vec::new();
        for p in v.image_planes() {
            all.extend_from_slice(&p.data);
        }
        views.push((tag.layer_id, tag.view_id, common::fnv1a64(&all)));
    }
    assert_eq!(views, vec![(0, Some(0), VIEW0), (1, Some(1), VIEW1)]);
}
