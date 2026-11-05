//! `sato` pixel transform derived image items (ISO/IEC 23008-12 4th
//! edition working draft §6.6.2.6): the Annex J.9 bit-depth extensions
//! built from lossless HEVC items, written by `HeifWriter`, decoded
//! sample-exact, predicted by the layout layer, checked by
//! `miaf::check`, re-muxed; the limited-range clamp; the refusals.
#![cfg(feature = "registry")]

use oxideav_heif::decode::{ItemDecoder, decode_primary};
use oxideav_heif::encode::encode_hevc_picture;
use oxideav_heif::image::Chroma;
use oxideav_heif::miaf::{MiafProfile, check};
use oxideav_heif::pixel_transform::{PixelToken, PixelTransform};
use oxideav_heif::props::{Colr, Ispe, Pixi, Property};
use oxideav_heif::{HeifError, HeifFile, HeifFrame, HeifPixelFormat, HeifWriter};

const FULL: Colr = Colr::Nclx {
    primaries: 1,
    transfer: 13,
    matrix: 6,
    full_range: true,
};

/// A deep 4:2:0 picture whose samples use every bit.
fn deep_picture(w: u32, h: u32, depth: u8) -> HeifFrame {
    let fmt = HeifPixelFormat::new(Chroma::Yuv420, depth, false).unwrap();
    let mut f = HeifFrame::zeroed(w, h, fmt).unwrap();
    let max = (1u32 << depth) - 1;
    for p in 0..3 {
        let (pw, ph) = f.plane_dims(p);
        for y in 0..ph {
            for x in 0..pw {
                let v = (x.wrapping_mul(2654435761) ^ (y * 40503) ^ (p as u32 * 977)) % (max + 1);
                f.set_sample(p, x, y, v as u16);
            }
        }
    }
    f
}

/// `src` with every sample mapped through `map`, at `depth` bits.
fn map_frame(src: &HeifFrame, depth: u8, map: impl Fn(u16) -> u16) -> HeifFrame {
    let fmt = HeifPixelFormat::new(src.format.chroma, depth, false).unwrap();
    let mut f = HeifFrame::zeroed(src.width, src.height, fmt).unwrap();
    for p in 0..3 {
        let (pw, ph) = f.plane_dims(p);
        for y in 0..ph {
            for x in 0..pw {
                f.set_sample(p, x, y, map(src.sample(p, x, y)));
            }
        }
    }
    f
}

/// A lossless HEVC item of `f` with pixi / ispe / `colr`.
fn add_item(w: &mut HeifWriter, f: &HeifFrame, colr: &Colr) -> u32 {
    let pic = encode_hevc_picture(f, "pcm", 0).unwrap();
    let d = f.format.bit_depth;
    w.add_coded_item(
        *b"hvc1",
        pic.data,
        vec![
            (pic.config, true),
            (Property::Ispe(Ispe::new(f.width, f.height)), false),
            (Property::Pixi(Pixi::new(vec![d, d, d])), false),
            (Property::Colr(colr.clone()), false),
        ],
    )
}

fn sato_props(w: u32, h: u32, depth: u8, colr: &Colr) -> Vec<(Property, bool)> {
    vec![
        (Property::Ispe(Ispe::new(w, h)), false),
        (Property::Pixi(Pixi::new(vec![depth, depth, depth])), false),
        (Property::Colr(colr.clone()), false),
    ]
}

/// J.9.1 suffix extension: a 16-bit picture as its 8 most significant
/// bits (lossless) and its 8 least significant bits, `256 × in₁ + in₂`.
fn suffix_file(src: &HeifFrame) -> Vec<u8> {
    let hi = map_frame(src, 8, |v| v >> 8);
    let lo = map_frame(src, 8, |v| v & 0xff);
    let mut w = HeifWriter::new();
    let a = add_item(&mut w, &hi, &FULL);
    let b = add_item(&mut w, &lo, &FULL);
    w.set_hidden(a, true);
    w.set_hidden(b, true);
    use PixelToken::*;
    let t =
        PixelTransform::new(2, vec![Constant(256), Input(1), Product, Input(2), Sum], 2).unwrap();
    let s = w
        .add_pixel_transform(&[a, b], t, sato_props(src.width, src.height, 16, &FULL))
        .unwrap();
    w.set_primary(s);
    w.write_to_vec().unwrap()
}

#[test]
fn suffix_bit_depth_extension_is_sample_exact() {
    let src = deep_picture(96, 64, 16);
    let bytes = suffix_file(&src);
    let f = HeifFile::parse(&bytes).unwrap();
    let primary = f.primary_item().unwrap();
    assert_eq!(primary.item_type, *b"sato");
    // The body sits in idat as J.9.1 lists it.
    let body = f.item_data(primary.id).unwrap();
    assert_eq!(&body[..], &[2, 5, 0, 0, 0, 1, 0, 1, 130, 2, 128]);
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame.format.bit_depth, 16);
    assert_eq!(img.frame, src);
    // The layout layer predicts what the decode yields.
    let info = oxideav_heif::info(&bytes).unwrap();
    assert_eq!((info.width, info.height), (96, 64));
    let api = oxideav_heif::decode(&bytes).unwrap();
    assert_eq!(api.format, info.format);
    // Re-mux keeps the derivation.
    let out = HeifWriter::from_files(&[&bytes])
        .unwrap()
        .write_to_vec()
        .unwrap();
    let g = HeifFile::parse(&out).unwrap();
    assert_eq!(g.primary_item().unwrap().item_type, *b"sato");
    assert_eq!(
        decode_primary(&g, ItemDecoder::direct()).unwrap().frame,
        src
    );
}

/// J.9.2 residual extension: a 12-bit base and an 8-bit residual,
/// `16 × in₁ + in₂ − 128`, back to the 16-bit source.
#[test]
fn residual_bit_depth_extension_is_sample_exact() {
    let src = deep_picture(64, 48, 16);
    let base = map_frame(&src, 12, |v| v >> 4);
    let residual = map_frame(&src, 8, |v| (v & 15) + 128);
    let mut w = HeifWriter::new();
    let a = add_item(&mut w, &base, &FULL);
    let b = add_item(&mut w, &residual, &FULL);
    w.set_hidden(b, true);
    use PixelToken::*;
    let t = PixelTransform::new(
        2,
        vec![
            Constant(16),
            Input(1),
            Product,
            Input(2),
            Sum,
            Constant(128),
            Difference,
        ],
        2,
    )
    .unwrap();
    let s = w
        .add_pixel_transform(&[a, b], t, sato_props(64, 48, 16, &FULL))
        .unwrap();
    w.set_primary(s);
    let bytes = w.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert!(check(&f, MiafProfile::Miaf).unwrap().is_conformant());
    assert_eq!(
        decode_primary(&f, ItemDecoder::direct()).unwrap().frame,
        src
    );
}

/// Results are clamped to the item's pixi depth and, under a limited
/// range `nclx`, to the nominal range (16..=235 luma, 16..=240 chroma
/// at 8 bits).
#[test]
fn output_is_clamped_to_depth_and_range() {
    let src = deep_picture(32, 32, 8);
    let limited = Colr::Nclx {
        primaries: 1,
        transfer: 13,
        matrix: 6,
        full_range: false,
    };
    for (colr, lo, hi_y, hi_c) in [(FULL, 0, 255, 255), (limited, 16, 235, 240)] {
        let mut w = HeifWriter::new();
        let a = add_item(&mut w, &src, &colr);
        use PixelToken::*;
        // 2 × in − 100 leaves the 8-bit range on both sides.
        let t = PixelTransform::new(
            1,
            vec![Constant(2), Input(1), Product, Constant(100), Difference],
            1,
        )
        .unwrap();
        let s = w
            .add_pixel_transform(&[a], t, sato_props(32, 32, 8, &colr))
            .unwrap();
        w.set_primary(s);
        let f = HeifFile::parse(&w.write_to_vec().unwrap()).unwrap();
        let out = decode_primary(&f, ItemDecoder::direct()).unwrap().frame;
        for p in 0..3 {
            let hi = if p == 0 { hi_y } else { hi_c };
            let (pw, ph) = out.plane_dims(p);
            for y in 0..ph {
                for x in 0..pw {
                    let want = (2 * src.sample(p, x, y) as i64 - 100).clamp(lo, hi);
                    assert_eq!(out.sample(p, x, y) as i64, want, "plane {p} ({x},{y})");
                }
            }
        }
    }
}

/// An unrecognised version / reserved token makes the item ignorable
/// (`Unsupported`); a broken expression, mismatched inputs, or a
/// writer call without pixi / ispe are refused.
#[test]
fn pixel_transform_refusals() {
    let src = deep_picture(32, 32, 8);
    let small = deep_picture(16, 32, 8);
    let mut w = HeifWriter::new();
    let a = add_item(&mut w, &src, &FULL);
    let b = add_item(&mut w, &small, &FULL);
    use PixelToken::*;
    let t = PixelTransform::new(0, vec![Input(1), Input(2), Max], 2).unwrap();
    assert!(
        w.add_pixel_transform(&[a, b], t.clone(), vec![]).is_err(),
        "pixi + ispe required"
    );
    assert!(
        w.add_pixel_transform(&[a], t.clone(), sato_props(32, 32, 8, &FULL))
            .is_err()
    );
    let s = w
        .add_pixel_transform(&[a, b], t, sato_props(32, 32, 8, &FULL))
        .unwrap();
    w.set_primary(s);
    let bytes = w.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(
        rep.violations
            .iter()
            .any(|v| v.clause == "HEIF-WD 6.6.2.6.1" && v.item_id == Some(b)),
        "{:#?}",
        rep.violations
    );
    let e = decode_primary(&f, ItemDecoder::direct()).unwrap_err();
    assert!(matches!(e, HeifError::InvalidData(_)), "{e}");
    // A reserved token in the stored body: the item is ignored
    // (Unsupported), a stack underflow is invalid.
    let patch = |body: &[u8]| {
        let mut m = f.clone();
        let meta = m.meta.as_mut().unwrap();
        let idat = meta.idat.as_mut().unwrap();
        let at = idat.len() - 5;
        idat[at..].copy_from_slice(body);
        m
    };
    let e = decode_primary(&patch(&[0, 3, 1, 2, 200]), ItemDecoder::direct()).unwrap_err();
    assert!(matches!(e, HeifError::Unsupported(_)), "{e}");
    let e = decode_primary(&patch(&[0, 3, 1, 128, 2]), ItemDecoder::direct()).unwrap_err();
    assert!(matches!(e, HeifError::InvalidData(_)), "{e}");
}

/// The high-level sato carrier: a deep picture (unaligned, HEVC 4:2:0
/// at 16 bits; AV1 4:4:4 at 12 bits) coded losslessly as two hidden
/// 8-bit items + the J.9.1 transform decodes to the source exactly,
/// passes the MIAF checks, carries the colour on every item; alpha /
/// shallow pictures / other codecs are refused.
#[test]
fn encoder_carries_deep_pictures_as_a_pixel_transform() {
    use oxideav_heif::encode::{EncodeOptions, StillCodec, encode_pixel_transform_still};
    let mut o = EncodeOptions::default();
    o.icc_profile = Some(vec![7u8; 64]);
    let src = deep_picture(76, 42, 16);
    let bytes = encode_pixel_transform_still(&src, &o).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert_eq!(f.primary_item().unwrap().item_type, *b"sato");
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, src);
    assert_eq!(img.icc_profile.as_deref(), Some(&[7u8; 64][..]));
    let fmt = HeifPixelFormat::new(Chroma::Yuv444, 12, false).unwrap();
    let mut deep444 = HeifFrame::zeroed(64, 40, fmt).unwrap();
    for p in 0..3 {
        for y in 0..40 {
            for x in 0..64 {
                deep444.set_sample(p, x, y, ((x * 61 + y * 17 + p as u32 * 1000) % 4096) as u16);
            }
        }
    }
    o.codec = StillCodec::Av1;
    o.av1_quality = Some(50); // the carrier codes losslessly regardless
    let bytes = encode_pixel_transform_still(&deep444, &o).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert_eq!(
        decode_primary(&f, ItemDecoder::direct()).unwrap().frame,
        deep444
    );
    // Refusals.
    assert!(encode_pixel_transform_still(&deep_picture(16, 16, 8), &o).is_err());
    o.codec = StillCodec::Vvc;
    assert!(matches!(
        encode_pixel_transform_still(&src, &o),
        Err(HeifError::Unsupported(_))
    ));
}
