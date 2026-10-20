//! `tili` tiled image items (ISO/IEC 23008-12:2025/Amd 2:2026 §6.11):
//! the writer packs coded tiles behind a `deti` offset table, the reader
//! composes them (in parallel under a thread budget) and crops the
//! padding to the `ispe`; empty tiles render neutral; external tiles
//! and hyperrectangles are typed refusals.
#![cfg(feature = "registry")]

mod common;

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecOptions, CodecParameters, Encoder, ExecutionContext, Frame, RuntimeContext,
};
use oxideav_heif::compose::{crop, GridCanvas};
use oxideav_heif::decode::{decode_primary, ItemDecoder};
use oxideav_heif::derived::GridDescriptor;
use oxideav_heif::encode::{
    encode_hevc_picture, encode_still, pad_frame, EncodeOptions, StillCodec, TileCarrier,
};
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
    let desc = GridDescriptor::new(rows as u16, cols as u16, src.width, src.height);
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
                (Property::Pixi(Pixi::new(vec![8, 8, 8])), false),
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
                    Property::Ispe(oxideav_heif::props::Ispe::new(120, 100)),
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
                    Property::Ispe(oxideav_heif::props::Ispe::new(
                        pic.coded_width,
                        pic.coded_height,
                    )),
                    false,
                ),
                (Property::Pixi(Pixi::new(vec![8])), false),
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
                    (Property::Ispe(oxideav_heif::props::Ispe::new(w, h)), false),
                    (Property::Pixi(Pixi::new(vec![8, 8, 8])), false),
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

/// Code a monochrome picture as an `hvc1` item (padded to 16, cropped
/// back with `clap`) and return its item id.
fn add_mono_hevc_item(writer: &mut HeifWriter, p: &HeifFrame) -> u32 {
    use oxideav_heif::encode::to_yuv420_8;
    let pic = encode_hevc_picture(
        &to_yuv420_8(&pad_frame(p, p.width.div_ceil(16) * 16, p.height.div_ceil(16) * 16).unwrap())
            .unwrap(),
        "pcm",
        0,
    )
    .unwrap();
    let mut props = vec![
        (pic.config.clone(), true),
        (
            Property::Ispe(oxideav_heif::props::Ispe::new(
                pic.coded_width,
                pic.coded_height,
            )),
            false,
        ),
        (Property::Pixi(Pixi::new(vec![8])), false),
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
    writer.add_coded_item(*b"hvc1", pic.data, props)
}

/// A seeded monochrome 8-bit picture.
fn mono_picture(w: u32, h: u32, seed: u32) -> HeifFrame {
    let mut f =
        HeifFrame::zeroed(w, h, HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap()).unwrap();
    for y in 0..h {
        for x in 0..w {
            f.set_sample(0, x, y, ((x * seed + y * 3 + seed * 17) % 256) as u16);
        }
    }
    f
}

/// Packed `cfen` inputs (Amd 1 §6.6.2.5.2 `is_packed_flag`, Amd 2
/// §6.6.2.5.3 regions): channels packed side by side / stacked in one
/// coded item's luma plane, with and without guard bands, with an
/// unused partition and a remainder column, in every chroma layout and
/// as RGB / RGBA under an identity matrix — written by `HeifWriter` +
/// `pack_colour_format_channels`, read back exactly (serial and
/// parallel), predicted by `predict_output`, clean under `check`.
#[test]
fn packed_colour_format_enhancement_round_trips() {
    use oxideav_heif::compose::pack_colour_format_channels;
    use oxideav_heif::derived::CfenInput;
    let packed = |cols: u8, rows: u8, hg: u8, vg: u8, channels: &[u8]| CfenInput::Packed {
        cols,
        rows,
        hor_guard_band_mul2: hg,
        ver_guard_band_mul2: vg,
        channels: channels.to_vec(),
    };
    // (label, w, h, chroma, matrix, alpha, per input: (channel pictures as
    // cells, entry)) — cells are indices into the plane list [Y/R, Cb/G,
    // Cr/B, alpha] or None (unused partition).
    struct Case {
        label: &'static str,
        w: u32,
        h: u32,
        chroma: Chroma,
        matrix: u16,
        alpha: bool,
        inputs: Vec<(Vec<Option<usize>>, CfenInput)>,
        /// Extra columns appended to the packed picture (a remainder the
        /// partition ignores).
        remainder: u32,
    }
    let cases = vec![
        Case {
            label: "420 Y + packed [Cb Cr]",
            w: 64,
            h: 48,
            chroma: Chroma::Yuv420,
            matrix: 6,
            alpha: false,
            inputs: vec![
                (vec![Some(0)], CfenInput::Channel(2)),
                (vec![Some(1), Some(2)], packed(2, 1, 0, 0, &[3, 4])),
            ],
            remainder: 0,
        },
        Case {
            label: "420 odd size, remainder column, guard bands",
            w: 33,
            h: 21,
            chroma: Chroma::Yuv420,
            matrix: 1,
            alpha: false,
            inputs: vec![
                (vec![Some(0)], CfenInput::Channel(2)),
                (vec![Some(1), Some(2)], packed(2, 1, 1, 2, &[3, 4])),
            ],
            remainder: 1,
        },
        Case {
            label: "422 Y + packed [Cb; Cr] with guard bands",
            w: 40,
            h: 24,
            chroma: Chroma::Yuv422,
            matrix: 6,
            alpha: false,
            inputs: vec![
                (vec![Some(0)], CfenInput::Channel(2)),
                (vec![Some(1), Some(2)], packed(1, 2, 3, 1, &[3, 4])),
            ],
            remainder: 0,
        },
        Case {
            label: "444 one packed [Y Cb Cr]",
            w: 24,
            h: 20,
            chroma: Chroma::Yuv444,
            matrix: 6,
            alpha: false,
            inputs: vec![(
                vec![Some(0), Some(1), Some(2)],
                packed(3, 1, 0, 0, &[2, 3, 4]),
            )],
            remainder: 0,
        },
        Case {
            label: "RGB 2x2 with an unused partition",
            w: 30,
            h: 18,
            chroma: Chroma::Yuv444,
            matrix: 0,
            alpha: false,
            inputs: vec![(
                vec![Some(0), Some(1), Some(2), None],
                packed(2, 2, 0, 0, &[2, 3, 4, 0]),
            )],
            remainder: 0,
        },
        Case {
            label: "RGBA 2x2 with guard bands",
            w: 30,
            h: 18,
            chroma: Chroma::Yuv444,
            matrix: 0,
            alpha: true,
            inputs: vec![(
                vec![Some(0), Some(1), Some(2), Some(3)],
                packed(2, 2, 2, 2, &[2, 3, 4, 5]),
            )],
            remainder: 0,
        },
        Case {
            label: "mono alpha packed, luma unpacked, unspecified extra",
            w: 20,
            h: 16,
            chroma: Chroma::Mono,
            matrix: 6,
            alpha: true,
            inputs: vec![
                (vec![Some(3), None], packed(2, 1, 1, 0, &[5, 1])),
                (vec![Some(0)], CfenInput::Channel(2)),
            ],
            remainder: 0,
        },
    ];
    for c in cases {
        let (cw, ch) = c.chroma.chroma_dims(c.w, c.h);
        let planes = [
            mono_picture(c.w, c.h, 5),
            mono_picture(cw, ch, 7),
            mono_picture(cw, ch, 11),
            mono_picture(c.w, c.h, 13),
        ];
        let mut writer = HeifWriter::new();
        let mut entries = Vec::new();
        for (cells, entry) in &c.inputs {
            let picture = match entry {
                CfenInput::Channel(_) => planes[cells[0].unwrap()].clone(),
                CfenInput::Packed {
                    cols,
                    rows,
                    hor_guard_band_mul2,
                    ver_guard_band_mul2,
                    ..
                } => {
                    let refs: Vec<Option<&HeifFrame>> =
                        cells.iter().map(|i| i.map(|i| &planes[i])).collect();
                    let p = pack_colour_format_channels(
                        &refs,
                        *cols,
                        *rows,
                        *hor_guard_band_mul2,
                        *ver_guard_band_mul2,
                    )
                    .unwrap();
                    if c.remainder > 0 {
                        pad_frame(&p, p.width + c.remainder, p.height).unwrap()
                    } else {
                        p
                    }
                }
                _ => unreachable!(),
            };
            let id = add_mono_hevc_item(&mut writer, &picture);
            entries.push((id, entry.clone()));
        }
        let colr = Colr::Nclx {
            primaries: 1,
            transfer: 13,
            matrix: c.matrix,
            full_range: true,
        };
        let n = c.chroma.colour_planes() + usize::from(c.alpha);
        let cfen = writer
            .add_colour_format_enhancement_inputs(
                &entries,
                vec![
                    (
                        Property::Ispe(oxideav_heif::props::Ispe::new(c.w, c.h)),
                        false,
                    ),
                    (Property::Pixi(Pixi::new(vec![8; n])), false),
                    (Property::Colr(colr.clone()), false),
                ],
            )
            .unwrap();
        writer.set_primary(cfen);
        let bytes = writer.write_to_vec().unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        // The body round-trips through the parser.
        let body = f.item_data_owned(cfen).unwrap();
        let parsed =
            oxideav_heif::derived::ColourFormatEnhancement::parse(&body, entries.len()).unwrap();
        assert_eq!(
            parsed.inputs,
            entries.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>(),
            "{}: body",
            c.label
        );
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(
            rep.violations
                .iter()
                .all(|v| !v.clause.starts_with("HEIF-A1 6.6.2.5")),
            "{}: {:#?}",
            c.label,
            rep.violations
        );
        let node = oxideav_heif::derived::build_primary_graph(&f).unwrap();
        let (fmt, size) = oxideav_heif::demux::predict_output(&node).unwrap();
        assert_eq!(
            (fmt.chroma, fmt.has_alpha, size),
            (c.chroma, c.alpha, (c.w, c.h)),
            "{}: prediction",
            c.label
        );
        for threads in [1usize, 3] {
            let dec = ItemDecoder::direct()
                .with_execution_context(&ExecutionContext::with_threads(threads));
            let img = decode_primary(&f, dec).unwrap();
            assert_eq!(img.frame.format, fmt, "{}", c.label);
            let t = img.frame.tight();
            // Output plane p holds channel: YCbCr — plane p is plane p;
            // RGB — planes G, B, R are channels [1, 2, 0]; alpha is last.
            let source_of_plane = |p: usize| -> &HeifFrame {
                if c.alpha && p == fmt.plane_count() - 1 {
                    &planes[3]
                } else if c.matrix == 0 {
                    &planes[[1usize, 2, 0][p]]
                } else {
                    &planes[p]
                }
            };
            for p in 0..fmt.plane_count() {
                assert_eq!(
                    t.planes[p].data,
                    source_of_plane(p).tight().planes[0].data,
                    "{}: plane {p} ({threads} threads)",
                    c.label
                );
            }
        }
    }
}

/// Packed `cfen` refusals: a repeated channel across inputs, an
/// unpacked `channel_id` 0, K (7) without the colr signalling that
/// selects CMYK, guard bands that swallow the cell, and a wrong
/// `packed_channel_id` count at the writer.
#[test]
fn packed_colour_format_enhancement_refusals() {
    use oxideav_heif::compose::plan_colour_format_enhancement_regions;
    use oxideav_heif::derived::{CfenInput, ColourFormatEnhancement};
    let colr = Colr::Nclx {
        primaries: 1,
        transfer: 13,
        matrix: 6,
        full_range: true,
    };
    let packed = |cols: u8, rows: u8, hg: u8, vg: u8, channels: &[u8]| CfenInput::Packed {
        cols,
        rows,
        hor_guard_band_mul2: hg,
        ver_guard_band_mul2: vg,
        channels: channels.to_vec(),
    };
    let plan = |inputs: Vec<CfenInput>, geometry: &[((u32, u32), u8)]| {
        plan_colour_format_enhancement_regions(
            &ColourFormatEnhancement::new(inputs),
            geometry,
            Some(&colr),
        )
    };
    let half = ((32, 24), 8u8);
    let full = ((64, 48), 8u8);
    // The reference layout plans.
    let ok = plan(
        vec![CfenInput::Channel(2), packed(2, 1, 0, 0, &[3, 4])],
        &[full, ((64, 24), 8)],
    )
    .unwrap();
    assert_eq!(
        (ok.format.chroma, ok.width, ok.height),
        (Chroma::Yuv420, 64, 48)
    );
    assert_eq!(ok.regions[1].len(), 2);
    assert_eq!(
        (
            ok.regions[1][1].x,
            ok.regions[1][1].width,
            ok.regions[1][1].plane
        ),
        (32, 32, Some(2))
    );
    // Guard bands shrink the regions symmetrically (Amd 2 OrigX / EndX).
    let g = plan(
        vec![CfenInput::Channel(2), packed(2, 1, 1, 2, &[3, 4])],
        &[full, ((72, 32), 8)],
    )
    .unwrap();
    assert_eq!(
        (
            g.regions[1][0].x,
            g.regions[1][0].y,
            g.regions[1][0].width,
            g.regions[1][0].height
        ),
        (2, 4, 32, 24)
    );
    assert_eq!((g.regions[1][1].x, g.regions[1][1].y), (38, 4));
    let e = plan(
        vec![CfenInput::Channel(3), packed(2, 1, 0, 0, &[3, 4])],
        &[half, ((64, 24), 8)],
    )
    .unwrap_err();
    assert!(e.to_string().contains("duplicate channel"), "{e}");
    let e = plan(vec![CfenInput::Channel(0)], &[full]).unwrap_err();
    assert!(e.to_string().contains("shall not be 0"), "{e}");
    let e = plan(
        vec![CfenInput::Channel(2), packed(2, 2, 0, 0, &[3, 4, 7, 0])],
        &[full, ((128, 96), 8)],
    )
    .unwrap_err();
    assert!(e.to_string().contains("CMYK"), "{e}");
    let e = plan(
        vec![CfenInput::Channel(2), packed(2, 1, 8, 0, &[3, 4])],
        &[full, ((64, 24), 8)],
    )
    .unwrap_err();
    assert!(e.to_string().contains("no region samples"), "{e}");
    let e = plan(
        vec![CfenInput::Channel(2), packed(2, 1, 0, 0, &[3])],
        &[full, ((64, 24), 8)],
    )
    .unwrap_err();
    assert!(e.to_string().contains("packed_channel_id values"), "{e}");
    let mut w = HeifWriter::new();
    assert!(w
        .add_colour_format_enhancement_inputs(&[(1, packed(2, 1, 0, 0, &[3]))], vec![])
        .is_err());
    assert!(w
        .add_colour_format_enhancement_inputs(&[(1, packed(9, 1, 0, 0, &[3; 9]))], vec![])
        .is_err());
}

/// A picture with an alpha plane for the high-level encoder.
fn picture_with_alpha(w: u32, h: u32) -> HeifFrame {
    let colour = picture(w, h);
    let mut alpha =
        HeifFrame::zeroed(w, h, HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap()).unwrap();
    for y in 0..h {
        for x in 0..w {
            alpha.set_sample(0, x, y, ((x * 2 + y) % 256) as u16);
        }
    }
    colour.with_alpha_plane(&alpha).unwrap()
}

fn tiled_opts(codec: StillCodec, tile: u32) -> EncodeOptions {
    let mut o = EncodeOptions::default();
    o.codec = codec;
    o.hevc_mode = "pcm".into();
    o.av1_quality = None;
    o.grid_tile = Some(tile);
    o.tile_carrier = TileCarrier::Tiled;
    o.thumbnail_max_dim = Some(64);
    o.exif = Some(b"MM\0*\0\0\0\x08\0\0".to_vec());
    o.xmp = Some("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>".into());
    o.icc_profile = Some(vec![0u8; 128]);
    o
}

/// The high-level encoder writes the tiled picture as one `tili` item
/// under [`TileCarrier::Tiled`]: the same tiles as the `grid` carrier
/// (bytes identical under any thread budget), one `tilC` with the
/// shared `hvcC` and tile `ispe`, the item's `ispe` / `pixi` / `colr` /
/// ICC, alpha, thumbnail and metadata on the tiled item; the picture
/// decodes exactly (serial and parallel), equals the grid carrier's
/// decode, passes the MIAF checks, re-muxes through `import_item`, and
/// the framework `tiling=tili` option selects it.
#[test]
fn encoder_writes_a_tiled_item_equal_to_its_grid() {
    let src = picture_with_alpha(300, 200);
    let opts = tiled_opts(StillCodec::Hevc, 128);
    let bytes = encode_still(&src, &opts).unwrap();
    let parallel = encode_still(&src, &opts.clone().with_threads(Some(3))).unwrap();
    assert_eq!(bytes, parallel, "the budget does not change a byte");
    let f = HeifFile::parse(&bytes).unwrap();
    let meta = f.meta().unwrap();
    let primary = f.primary_item().unwrap();
    assert_eq!(primary.item_type, *b"tili");
    let props = oxideav_heif::props::ItemProperties::resolve(meta, primary.id).unwrap();
    let tilc = props.tilc().unwrap();
    assert_eq!((tilc.tile_width, tilc.tile_height), (128, 128));
    assert_eq!(tilc.tile_grid(300, 200), Some((3, 2)));
    let (ty, assoc) = tilc.in_file_tiles.as_ref().unwrap();
    assert_eq!(ty, b"hvc1");
    assert_eq!(assoc.len(), 2, "hvcC + tile ispe");
    assert!(assoc[0].0 && !assoc[1].0);
    assert_eq!(&meta.properties[assoc[0].1 as usize - 1].box_type, b"hvcC");
    assert_eq!(&meta.properties[assoc[1].1 as usize - 1].box_type, b"ispe");
    assert_eq!(props.ispe().map(|i| (i.width, i.height)), Some((300, 200)));
    assert_eq!(
        props.pixi().map(|p| p.bits_per_channel.clone()),
        Some(vec![8, 8, 8])
    );
    assert_eq!(props.colrs().len(), 2, "nclx + ICC");
    assert_eq!(meta.thumbnails_of(primary.id).len(), 1);
    assert_eq!(meta.auxiliaries_of(primary.id).len(), 1, "alpha");
    assert_eq!(meta.metadata_of(primary.id).len(), 2, "Exif + XMP");
    // One deti, one tili: the tiles are not items.
    assert_eq!(meta.data_references.len(), 1);
    assert_eq!(
        meta.items
            .iter()
            .filter(|i| i.item_type == *b"hvc1")
            .count(),
        2,
        "alpha + thumbnail"
    );
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    // Exact under every budget; identical to the grid carrier.
    let grid = encode_still(&src, &opts.clone().with_tile_carrier(TileCarrier::Grid)).unwrap();
    let want = decode_primary(&HeifFile::parse(&grid).unwrap(), ItemDecoder::direct()).unwrap();
    assert_eq!(want.frame, src);
    for threads in [1, 4] {
        let dec =
            ItemDecoder::direct().with_execution_context(&ExecutionContext::with_threads(threads));
        let img = decode_primary(&f, dec).unwrap();
        assert_eq!(img.frame, src, "{threads} threads");
        assert_eq!(img.icc_profile, opts.icc_profile, "{threads} threads");
    }
    let api = oxideav_heif::decode(&bytes).unwrap();
    assert_eq!(api.metadata.exif, opts.exif);
    assert_eq!(
        api.metadata.xmp.as_deref(),
        opts.xmp.as_deref().map(str::as_bytes)
    );
    // Re-mux: the tiled item travels byte for byte (tiles, offset table,
    // deti entry, tile properties) with its alpha, thumbnail, metadata.
    let mut w = HeifWriter::new();
    let id = w.import_item(&f, primary.id).unwrap();
    w.set_primary(id);
    let remux = w.write_to_vec().unwrap();
    let g = HeifFile::parse(&remux).unwrap();
    let gm = g.meta().unwrap();
    assert_eq!(g.primary_item().unwrap().item_type, *b"tili");
    assert_eq!(g.item_data(id).unwrap(), f.item_data(primary.id).unwrap());
    assert_eq!(gm.data_references, meta.data_references);
    assert_eq!(gm.metadata_of(id).len(), 2);
    assert_eq!(
        decode_primary(&g, ItemDecoder::direct()).unwrap().frame,
        src
    );
    assert!(check(&g, MiafProfile::Miaf).unwrap().is_conformant());
    // Framework: `tiling=tili` writes the tiled item, the default a grid,
    // anything else is a typed refusal; the frame decodes the same.
    let (vf, pf) = src.to_core().unwrap();
    let make = |tiling: &str| {
        let mut params = CodecParameters::video(CodecId::new("heif"));
        params.width = Some(300);
        params.height = Some(200);
        params.pixel_format = Some(pf);
        params.options = CodecOptions::new()
            .set("grid", "128")
            .set("mode", "pcm")
            .set("tiling", tiling);
        oxideav_heif::encode::make_encoder(&params)
    };
    let run = |enc: &mut Box<dyn Encoder>| {
        enc.send_frame(&Frame::Video(vf.clone())).unwrap();
        enc.flush().unwrap();
        enc.receive_packet().unwrap().data.to_vec()
    };
    let tili = run(&mut make("tili").unwrap());
    let ft = HeifFile::parse(&tili).unwrap();
    assert_eq!(ft.primary_item().unwrap().item_type, *b"tili");
    let grid = run(&mut make("grid").unwrap());
    assert_eq!(
        HeifFile::parse(&grid)
            .unwrap()
            .primary_item()
            .unwrap()
            .item_type,
        *b"grid"
    );
    assert_eq!(
        decode_primary(&ft, ItemDecoder::direct()).unwrap().frame,
        decode_primary(&HeifFile::parse(&grid).unwrap(), ItemDecoder::direct())
            .unwrap()
            .frame
    );
    assert!(make("mosaic").is_err());
}

/// AV1 tiles under the tiled carrier: `av01` tiles sharing one `av1C`,
/// exact under lossless coding, the automatic tiling above 4 MP never
/// changes the carrier rule (an explicit small tile forces it here).
#[test]
fn av1_tiled_item_round_trips() {
    let src = picture(260, 140);
    let opts = tiled_opts(StillCodec::Av1, 128);
    let bytes = encode_still(&src, &opts).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let meta = f.meta().unwrap();
    let primary = f.primary_item().unwrap();
    assert_eq!(primary.item_type, *b"tili");
    let props = oxideav_heif::props::ItemProperties::resolve(meta, primary.id).unwrap();
    let tilc = props.tilc().unwrap();
    assert_eq!(tilc.tile_grid(260, 140), Some((3, 2)));
    let (ty, assoc) = tilc.in_file_tiles.as_ref().unwrap();
    assert_eq!(ty, b"av01");
    assert_eq!(&meta.properties[assoc[0].1 as usize - 1].box_type, b"av1C");
    assert!(check(&f, MiafProfile::Miaf).unwrap().is_conformant());
    for threads in [1, 3] {
        let dec =
            ItemDecoder::direct().with_execution_context(&ExecutionContext::with_threads(threads));
        assert_eq!(
            decode_primary(&f, dec).unwrap().frame,
            src,
            "{threads} threads"
        );
    }
    // The grid carrier on the same options stays a grid.
    let grid = encode_still(&src, &opts.with_tile_carrier(TileCarrier::Grid)).unwrap();
    assert_eq!(
        HeifFile::parse(&grid)
            .unwrap()
            .primary_item()
            .unwrap()
            .item_type,
        *b"grid"
    );
}
