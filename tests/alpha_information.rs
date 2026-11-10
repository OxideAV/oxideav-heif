//! The draft alpha information box (`alpx`: the latest public draft of
//! ISO/IEC 14496-12 §12.11.5.2, what HEIF Amd 2:2026 O.4.6 entry 30
//! names `AlphaInformationProperty`) as an item property of an alpha
//! auxiliary and as a child of an alpha track's sample entry: the
//! opaque / transparent sample values and the premultiplication mode
//! are applied on decode, the default values change nothing.
#![cfg(feature = "registry")]

use std::time::Duration;

use oxideav_heif::decode::{ItemDecoder, decode_primary};
use oxideav_heif::encode::{encode_hevc_picture, to_yuv420_8};
use oxideav_heif::image::Chroma;
use oxideav_heif::props::{AlphaInformation, AlphaPremultiplication, Colr, Ispe, Pixi, Property};
use oxideav_heif::{
    HeifFile, HeifFrame, HeifPixelFormat, HeifWriter, SequenceAlphaTrack, SequenceSample,
    SequenceWriter, decode_all,
};

const W: u32 = 32;
const H: u32 = 32;

fn colour(seed: u32) -> HeifFrame {
    let mut f = HeifFrame::zeroed(
        W,
        H,
        HeifPixelFormat::new(Chroma::Yuv420, 8, false).unwrap(),
    )
    .unwrap();
    for y in 0..H {
        for x in 0..W {
            f.set_sample(0, x, y, ((x * 7 + y * 3 + seed * 11) % 256) as u16);
        }
    }
    f
}

/// Every sample value 0..=255 appears (32 × 32 = 4 × 256).
fn alpha_value(x: u32, y: u32, seed: u32) -> u16 {
    ((y * W + x + seed * 5) % 256) as u16
}

fn alpha(seed: u32) -> HeifFrame {
    let mut a =
        HeifFrame::zeroed(W, H, HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap()).unwrap();
    for y in 0..H {
        for x in 0..W {
            a.set_sample(0, x, y, alpha_value(x, y, seed));
        }
    }
    a
}

fn std_props(config: Property, channels: usize) -> Vec<(Property, bool)> {
    vec![
        (config, true),
        (Property::Ispe(Ispe::new(W, H)), false),
        (Property::Pixi(Pixi::new(vec![8; channels])), false),
        (Property::Colr(Colr::MIAF_DEFAULT), false),
    ]
}

fn still(info: Option<AlphaInformation>) -> Vec<u8> {
    let pic = encode_hevc_picture(&colour(0), "pcm", 0).unwrap();
    let apic = encode_hevc_picture(&to_yuv420_8(&alpha(0)).unwrap(), "pcm", 0).unwrap();
    let mut w = HeifWriter::new();
    let id = w.add_coded_item(*b"hvc1", pic.data, std_props(pic.config, 3));
    w.set_primary(id);
    let mut aprops = std_props(apic.config, 1);
    if let Some(info) = info {
        aprops.push((Property::Alpx(info), false));
    }
    w.add_alpha(id, *b"hvc1", apic.data, aprops, false);
    w.write_to_vec().unwrap()
}

/// The §6.9.1 alpha (0 transparent, 255 opaque) an `alpx` with integer
/// `opaque` / `transparent` values makes of a coded sample.
fn expected(v: u16, opaque: i32, transparent: i32) -> u16 {
    let t = (v as i32 - transparent) as f64 / (opaque - transparent) as f64;
    (t.clamp(0.0, 1.0) * 255.0 + 0.5) as u16
}

#[test]
fn alpha_item_property_sets_the_opaque_and_transparent_values() {
    // No property, and the draft's defaults spelled out: the plane as
    // coded, straight alpha.
    for info in [
        None,
        Some(AlphaInformation::integer(
            AlphaPremultiplication::Straight,
            255,
            0,
        )),
    ] {
        let bytes = still(info);
        let f = HeifFile::parse(&bytes).unwrap();
        let img = decode_primary(&f, ItemDecoder::default()).unwrap();
        assert!(!img.premultiplied_alpha);
        let ap = img.frame.format.alpha_plane().expect("alpha plane");
        for y in 0..H {
            for x in 0..W {
                assert_eq!(img.frame.sample(ap, x, y), alpha_value(x, y, 0));
            }
        }
    }
    // A narrower range, an inverted one, floating-point values; mode 1
    // and 2 both mean pre-multiplied colour.
    let cases = [
        (
            AlphaInformation::integer(AlphaPremultiplication::NonLinear, 200, 50),
            (200, 50),
            true,
        ),
        (
            AlphaInformation::integer(AlphaPremultiplication::Straight, 0, 255),
            (0, 255),
            false,
        ),
        (
            AlphaInformation::float(AlphaPremultiplication::Linear, 128.0, 16.0),
            (128, 16),
            true,
        ),
    ];
    for (info, (o, t), prem) in cases {
        let bytes = still(Some(info));
        let f = HeifFile::parse(&bytes).unwrap();
        // The property re-parses to the record that was written.
        let alpha_id = f.meta().unwrap().auxiliaries_of(1)[0];
        let props =
            oxideav_heif::props::ItemProperties::resolve(f.meta().unwrap(), alpha_id).unwrap();
        assert_eq!(props.alpha_information(), Some(info));
        let img = decode_primary(&f, ItemDecoder::default()).unwrap();
        assert_eq!(img.premultiplied_alpha, prem, "{info:?}");
        let ap = img.frame.format.alpha_plane().expect("alpha plane");
        for y in 0..H {
            for x in 0..W {
                assert_eq!(
                    img.frame.sample(ap, x, y),
                    expected(alpha_value(x, y, 0), o, t),
                    "{info:?} at ({x}, {y})"
                );
            }
        }
        // The colour planes are untouched.
        for y in 0..H {
            for x in 0..W {
                assert_eq!(img.frame.sample(0, x, y), colour(0).sample(0, x, y));
            }
        }
    }
}

#[test]
fn opacity_is_the_position_between_the_two_values() {
    let i = AlphaInformation::integer(AlphaPremultiplication::Straight, 200, 50);
    assert_eq!(i.opacity(50.0), 0.0);
    assert_eq!(i.opacity(10.0), 0.0);
    assert_eq!(i.opacity(200.0), 1.0);
    assert_eq!(i.opacity(250.0), 1.0);
    assert_eq!(i.opacity(125.0), 0.5);
    assert_eq!(
        (i.flags(), i.is_float(), i.is_premultiplied()),
        (0, false, false)
    );
    let f = AlphaInformation::float(AlphaPremultiplication::Linear, 1.0, 0.0);
    assert_eq!(
        (f.flags(), f.is_float(), f.is_premultiplied()),
        (6, true, true)
    );
    assert_eq!((f.opaque(), f.transparent()), (1.0, 0.0));
    // Equal values: a threshold.
    let e = AlphaInformation::integer(AlphaPremultiplication::Straight, 7, 7);
    assert_eq!((e.opacity(6.0), e.opacity(7.0)), (0.0, 1.0));
}

#[test]
fn box_syntax_is_the_drafts() {
    // FullBox('alpx', version 0, flags): 16-bit integers, or 32-bit
    // floats under flag 4.
    let int = oxideav_heif::props::write::property_box(&Property::Alpx(AlphaInformation::integer(
        AlphaPremultiplication::NonLinear,
        0x0123,
        0x0045,
    )));
    assert_eq!(
        int,
        [
            0, 0, 0, 16, b'a', b'l', b'p', b'x', 0, 0, 0, 1, 0x01, 0x23, 0x00, 0x45
        ]
    );
    let float = oxideav_heif::props::write::property_box(&Property::Alpx(AlphaInformation::float(
        AlphaPremultiplication::Straight,
        1.0,
        0.0,
    )));
    assert_eq!(
        float,
        [
            0, 0, 0, 20, b'a', b'l', b'p', b'x', 0, 0, 0, 4, 0x3f, 0x80, 0, 0, 0, 0, 0, 0
        ]
    );
    // Reserved flag bits survive a round trip; a truncated body and
    // another version do not parse as the property.
    let mut odd = int.clone();
    odd[9] = 0x80;
    let raw = |b: &[u8]| oxideav_heif::RawProperty::new(*b"alpx", None, b[8..].to_vec(), b.len());
    let p = Property::parse(&raw(&odd)).unwrap();
    assert_eq!(oxideav_heif::props::write::property_box(&p), odd);
    assert!(Property::parse(&raw(&int[..14])).is_err());
    let mut v1 = int.clone();
    v1[8] = 1;
    assert!(Property::parse(&raw(&v1)).is_err());
}

#[test]
fn alpha_track_sample_entry_carries_the_box() {
    let info = AlphaInformation::integer(AlphaPremultiplication::Straight, 200, 50);
    for with_info in [false, true] {
        let mut sw: Option<SequenceWriter> = None;
        let mut at: Option<SequenceAlphaTrack> = None;
        for i in 0..2u32 {
            let pic = encode_hevc_picture(&colour(i), "pcm", 0).unwrap();
            let apic = encode_hevc_picture(&to_yuv420_8(&alpha(i)).unwrap(), "pcm", 0).unwrap();
            sw.get_or_insert_with(|| {
                SequenceWriter::new(*b"hvc1", pic.config.clone(), W as u16, H as u16, 1000)
            })
            .push_sample(pic.data, 40, true);
            at.get_or_insert_with(|| {
                SequenceAlphaTrack::new(
                    *b"hvc1",
                    apic.config.clone(),
                    W as u16,
                    H as u16,
                    oxideav_heif::props::AUX_URN_ALPHA_HEVC.to_string(),
                    Vec::<SequenceSample>::new(),
                    if with_info {
                        vec![Property::Alpx(info)]
                    } else {
                        Vec::new()
                    },
                )
            })
            .push_sample(apic.data, 40, true);
        }
        let mut sw = sw.unwrap();
        sw.alpha = at;
        let bytes = sw.write_to_vec().unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        let mv = oxideav_heif::sequence::parse_movie(&f).unwrap().unwrap();
        let entry = mv.alpha_track_of(1).unwrap().primary_entry().unwrap();
        assert_eq!(entry.alpha_information(), with_info.then_some(info));
        let frames = decode_all(&bytes).unwrap();
        assert_eq!(frames.len(), 2);
        for (i, fr) in frames.iter().enumerate() {
            assert_eq!(fr.delay, Some(Duration::from_millis(40)));
            let img = &fr.image;
            let rgba = img.to_rgba8();
            for y in 0..H {
                for x in 0..W {
                    let v = alpha_value(x, y, i as u32);
                    let want = if with_info { expected(v, 200, 50) } else { v };
                    let got = rgba[((y * W + x) * 4 + 3) as usize] as u16;
                    assert_eq!(got, want, "frame {i} ({x}, {y}) info: {with_info}");
                }
            }
        }
    }
}
