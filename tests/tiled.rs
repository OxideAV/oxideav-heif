//! `tili` tiled image items (ISO/IEC 23008-12:2025/Amd 2:2026 §6.11):
//! the writer packs coded tiles behind a `deti` offset table, the reader
//! composes them (in parallel under a thread budget) and crops the
//! padding to the `ispe`; empty tiles render neutral; external tiles
//! and hyperrectangles are typed refusals.
#![cfg(feature = "registry")]

mod common;

use std::io::Cursor;

use oxideav_core::{ExecutionContext, Frame, RuntimeContext};
use oxideav_heif::compose::{crop, GridCanvas};
use oxideav_heif::decode::{decode_primary, ItemDecoder};
use oxideav_heif::derived::GridDescriptor;
use oxideav_heif::encode::{encode_hevc_picture, pad_frame};
use oxideav_heif::image::Chroma;
use oxideav_heif::miaf::{check, MiafProfile};
use oxideav_heif::props::{Colr, Pixi, Property};
use oxideav_heif::{HeifFile, HeifFrame, HeifPixelFormat, HeifWriter};

fn picture(w: u32, h: u32) -> HeifFrame {
    let mut f = HeifFrame::zeroed(
        w,
        h,
        HeifPixelFormat::new(Chroma::Yuv420, 8, false).unwrap(),
    )
    .unwrap();
    for y in 0..h {
        for x in 0..w {
            f.set_sample(0, x, y, ((x * 3 + y * 5) % 256) as u16);
        }
    }
    let (cw, ch) = f.plane_dims(1);
    for y in 0..ch {
        for x in 0..cw {
            f.set_sample(1, x, y, ((x * 7 + 30) % 256) as u16);
            f.set_sample(2, x, y, ((y * 11 + 60) % 256) as u16);
        }
    }
    f
}

/// Cut `src` into `tile`-sized coded HEVC tiles (lossless), skipping
/// the indices in `empty`; returns the tiles, the decoder configuration
/// and the expected composition.
fn tiles_of(
    src: &HeifFrame,
    tile: u32,
    empty: &[usize],
) -> (Vec<Option<Vec<u8>>>, Property, HeifFrame) {
    let cols = src.width.div_ceil(tile);
    let rows = src.height.div_ceil(tile);
    let padded = pad_frame(src, cols * tile, rows * tile).unwrap();
    let mut tiles = Vec::new();
    let mut config = None;
    let desc = GridDescriptor {
        rows: rows as u16,
        columns: cols as u16,
        output_width: src.width,
        output_height: src.height,
    };
    let mut expected = GridCanvas::new(&desc);
    let mut blanks = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let i = (r * cols + c) as usize;
            let t = crop(&padded, c * tile, r * tile, tile, tile).unwrap();
            if empty.contains(&i) {
                tiles.push(None);
                blanks.push(i);
                continue;
            }
            let pic = encode_hevc_picture(&t, "pcm", 0).unwrap();
            config.get_or_insert(pic.config.clone());
            tiles.push(Some(pic.data));
            expected.place(i, &t).unwrap();
        }
    }
    for b in blanks {
        expected.place_blank(b).unwrap();
    }
    (tiles, config.unwrap(), expected.finish().unwrap())
}

fn tiled_file(src: &HeifFrame, tile: u32, empty: &[usize]) -> (Vec<u8>, HeifFrame) {
    let (tiles, config, expected) = tiles_of(src, tile, empty);
    let mut w = HeifWriter::new();
    let id = w
        .add_tiled_item(
            src.width,
            src.height,
            tile,
            tile,
            *b"hvc1",
            tiles,
            vec![(config, true)],
            vec![
                (
                    Property::Pixi(Pixi {
                        bits_per_channel: vec![8, 8, 8],
                    }),
                    false,
                ),
                (Property::Colr(Colr::MIAF_DEFAULT), false),
            ],
        )
        .unwrap();
    w.set_primary(id);
    (w.write_to_vec().unwrap(), expected)
}

/// A 200×136 picture in 64-px tiles (4×3, padded on the right and
/// bottom, one empty tile) round-trips through the writer and the
/// reader — serial and parallel — to the expected composition.
#[test]
fn tiled_item_round_trips_with_padding_and_an_empty_tile() {
    let src = picture(200, 136);
    let (bytes, expected) = tiled_file(&src, 64, &[5]);
    let f = HeifFile::parse(&bytes).unwrap();
    let meta = f.meta().unwrap();
    let primary = f.primary_item().unwrap();
    assert_eq!(primary.item_type, *b"tili");
    assert_eq!(meta.data_references.len(), 1);
    assert_eq!(meta.data_references[0].entry_type, *b"deti");
    assert!(meta.data_references[0].self_contained);
    let loc = meta.location(primary.id).unwrap();
    assert_eq!(loc.data_reference_index, 1);
    let props = oxideav_heif::props::ItemProperties::resolve(meta, primary.id).unwrap();
    let tilc = props.tilc().unwrap();
    assert_eq!((tilc.tile_width, tilc.tile_height), (64, 64));
    assert_eq!(tilc.tile_grid(200, 136), Some((4, 3)));
    let (ty, assoc) = tilc.in_file_tiles.as_ref().unwrap();
    assert_eq!(ty, b"hvc1");
    assert_eq!(assoc.len(), 1, "the hvcC of the tiles");
    assert!(assoc[0].0, "essential");
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!((img.width(), img.height()), (200, 136));
    assert_eq!(img.frame, expected);
    // The non-empty area is the source picture.
    for y in 0..136u32 {
        for x in 0..200u32 {
            let in_empty = (64..128).contains(&x) && (64..128).contains(&y);
            let want = if in_empty { 128 } else { src.sample(0, x, y) };
            assert_eq!(img.frame.sample(0, x, y), want, "({x},{y})");
        }
    }
    for threads in [2, 5] {
        let dec =
            ItemDecoder::direct().with_execution_context(&ExecutionContext::with_threads(threads));
        let par = decode_primary(&f, dec).unwrap();
        assert_eq!(par.frame, expected, "{threads} threads");
    }
    // Framework: the still stream predicts the geometry / layout and
    // the decoder emits the composition.
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let mut demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .unwrap();
    let still = demuxer.streams()[0].clone();
    assert_eq!(
        (still.params.width, still.params.height),
        (Some(200), Some(136))
    );
    assert_eq!(
        still.params.pixel_format,
        Some(oxideav_core::PixelFormat::YuvJ420P)
    );
    let pkt = demuxer.next_packet().unwrap();
    let mut dec = ctx.codecs.first_decoder(&still.params).unwrap();
    dec.send_packet(&pkt).unwrap();
    let Frame::Video(v) = dec.receive_frame().unwrap() else {
        panic!("video frame");
    };
    assert_eq!(v.image_planes()[0].data, expected.tight().planes[0].data);
}

/// Odd tile sizes promote to 4:4:4 like a grid; a tiled item without
/// an `ispe` or with external tiles is refused typed.
#[test]
fn tiled_item_edge_cases() {
    let src = picture(96, 80);
    let (bytes, expected) = tiled_file(&src, 32, &[]);
    let f = HeifFile::parse(&bytes).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, expected);
    assert_eq!(img.frame.format.chroma, Chroma::Yuv420);
    // External tiles: parse fine, compose refused.
    let mut m = f.clone();
    let dref = &mut m.meta.as_mut().unwrap().data_references[0];
    dref.flags |= 1 << 7;
    dref.self_contained = false;
    let mut payload = vec![0, 0, 0, 1]; // no_of_input_items (32-bit)
    payload.push(0); // directory_ID_flag 0
    payload.extend_from_slice(&0u64.to_be_bytes());
    payload.extend_from_slice(b"http://example.invalid/\0Rep\0$tileID$.heif\0");
    dref.payload = payload;
    let err = decode_primary(&m, ItemDecoder::direct()).unwrap_err();
    assert!(
        matches!(err, oxideav_heif::HeifError::Unsupported(_)),
        "{err}"
    );
    // Tile count mismatch in the writer.
    let mut w = HeifWriter::new();
    assert!(w
        .add_tiled_item(96, 80, 32, 32, *b"hvc1", vec![None; 5], vec![], vec![])
        .is_err());
}

/// A coded item whose `iloc` extents are the tiles of a `cexg` grid
/// (HEIF Amd 1:2025 §6.5.41) composes like a grid and crops to its
/// `ispe`; extent count / coverage mismatches are refused.
#[test]
fn constrained_extents_item_composes_its_extents() {
    let src = picture(120, 100);
    let (tiles, config, expected) = tiles_of(&src, 64, &[]);
    let tiles: Vec<Vec<u8>> = tiles.into_iter().map(Option::unwrap).collect();
    let mut w = HeifWriter::new();
    let id = w
        .add_constrained_extents_item(
            *b"hvc1",
            2,
            2,
            64,
            64,
            tiles.clone(),
            vec![
                (config.clone(), true),
                (
                    Property::Ispe(oxideav_heif::props::Ispe {
                        width: 120,
                        height: 100,
                    }),
                    false,
                ),
                (Property::Colr(Colr::MIAF_DEFAULT), false),
            ],
        )
        .unwrap();
    w.set_primary(id);
    let bytes = w.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let meta = f.meta().unwrap();
    assert_eq!(meta.location(id).unwrap().extents.len(), 4);
    assert_eq!(f.item_extents(id).unwrap().len(), 4);
    assert_eq!(f.item_extents(id).unwrap()[3], &tiles[3][..]);
    let props = oxideav_heif::props::ItemProperties::resolve(meta, id).unwrap();
    assert_eq!(props.cexg().map(|c| c.tile_count()), Some(4));
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    for threads in [1, 3] {
        let dec =
            ItemDecoder::direct().with_execution_context(&ExecutionContext::with_threads(threads));
        let img = decode_primary(&f, dec).unwrap();
        assert_eq!((img.width(), img.height()), (120, 100));
        assert_eq!(img.frame, expected, "{threads} threads");
    }
    // A cexg whose extent count disagrees with the iloc is refused.
    let mut broken = f.clone();
    let m = broken.meta.as_mut().unwrap();
    let cexg_index = m
        .properties
        .iter()
        .position(|p| &p.box_type == b"cexg")
        .unwrap();
    // rows_minus_one 2 -> 3 rows x 2 columns = 6 extents expected.
    m.properties[cexg_index].body[4..6].copy_from_slice(&2u16.to_be_bytes());
    let err = decode_primary(&broken, ItemDecoder::direct()).unwrap_err();
    assert!(err.to_string().contains("extents"), "{err}");
    assert!(w
        .add_constrained_extents_item(*b"hvc1", 2, 2, 64, 64, vec![Vec::new(); 3], vec![])
        .is_err());
}

/// `cfen` (HEIF Amd 1:2025 §6.6.2.5): three monochrome coded items — Y,
/// Cb, Cr at full resolution, or Cb / Cr at half resolution — compose
/// into one 4:4:4 / 4:2:0 picture exactly; with an identity-matrix
/// `colr` the channel ids read as R / G / B.
#[test]
fn colour_format_enhancement_composes_luma_planes() {
    use oxideav_heif::encode::to_yuv420_8;
    let mono = |w: u32, h: u32, seed: u32| {
        let mut f =
            HeifFrame::zeroed(w, h, HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap()).unwrap();
        for y in 0..h {
            for x in 0..w {
                f.set_sample(0, x, y, ((x * seed + y * 3 + seed * 17) % 256) as u16);
            }
        }
        f
    };
    for (label, chroma, matrix) in [
        ("444", Chroma::Yuv444, 6u16),
        ("420", Chroma::Yuv420, 6),
        ("rgb", Chroma::Yuv444, 0),
    ] {
        let (w, h) = (64u32, 48u32);
        let (cw, ch) = chroma.chroma_dims(w, h);
        let planes = [mono(w, h, 5), mono(cw, ch, 7), mono(cw, ch, 11)];
        let mut writer = HeifWriter::new();
        let mut ids = Vec::new();
        for p in &planes {
            let pic = encode_hevc_picture(
                &to_yuv420_8(
                    &pad_frame(p, p.width.div_ceil(16) * 16, p.height.div_ceil(16) * 16).unwrap(),
                )
                .unwrap(),
                "pcm",
                0,
            )
            .unwrap();
            let mut props = vec![
                (pic.config.clone(), true),
                (
                    Property::Ispe(oxideav_heif::props::Ispe {
                        width: pic.coded_width,
                        height: pic.coded_height,
                    }),
                    false,
                ),
                (
                    Property::Pixi(Pixi {
                        bits_per_channel: vec![8],
                    }),
                    false,
                ),
            ];
            if (pic.coded_width, pic.coded_height) != (p.width, p.height) {
                props.push((
                    Property::Clap(oxideav_heif::props::Clap::for_rect(
                        pic.coded_width,
                        pic.coded_height,
                        oxideav_heif::props::CropRect {
                            x: 0,
                            y: 0,
                            width: p.width,
                            height: p.height,
                        },
                    )),
                    true,
                ));
            }
            ids.push(writer.add_coded_item(*b"hvc1", pic.data, props));
        }
        let colr = Colr::Nclx {
            primaries: 1,
            transfer: 13,
            matrix,
            full_range: true,
        };
        // RGB: the three items are R, G, B (channel ids 2, 3, 4).
        let channels: Vec<(u32, u8)> = ids.iter().zip([2u8, 3, 4]).map(|(i, c)| (*i, c)).collect();
        let cfen = writer
            .add_colour_format_enhancement(
                &channels,
                vec![
                    (
                        Property::Ispe(oxideav_heif::props::Ispe {
                            width: w,
                            height: h,
                        }),
                        false,
                    ),
                    (
                        Property::Pixi(Pixi {
                            bits_per_channel: vec![8, 8, 8],
                        }),
                        false,
                    ),
                    (Property::Colr(colr.clone()), false),
                ],
            )
            .unwrap();
        writer.set_primary(cfen);
        let bytes = writer.write_to_vec().unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(
            rep.violations
                .iter()
                .all(|v| !v.clause.starts_with("HEIF-A1 6.6.2.5")),
            "{label}: {:#?}",
            rep.violations
        );
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        assert_eq!(img.frame.format.chroma, chroma, "{label}");
        let t = img.frame.tight();
        let order: [usize; 3] = if matrix == 0 { [1, 2, 0] } else { [0, 1, 2] };
        // RGB: R (item 0) is plane 2, G (item 1) plane 0, B (item 2) plane 1.
        for (item, plane) in
            [0usize, 1, 2]
                .into_iter()
                .zip(if matrix == 0 { [2usize, 0, 1] } else { order })
        {
            assert_eq!(
                t.planes[plane].data,
                planes[item].tight().planes[0].data,
                "{label}: item {item}"
            );
        }
        let node = oxideav_heif::derived::build_primary_graph(&f).unwrap();
        let (fmt, size) = oxideav_heif::demux::predict_output(&node).unwrap();
        assert_eq!((fmt.chroma, size), (chroma, (w, h)), "{label}: prediction");
    }
}
