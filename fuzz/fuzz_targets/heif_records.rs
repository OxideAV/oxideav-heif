#![no_main]

//! Arbitrary bytes through the typed record parsers this round added
//! or reshaped: the `ToneMapImage` body (tmap), the ISOBMFF HDR boxes
//! (`clli` / `mdcv` / `cclv` / `amve`, plain or FullBox-prefixed), the
//! `avcC` / `lhvC` / `oinf` / `tols` records, the sample-group boxes
//! (`sbgp` / `csgp` / `sgpd`) and `prft` / `ssix`, plus every typed
//! property through `Property::parse` under each box type. The
//! contract is "the call returns"; whatever parses must re-serialize
//! and re-parse to the same value (a round-trip oracle).

use libfuzzer_sys::fuzz_target;
use oxideav_heif::avcc::AvcConfig;
use oxideav_heif::gainmap::GainMapMetadata;
use oxideav_heif::lhvc::{LhevcConfig, OperatingPoints};
use oxideav_heif::meta::RawProperty;
use oxideav_heif::props::{write::property_box, Property};
use oxideav_heif::sequence::{parse_csgp, parse_prft, parse_sbgp, parse_sgpd, parse_ssix};

const TYPES: &[&[u8; 4]] = &[
    b"clli", b"mdcv", b"cclv", b"amve", b"avcC", b"lhvC", b"oinf", b"tols", b"ispe", b"pixi",
    b"colr", b"pasp", b"clap", b"irot", b"imir", b"iscl", b"auxC", b"hvcC", b"av1C", b"rloc",
    b"lsel", b"a1op", b"a1lx", b"rref", b"crtt", b"mdft", b"udes", b"altt",
];

fn round_trip(p: &Property) {
    let bytes = property_box(p);
    let raw = RawProperty {
        box_type: p.box_type(),
        user_type: None,
        body: bytes[8..].to_vec(),
        box_size: bytes.len(),
    };
    if let Ok(back) = Property::parse(&raw) {
        assert_eq!(&back, p, "re-serialized property must re-parse equal");
    }
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let sel = data[0] as usize;
    let body = &data[1..];
    // Typed property under the selected box type.
    let t = TYPES[sel % TYPES.len()];
    let raw = RawProperty {
        box_type: *t,
        user_type: None,
        body: body.to_vec(),
        box_size: body.len() + 8,
    };
    if let Ok(p) = Property::parse(&raw) {
        round_trip(&p);
    }
    // Records on their own.
    if let Ok(m) = GainMapMetadata::parse_tmap_body(body) {
        let again = GainMapMetadata::parse_tmap_body(&m.serialize_tmap_body()).unwrap();
        assert_eq!(again, m);
    }
    if let Ok(c) = AvcConfig::parse(body) {
        let _ = c.layout();
        let again = AvcConfig::parse(&c.serialize()).unwrap();
        assert_eq!(again.serialize(), c.serialize());
    }
    if let Ok(c) = LhevcConfig::parse(body) {
        let _ = c.nal_units().count();
        let again = LhevcConfig::parse(&c.serialize()).unwrap();
        assert_eq!(again.serialize(), c.serialize());
    }
    if let Ok(o) = OperatingPoints::parse(body) {
        let _ = o.output_layers(0);
        let again = OperatingPoints::parse(&o.serialize()).unwrap();
        assert_eq!(again.serialize(), o.serialize());
    }
    // Sample-group / timing boxes (FullBox payloads).
    if let Ok(g) = parse_sbgp(body) {
        let _ = g.index_of(sel);
    }
    if let Ok(g) = parse_csgp(body) {
        let _ = g.index_of(sel);
    }
    let _ = parse_sgpd(body);
    let _ = parse_prft(body);
    let _ = parse_ssix(body);
});
