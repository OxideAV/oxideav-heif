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
use oxideav_heif::vvcc::{CompactVvcConfig, SpsHead, VvcConfig};

const TYPES: &[&[u8; 4]] = &[
    b"clli", b"mdcv", b"cclv", b"amve", b"avcC", b"lhvC", b"oinf", b"tols", b"ispe", b"pixi",
    b"colr", b"pasp", b"clap", b"irot", b"imir", b"iscl", b"auxC", b"hvcC", b"av1C", b"rloc",
    b"lsel", b"a1op", b"a1lx", b"rref", b"crtt", b"mdft", b"udes", b"altt", b"reve", b"ndwt",
    b"cexg", b"dadj", b"stag", b"tilC",
];

fn round_trip(p: &Property) {
    let bytes = property_box(p);
    let raw = RawProperty::new(p.box_type(), None, bytes[8..].to_vec(), bytes.len());
    if let Ok(back) = Property::parse(&raw) {
        match p {
            // Decoder configuration records keep their input bytes in
            // `raw` (trailing bytes included) while the writer emits the
            // canonical record: the contract is a serialization fixed
            // point, not field equality with the non-canonical input.
            Property::HvcC(_)
            | Property::AvcC(_)
            | Property::LhvC(_)
            | Property::Av1C(_)
            | Property::Oinf(_) => {
                assert_eq!(
                    property_box(&back),
                    bytes,
                    "re-serialized record must be a fixed point"
                );
            }
            _ => assert_eq!(&back, p, "re-serialized property must re-parse equal"),
        }
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
    let raw = RawProperty::new(*t, None, body.to_vec(), body.len() + 8);
    if let Ok(p) = Property::parse(&raw) {
        round_trip(&p);
    }
    // Low-overhead `mini` box (Amd 2 Annex O): parse → serialize →
    // parse is a fixed point; the O.4 expansion parses as a HEIF file.
    if let Ok(m) = oxideav_heif::mini::MinimizedImage::parse(body) {
        if let Ok(b) = m.to_box() {
            let again = oxideav_heif::mini::MinimizedImage::parse(&b[8..]).unwrap();
            assert_eq!(again, m, "mini must round-trip");
        }
        let ft = oxideav_heif::FileType::new(
            *b"ftyp",
            *b"mif3",
            if sel & 1 == 1 {
                u32::from_be_bytes(*b"vvi3")
            } else {
                0
            },
            vec![],
        );
        if let Ok(eq) = m.equivalent_file(&ft) {
            let f = oxideav_heif::HeifFile::parse(&eq).expect("equivalent file parses");
            let _ = oxideav_heif::miaf::check(&f, oxideav_heif::MiafProfile::Miaf);
            let _ = oxideav_heif::derived::build_primary_graph(&f);
        }
    }
    // deti data reference (Amd 2 §6.11.5) + its offset table over the
    // same bytes.
    let dref = oxideav_heif::meta::DataReference::new(
        *b"deti",
        (sel >> 7) & 1 == 0,
        String::new(),
        String::new(),
        sel as u32,
        body.to_vec(),
    );
    if let Ok(d) = oxideav_heif::tiled::DataEntryTiledItem::parse(&dref) {
        let _ = d.tile_spans(body, (sel % 7) as u64);
    }
    // cfen body (Amd 1 §6.6.2.5.2): parse → serialize → parse.
    if let Ok(c) = oxideav_heif::derived::ColourFormatEnhancement::parse(body, sel % 5) {
        let again =
            oxideav_heif::derived::ColourFormatEnhancement::parse(&c.to_bytes(), sel % 5).unwrap();
        assert_eq!(again, c, "cfen must round-trip");
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
    if let Ok(c) = VvcConfig::parse(body) {
        let _ = c.sample_format();
        let _ = c.nal_count();
        // A DCI / OPI array codes exactly one NAL unit, so the first
        // serialization may drop surplus entries; from there on the
        // bytes are a fixed point.
        let first = c.serialize();
        let again = VvcConfig::parse(&first).unwrap();
        assert_eq!(again.serialize(), first);
        let _ = oxideav_heif::vvcc::access_unit_annex_b(&c, body, None);
    }
    if let Ok(c) = CompactVvcConfig::parse(body) {
        let _ = c.to_full(1, 8, 64, 64);
        let _ = c.item_data(body);
        let _ = c.strip_item_data(body);
        if let Ok(bytes) = c.serialize() {
            assert_eq!(CompactVvcConfig::parse(&bytes).unwrap(), c);
        }
    }
    if let Ok(h) = SpsHead::parse_rbsp(body) {
        let _ = h.cropped_size();
    }
    let _ = oxideav_heif::vvcc::vps_first_ptl(body);
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
