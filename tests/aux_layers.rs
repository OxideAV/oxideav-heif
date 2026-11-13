//! An HEVC access unit that carries an alpha auxiliary layer next to
//! its primary picture (`tests/fixtures/layered/hevc_alpha_128x96.mov`,
//! a black-box producer's one-frame movie: layer 0 the 4:2:0 picture,
//! layer 1 the alpha plane) wrapped by this crate's writer in the HEIF
//! shapes that carry such a bitstream.
#![cfg(feature = "registry")]

mod common;

use oxideav_heif::decode::{ItemDecoder, decode_item, decode_primary};
use oxideav_heif::hvcc::{NalArray, nal_layer_id};
use oxideav_heif::lhvc::{
    LayerDependency, LhevcConfig, OperatingPoint, OperatingPointLayer, OperatingPointPtl,
    OperatingPoints,
};
use oxideav_heif::meta::{ITEM_TYPE_HVC1, ITEM_TYPE_LHV1};
use oxideav_heif::props::{AuxC, Colr, Ispe, Lsel, Pixi, Property};
use oxideav_heif::{HeifFile, HeifWriter, HevcConfig};

const MOV: &str = "tests/fixtures/layered/hevc_alpha_128x96.mov";
const W: u32 = 128;
const H: u32 = 96;

/// The movie's `hvcC` split by layer (the base record keeps the
/// `nuh_layer_id` 0 parameter sets, the `lhvC` takes the others), and
/// its one access unit.
fn source() -> (HevcConfig, LhevcConfig, Vec<u8>) {
    let bytes = std::fs::read(MOV).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let mv = oxideav_heif::sequence::parse_movie(&f).unwrap().unwrap();
    let t = &mv.tracks[0];
    let e = &t.sample_entries[0];
    let s = &t.samples[0];
    let au = bytes[s.offset as usize..(s.offset + s.size as u64) as usize].to_vec();
    let all = e.hvcc.clone().unwrap();
    let split = |upper: bool| -> Vec<NalArray> {
        all.arrays
            .iter()
            .filter_map(|a| {
                let units: Vec<Vec<u8>> = a
                    .nal_units
                    .iter()
                    .filter(|n| (nal_layer_id(n).unwrap_or(0) > 0) == upper)
                    .cloned()
                    .collect();
                (!units.is_empty())
                    .then(|| NalArray::new(a.complete, false, a.nal_unit_type, units))
            })
            .collect()
    };
    let mut base = all.clone();
    base.arrays = split(false);
    base.raw = base.serialize();
    let mut lhvc = LhevcConfig::new(
        1,
        all.min_spatial_segmentation_idc,
        all.parallelism_type,
        all.num_temporal_layers,
        all.temporal_id_nested,
        all.length_size,
        split(true),
        Vec::new(),
    );
    lhvc.raw = lhvc.serialize();
    (base, lhvc, au)
}

/// An `oinf` for the two-layer stream: output layer set 0 is the
/// primary layer, set 1 the layers `outputs_of_set_1` (all output).
/// One scalability dimension (`scalability_mask` bit 12 —
/// `scalability_mask_flag[3]`, ISO/IEC 14496-15 §9.6.2.3: the most
/// significant bit is flag 0), identifier 1 on the second layer.
fn oinf_for(hvcc: &HevcConfig, outputs_of_set_1: &[u8]) -> OperatingPoints {
    let op = |ols: u16, layers: &[u8]| {
        OperatingPoint::new(
            ols,
            0,
            layers
                .iter()
                .map(|id| OperatingPointLayer::new(1, *id, true, false))
                .collect(),
            (W as u16, H as u16),
            (W as u16, H as u16),
            1,
            0,
            None,
            None,
        )
    };
    OperatingPoints::new(
        1 << 12,
        vec![OperatingPointPtl::new(
            0,
            false,
            hvcc.general_profile_idc,
            hvcc.general_profile_compatibility_flags,
            hvcc.general_constraint_indicator_flags,
            hvcc.general_level_idc,
        )],
        vec![op(0, &[0]), op(1, outputs_of_set_1)],
        vec![
            LayerDependency::new(0, vec![], vec![(12, 0)]),
            LayerDependency::new(1, vec![], vec![(12, 1)]),
        ],
        Vec::new(),
    )
}

fn lhv1_props(
    hvcc: &HevcConfig,
    lhvc: &LhevcConfig,
    outputs_of_set_1: &[u8],
    lsel: Option<u16>,
) -> Vec<(Property, bool)> {
    let mut v = vec![
        (Property::HvcC(hvcc.clone()), true),
        (Property::LhvC(lhvc.clone()), true),
        (Property::Oinf(oinf_for(hvcc, outputs_of_set_1)), false),
        (Property::Tols(1), true),
        (Property::Ispe(Ispe::new(W, H)), false),
        (Property::Pixi(Pixi::new(vec![8, 8, 8])), false),
    ];
    if let Some(l) = lsel {
        v.push((Property::Lsel(Lsel::new(l)), true));
    }
    v
}

/// FNV-1a 64 of the black-box decoder's `yuv420p` planes of the movie
/// (it decodes the primary layer).
const PRIMARY: u64 = 0x3591_5023_aab6_45e7;

fn oracle_primary() -> u64 {
    if !common::have_binary("ffmpeg") {
        return PRIMARY;
    }
    let raw = common::scratch_dir("aux_layers").join("primary.yuv");
    let ok = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i", MOV])
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg(&raw)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        common::fnv1a64(&std::fs::read(&raw).unwrap())
    } else {
        eprintln!("ffmpeg could not decode the movie; using the pinned value");
        PRIMARY
    }
}

/// The shape a layered bitstream with an alpha layer takes in a HEIF
/// file (B.2.2.1.2, B.2.4): the master is an `hvc1` item, whose readers
/// ignore the NAL units above the base layer, and the alpha plane an
/// `lhv1` auxiliary item over the same access unit (`auxC`
/// `urn:mpeg:hevc:2015:auxid:1`, `lsel` selecting the alpha layer).
/// The master is the black-box decoder's picture; its alpha plane is
/// the producer's source ramp.
#[test]
fn alpha_layer_item_is_the_alpha_plane_of_its_master() {
    let (hvcc, lhvc, au) = source();
    let mut w = HeifWriter::new();
    let master = w.add_coded_item(
        ITEM_TYPE_HVC1,
        au.clone(),
        vec![
            (Property::HvcC(hvcc.clone()), true),
            (Property::Ispe(Ispe::new(W, H)), false),
            (Property::Pixi(Pixi::new(vec![8, 8, 8])), false),
            (Property::Colr(Colr::MIAF_DEFAULT), false),
        ],
    );
    let mut props = lhv1_props(&hvcc, &lhvc, &[1], Some(1));
    props.push((
        Property::AuxC(AuxC::new("urn:mpeg:hevc:2015:auxid:1".into(), Vec::new())),
        true,
    ));
    let alpha = w.add_alpha(master, ITEM_TYPE_LHV1, au, props, false);
    w.set_primary(master);
    let bytes = w.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.item_id, master);
    assert_eq!((img.width(), img.height()), (W, H));
    assert!(img.frame.format.has_alpha);
    let t = img.frame.tight();
    let colour: Vec<u8> = t.planes[..3]
        .iter()
        .flat_map(|p| p.data.iter().copied())
        .collect();
    let primary = oracle_primary();
    eprintln!("primary {primary:#018x}");
    assert_eq!(common::fnv1a64(&colour), primary, "primary layer");
    // The alpha plane against the source ramp `255 * x / 127`.
    let a = &t.planes[3].data;
    let (mut worst, mut sum) = (0i32, 0i64);
    for y in 0..H as usize {
        for x in 0..W as usize {
            let d = (i32::from(a[y * W as usize + x]) - (255 * x as i32) / 127).abs();
            worst = worst.max(d);
            sum += i64::from(d);
        }
    }
    eprintln!(
        "alpha layer vs the source ramp: max {worst}, mean {:.2}",
        sum as f64 / f64::from(W * H)
    );
    assert!(worst <= 2, "alpha plane off the ramp by {worst}");
    // The auxiliary item on its own is the alpha layer's picture.
    let aux = decode_item(&f, alpha, ItemDecoder::direct()).unwrap();
    assert_eq!(aux.frame.tight().planes[0].data, *a);
}

/// An `lhv1` item whose `oinf` names both layers as output layers of
/// its `tols` set, over a codec whose output layer set selects the
/// highest layer alone (a VPS under the default "highest layer only"
/// rule with an auxiliary layer on top): the layer set is decoded in
/// full and both pictures come back, the primary first.
#[test]
fn layer_set_supplies_the_output_layers_the_output_layer_set_omits() {
    let (hvcc, lhvc, au) = source();
    let mut w = HeifWriter::new();
    let id = w.add_coded_item(ITEM_TYPE_LHV1, au, lhv1_props(&hvcc, &lhvc, &[0, 1], None));
    w.set_primary(id);
    let bytes = w.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let node = oxideav_heif::derived::build_primary_graph(&f).unwrap();
    let reg = common::stub::registry_highest_only();
    let layers = ItemDecoder::with_registry(&reg)
        .decode_coded_layers(&f, &node)
        .unwrap();
    assert_eq!(
        layers.iter().map(|l| l.layer_id).collect::<Vec<_>>(),
        vec![0, 1]
    );
    for l in &layers {
        assert_eq!((l.frame.width, l.frame.height), (W, H));
        assert!(common::stub::is_flat(&l.frame), "layer {}", l.layer_id);
    }
    let img = decode_primary(&f, ItemDecoder::with_registry(&reg)).unwrap();
    assert_eq!(img.layers.len(), 2);
    // The real bitstream's layer set 1 holds the alpha layer alone, so
    // the layers this `oinf` names are not there to take: the typed
    // refusal stays.
    let e = decode_primary(&f, ItemDecoder::direct()).unwrap_err();
    assert!(e.to_string().contains("1 of the 2 output layers"), "{e}");
}
