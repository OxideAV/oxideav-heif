//! A `grid` or `tili` item whose transformative chain starts with
//! rotations / mirrors is composed in its final orientation (the
//! tiles are written to their oriented places; round 488). The decoded
//! picture must be, sample for sample, the picture of the same file
//! written without transforms taken through the chain one operation at
//! a time — for both tile carriers, with and without alpha, at sizes
//! on and off the chroma grid, serial and under a thread budget.
#![cfg(feature = "registry")]

use oxideav_heif::HeifFile;
use oxideav_heif::compose::{apply_imir, apply_irot};
use oxideav_heif::decode::{ItemDecoder, decode_primary};
use oxideav_heif::encode::{EncodeOptions, TileCarrier, encode_still};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};
use oxideav_heif::props::{Imir, Irot, Property};

fn picture(w: u32, h: u32, alpha: bool) -> HeifFrame {
    let fmt = HeifPixelFormat::new(Chroma::Yuv420, 8, alpha).unwrap();
    let mut f = HeifFrame::zeroed(w, h, fmt).unwrap();
    for p in 0..fmt.plane_count() {
        let (pw, ph) = f.plane_dims(p);
        for y in 0..ph {
            for x in 0..pw {
                let v = (x * 7 + y * 13 + p as u32 * 41 + (x * y) % 29) % 256;
                f.set_sample(p, x, y, v as u16);
            }
        }
    }
    f
}

fn decode(bytes: &[u8], threads: usize) -> HeifFrame {
    let file = HeifFile::parse(bytes).unwrap();
    decode_primary(&file, ItemDecoder::direct().with_threads(threads))
        .unwrap()
        .frame
        .tight()
}

#[test]
fn oriented_tiles_equal_the_composed_picture_taken_through_the_chain() {
    // The chains the writer accepts (MIAF §7.3.6.7: `irot` before
    // `imir`, each at most once); arbitrary chains are pinned on the
    // canvas itself (`compose`'s unit test and the composition fuzz
    // target).
    let chains: [Vec<Property>; 6] = [
        vec![Property::Irot(Irot::new(1))],
        vec![Property::Irot(Irot::new(3)), Property::Imir(Imir::new(1))],
        vec![Property::Irot(Irot::new(2)), Property::Imir(Imir::new(0))],
        vec![Property::Irot(Irot::new(0))],
        vec![Property::Imir(Imir::new(1))],
        vec![Property::Irot(Irot::new(1)), Property::Imir(Imir::new(0))],
    ];
    for (w, h, alpha) in [(200u32, 136u32, false), (192, 128, true), (199, 135, false)] {
        let src = picture(w, h, alpha);
        for carrier in [TileCarrier::Grid, TileCarrier::Tiled] {
            let base = EncodeOptions::default()
                .with_hevc_mode("pcm".into())
                .with_grid_tile(Some(64))
                .with_tile_carrier(carrier);
            let plain = decode(&encode_still(&src, &base).unwrap(), 1);
            assert_eq!((plain.width, plain.height), (w, h));
            for chain in &chains {
                let mut want = plain.clone();
                for t in chain {
                    want = match t {
                        Property::Irot(r) => apply_irot(&want, r).unwrap(),
                        Property::Imir(m) => apply_imir(&want, m).unwrap(),
                        _ => unreachable!(),
                    };
                }
                let want = want.tight();
                let bytes =
                    encode_still(&src, &base.clone().with_transforms(chain.clone())).unwrap();
                for threads in [1usize, 4] {
                    let got = decode(&bytes, threads);
                    let tag =
                        format!("{w}x{h} alpha {alpha} {carrier:?} {chain:?} {threads} threads");
                    assert_eq!((got.width, got.height), (want.width, want.height), "{tag}");
                    assert_eq!(got.format, want.format, "{tag}");
                    assert_eq!(got.planes, want.planes, "{tag}");
                }
            }
        }
    }
}
