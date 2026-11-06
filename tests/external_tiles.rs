//! `tili` tiled image items with external tiles (ISO/IEC
//! 23008-12:2025/Amd 2:2026 §6.11.2 / §6.11.5, `external_tiles_urls =
//! 1`): every tile a HEIF file of its own, addressed through the
//! `deti` URL template. The writer authors the item, the reader
//! composes it through a caller-supplied resolver (never touching a
//! filesystem or network itself) and refuses it typed — naming the
//! first missing URL — without one.
#![cfg(feature = "registry")]

use std::collections::HashMap;
use std::sync::Mutex;

use oxideav_core::ExecutionContext;
use oxideav_heif::compose::{GridCanvas, apply_imir, crop};
use oxideav_heif::decode::{ItemDecoder, decode_primary};
use oxideav_heif::derived::GridDescriptor;
use oxideav_heif::encode::{
    EncodeOptions, StillCodec, encode_hevc_picture, encode_still, pad_frame,
};
use oxideav_heif::image::Chroma;
use oxideav_heif::miaf::{MiafProfile, check};
use oxideav_heif::props::{Colr, Imir, Ispe, Pixi, Property};
use oxideav_heif::tiled::{DataEntryTiledItem, ExternalTileRequest, ExternalTiles};
use oxideav_heif::{DecodeOptions, HeifError, HeifFile, HeifFrame, HeifPixelFormat, HeifWriter};

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

/// A tile file: one lossless `hvc1` item of `t` under its own `hvcC` /
/// `ispe` (+ `extra` properties), the only image item of the file.
fn tile_file(t: &HeifFrame, extra: Vec<(Property, bool)>) -> Vec<u8> {
    let pic = encode_hevc_picture(t, "pcm", 0).unwrap();
    let mut w = HeifWriter::new();
    let mut props = vec![
        (pic.config, true),
        (Property::Ispe(Ispe::new(t.width, t.height)), false),
    ];
    props.extend(extra);
    let id = w.add_coded_item(*b"hvc1", pic.data, props);
    w.set_primary(id);
    w.write_to_vec().unwrap()
}

/// Cut `src` into `tile`-sized tile files (row-major) and the expected
/// composition (the padding cropped).
fn tile_files(src: &HeifFrame, tile: u32) -> (Vec<Vec<u8>>, HeifFrame) {
    let cols = src.width.div_ceil(tile);
    let rows = src.height.div_ceil(tile);
    let padded = pad_frame(src, cols * tile, rows * tile).unwrap();
    let desc = GridDescriptor::new(rows as u16, cols as u16, src.width, src.height);
    let mut expected = GridCanvas::new(&desc);
    let mut files = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let t = crop(&padded, c * tile, r * tile, tile, tile).unwrap();
            files.push(tile_file(&t, vec![]));
            expected.place((r * cols + c) as usize, &t).unwrap();
        }
    }
    (files, expected.finish().unwrap())
}

fn template(directories: Option<(u16, u16)>) -> ExternalTiles {
    ExternalTiles::new(
        directories,
        1000,
        "https://tiles.invalid/pictures/134532/image/".into(),
        "Representation1".into(),
        "$tileID$.heif".into(),
    )
}

/// The main file: an external-tile `tili` of `w × h` in `tile` tiles.
fn main_file(w: u32, h: u32, tile: u32, ext: ExternalTiles) -> Vec<u8> {
    let mut writer = HeifWriter::new();
    let id = writer
        .add_external_tiled_item(
            w,
            h,
            tile,
            tile,
            ext,
            vec![
                (Property::Pixi(Pixi::new(vec![8, 8, 8])), false),
                (Property::Colr(Colr::MIAF_DEFAULT), false),
            ],
        )
        .unwrap();
    writer.set_primary(id);
    writer.write_to_vec().unwrap()
}

/// A URL → bytes store keyed the way the `deti` addresses the tiles.
fn store(ext: &ExternalTiles, files: Vec<Vec<u8>>) -> HashMap<String, Vec<u8>> {
    let n = files.len() as u64;
    files
        .into_iter()
        .enumerate()
        .map(|(i, f)| (ext.url(i as u64, n).unwrap(), f))
        .collect()
}

/// A 200×136 picture in 64-px external tiles (4×3, padded right and
/// bottom): the file declares the item exactly as §6.11 describes it,
/// passes the MIAF / HEIF-A2 checks, refuses to compose from bytes
/// alone (naming the first tile's URL) and composes exactly — serial,
/// parallel and through the high-level options — once a resolver
/// supplies the tile files, every tile requested once with its row /
/// column / tile ID / URL.
#[test]
fn external_tiles_round_trip() {
    let src = picture(200, 136);
    let (files, expected) = tile_files(&src, 64);
    assert_eq!(files.len(), 12);
    let ext = template(None);
    let bytes = main_file(200, 136, 64, ext.clone());
    let tiles = store(&ext, files);

    let f = HeifFile::parse(&bytes).unwrap();
    let meta = f.meta().unwrap();
    let primary = f.primary_item().unwrap().clone();
    assert_eq!(primary.item_type, *b"tili");
    assert_eq!(meta.data_references.len(), 1);
    let dref = &meta.data_references[0];
    assert_eq!(dref.entry_type, *b"deti");
    assert!(!dref.self_contained, "external tiles are not in this file");
    let deti = DataEntryTiledItem::parse(dref).unwrap();
    assert_eq!(deti.input_items, 12);
    assert_eq!(deti.external.as_ref(), Some(&ext));
    let loc = meta.location(primary.id).unwrap();
    assert_eq!(loc.data_reference_index, 1);
    assert_eq!(loc.extents.len(), 1, "extent_count shall be >= 1");
    assert_eq!(loc.extents[0].length, 0);
    let props = oxideav_heif::props::ItemProperties::resolve(meta, primary.id).unwrap();
    let tilc = props.tilc().unwrap();
    assert_eq!((tilc.tile_width, tilc.tile_height), (64, 64));
    assert!(
        tilc.in_file_tiles.is_none(),
        "§6.11.3.2: no tile_item_type / tipa for external tiles"
    );
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    assert!(rep.advisories.is_empty(), "{:#?}", rep.advisories);

    // Bytes alone: a typed refusal naming the first missing tile.
    let err = decode_primary(&f, ItemDecoder::direct()).unwrap_err();
    assert!(matches!(err, HeifError::Unsupported(_)), "{err}");
    assert_eq!(
        err.external_tile_info(),
        Some((
            primary.id,
            "https://tiles.invalid/pictures/134532/image/Representation1/1000.heif".to_string()
        ))
    );
    let err = oxideav_heif::decode(&bytes).unwrap_err();
    assert!(err.external_tile_info().is_some(), "{err}");

    // With a resolver.
    let seen = Mutex::new(Vec::new());
    let resolver = |r: &ExternalTileRequest| -> oxideav_heif::Result<Vec<u8>> {
        seen.lock().unwrap().push(r.clone());
        tiles
            .get(&r.url)
            .cloned()
            .ok_or_else(|| HeifError::io(format!("no tile at {}", r.url)))
    };
    let img = decode_primary(&f, ItemDecoder::direct().with_tile_resolver(&resolver)).unwrap();
    assert_eq!((img.width(), img.height()), (200, 136));
    assert_eq!(img.frame, expected);
    let seen = seen.into_inner().unwrap();
    assert_eq!(seen.len(), 12);
    for (i, r) in seen.iter().enumerate() {
        assert_eq!(r.item_id, primary.id);
        assert_eq!(r.index, i as u64);
        assert_eq!((r.column, r.row), (i as u32 % 4, i as u32 / 4));
        assert_eq!(r.tile_id, 1000 + i as u64);
        assert_eq!(r.directory_id, None);
        assert!(
            r.url
                .ends_with(&format!("Representation1/{}.heif", 1000 + i))
        );
    }
    let resolver =
        |r: &ExternalTileRequest| -> oxideav_heif::Result<Vec<u8>> { Ok(tiles[&r.url].clone()) };
    for threads in [2, 5] {
        let dec = ItemDecoder::direct()
            .with_execution_context(&ExecutionContext::with_threads(threads))
            .with_tile_resolver(&resolver);
        let par = decode_primary(&f, dec).unwrap();
        assert_eq!(par.frame, expected, "{threads} threads");
    }
    // The high-level options carry a shareable resolver.
    let owned = tiles.clone();
    let opts = DecodeOptions::default().with_tile_resolver(
        move |r: &ExternalTileRequest| -> oxideav_heif::Result<Vec<u8>> {
            owned
                .get(&r.url)
                .cloned()
                .ok_or_else(|| HeifError::io("missing"))
        },
    );
    let a = oxideav_heif::decode_with(&bytes, &opts).unwrap();
    let b = oxideav_heif::HeifImage::from(img);
    assert_eq!((a.width, a.height, a.format), (b.width, b.height, b.format));
    assert_eq!(a.planes, b.planes);
    assert_eq!(opts.clone(), opts, "a handle equals its clone");
}

/// Directory IDs (§6.11.5.3): NumTiles / (end − start) tiles per
/// directory in row-major order, the last directory taking the rest;
/// the URL gains `<directory>/` (Table 12). The composition does not
/// depend on the addressing.
#[test]
fn external_tiles_in_directories() {
    let src = picture(96, 80);
    let (files, expected) = tile_files(&src, 32);
    assert_eq!(files.len(), 9);
    // 9 tiles over directories 10..=14: 9 / 4 = 2 per directory, the
    // ninth tile in the last one.
    let ext = template(Some((10, 14)));
    let dirs: Vec<u16> = (0..9)
        .map(|i| ext.directory_id(i, 9).unwrap().unwrap())
        .collect();
    assert_eq!(dirs, [10, 10, 11, 11, 12, 12, 13, 13, 14]);
    assert_eq!(
        ext.url(8, 9).unwrap(),
        "https://tiles.invalid/pictures/134532/image/Representation1/14/1008.heif"
    );
    let bytes = main_file(96, 80, 32, ext.clone());
    let tiles = store(&ext, files);
    let f = HeifFile::parse(&bytes).unwrap();
    let id = f.primary_item().unwrap().id;
    let err = decode_primary(&f, ItemDecoder::direct()).unwrap_err();
    assert_eq!(
        err.external_tile_info(),
        Some((
            id,
            "https://tiles.invalid/pictures/134532/image/Representation1/10/1000.heif".into()
        ))
    );
    let resolver = |r: &ExternalTileRequest| -> oxideav_heif::Result<Vec<u8>> {
        assert_eq!(r.directory_id, Some(dirs[r.index as usize]));
        Ok(tiles[&r.url].clone())
    };
    let img = decode_primary(&f, ItemDecoder::direct().with_tile_resolver(&resolver)).unwrap();
    assert_eq!(img.frame, expected);
    // Fewer tiles than directories leaves the split undefined: the
    // writer refuses it typed.
    let mut w = HeifWriter::new();
    let err = w
        .add_external_tiled_item(96, 80, 32, 32, template(Some((0, 20))), vec![])
        .unwrap_err();
    assert!(matches!(err, HeifError::Unsupported(_)), "{err}");
}

/// A tile carrying its own transformative property (§6.11.2: the input
/// tile in an external file may have item properties) decodes through
/// its file's graph: a tile stored mirrored with an `imir` lands
/// upright, the composition still exact.
#[test]
fn external_tile_with_its_own_properties() {
    let src = picture(128, 64);
    let (mut files, expected) = tile_files(&src, 64);
    let padded = pad_frame(&src, 128, 64).unwrap();
    let t1 = crop(&padded, 64, 0, 64, 64).unwrap();
    let imir = Imir::new(1);
    let stored = apply_imir(&t1, &imir).unwrap();
    assert_ne!(stored, t1);
    files[1] = tile_file(&stored, vec![(Property::Imir(imir), true)]);
    let ext = template(None);
    let bytes = main_file(128, 64, 64, ext.clone());
    let tiles = store(&ext, files);
    let f = HeifFile::parse(&bytes).unwrap();
    let resolver =
        |r: &ExternalTileRequest| -> oxideav_heif::Result<Vec<u8>> { Ok(tiles[&r.url].clone()) };
    for threads in [1, 3] {
        let dec = ItemDecoder::direct()
            .with_threads(threads)
            .with_tile_resolver(&resolver);
        assert_eq!(decode_primary(&f, dec).unwrap().frame, expected);
    }
}

/// The §6.11.2 shalls on the tile files are typed refusals carrying the
/// tile index and URL: a tile of the wrong size, a second coding format,
/// a second image item, an entity group, a non-HEIF payload; a resolver
/// error passes through unchanged.
#[test]
fn external_tile_refusals() {
    let src = picture(128, 64);
    let (files, _) = tile_files(&src, 64);
    let ext = template(None);
    let bytes = main_file(128, 64, 64, ext.clone());
    let f = HeifFile::parse(&bytes).unwrap();
    let decode_with_tile1 = |tile1: Vec<u8>| {
        let tiles = store(&ext, vec![files[0].clone(), tile1]);
        let resolver = |r: &ExternalTileRequest| -> oxideav_heif::Result<Vec<u8>> {
            Ok(tiles[&r.url].clone())
        };
        decode_primary(&f, ItemDecoder::direct().with_tile_resolver(&resolver)).unwrap_err()
    };
    let invalid = |e: HeifError, what: &str| {
        assert!(matches!(e, HeifError::InvalidData(_)), "{what}: {e}");
        let m = e.to_string();
        assert!(m.contains("external tile 1"), "{what}: {m}");
        assert!(m.contains("Representation1/1001.heif"), "{what}: {m}");
        assert!(m.contains(what), "{what}: {m}");
    };
    // Wrong size.
    invalid(
        decode_with_tile1(tile_file(&picture(32, 32), vec![])),
        "32x32",
    );
    // A second coding format (AV1 tile next to an HEVC one).
    let mut o = EncodeOptions::default();
    o.codec = StillCodec::Av1;
    o.thumbnail_max_dim = None;
    let av1 = encode_still(&picture(64, 64), &o).unwrap();
    invalid(decode_with_tile1(av1), "coding format");
    // Two image items.
    let t = picture(64, 64);
    let pic = encode_hevc_picture(&t, "pcm", 0).unwrap();
    let mut w = HeifWriter::new();
    let props = vec![
        (pic.config.clone(), true),
        (Property::Ispe(Ispe::new(64, 64)), false),
    ];
    let a = w.add_coded_item(*b"hvc1", pic.data.clone(), props.clone());
    w.add_coded_item(*b"hvc1", pic.data.clone(), props.clone());
    w.set_primary(a);
    invalid(
        decode_with_tile1(w.write_to_vec().unwrap()),
        "2 image items",
    );
    // An entity group.
    let mut w = HeifWriter::new();
    let a = w.add_coded_item(*b"hvc1", pic.data.clone(), props);
    w.set_primary(a);
    w.add_entity_group(*b"altr", 100, vec![a]);
    invalid(decode_with_tile1(w.write_to_vec().unwrap()), "entity group");
    // Not a HEIF file.
    let e = decode_with_tile1(b"not a heif file".to_vec());
    assert!(e.to_string().contains("external tile 1"), "{e}");
    // A resolver error passes through.
    let resolver = |_: &ExternalTileRequest| -> oxideav_heif::Result<Vec<u8>> {
        Err(HeifError::io("offline"))
    };
    let e = decode_primary(&f, ItemDecoder::direct().with_tile_resolver(&resolver)).unwrap_err();
    assert!(matches!(e, HeifError::Io(_)), "{e}");
    assert!(e.to_string().contains("offline"), "{e}");
}

/// Re-mux: `import_item` carries the external-tile item over with its
/// `deti` URL template byte for byte (no tile file is read); the
/// writer refuses a NUL in a URL field and a zero dimension.
#[test]
fn external_tiles_remux_and_writer_refusals() {
    let ext = template(Some((3, 5)));
    let bytes = main_file(200, 136, 64, ext.clone());
    let src = HeifFile::parse(&bytes).unwrap();
    let out = HeifWriter::from_files(&[&bytes])
        .unwrap()
        .write_to_vec()
        .unwrap();
    let dst = HeifFile::parse(&out).unwrap();
    let (m0, m1) = (src.meta().unwrap(), dst.meta().unwrap());
    assert_eq!(m0.data_references, m1.data_references);
    let p = dst.primary_item().unwrap();
    assert_eq!(p.item_type, *b"tili");
    assert_eq!(
        DataEntryTiledItem::parse(&m1.data_references[0])
            .unwrap()
            .external,
        Some(ext)
    );
    assert!(check(&dst, MiafProfile::Miaf).unwrap().is_conformant());

    let mut w = HeifWriter::new();
    let bad = ExternalTiles::new(None, 0, "a\0b".into(), String::new(), "$tileID$".into());
    assert!(matches!(
        w.add_external_tiled_item(64, 64, 64, 64, bad, vec![]),
        Err(HeifError::InvalidData(_))
    ));
    assert!(
        w.add_external_tiled_item(0, 64, 64, 64, template(None), vec![])
            .is_err()
    );
}

/// The `iloc` shape of an external-tile item, written and re-muxed:
/// exactly one extent at offset 0 of length 0, construction method 0,
/// `data_reference_index` naming the external `deti` (ISO/IEC 14496-12
/// §8.11.3.3 requires `extent_count` ≥ 1; HEIF Amd 2:2026 §6.11.4
/// defines no in-file payload for external tiles). It is the only
/// zero-length extent a written file carries (the `heif_writer` fuzz
/// invariant exempts exactly this shape; the daily run's regression
/// input is the re-mux of such a file).
#[test]
fn external_tile_item_iloc_is_one_empty_extent_naming_the_deti() {
    let bytes = main_file(200, 136, 64, template(None));
    let remuxed = HeifWriter::from_files(&[&bytes])
        .unwrap()
        .write_to_vec()
        .unwrap();
    for file in [&bytes, &remuxed] {
        let f = HeifFile::parse(file).unwrap();
        let meta = f.meta().unwrap();
        let mut external = 0;
        for loc in &meta.locations {
            let item = meta.item(loc.item_id).unwrap();
            let deti = (loc.data_reference_index as usize)
                .checked_sub(1)
                .and_then(|i| meta.data_references.get(i))
                .and_then(|d| DataEntryTiledItem::parse(d).ok());
            if item.item_type == *b"tili" && deti.as_ref().is_some_and(|d| d.external.is_some()) {
                external += 1;
                assert_eq!(loc.construction_method, 0);
                assert_eq!(loc.extents.len(), 1);
                assert_eq!((loc.extents[0].offset, loc.extents[0].length), (0, 0));
            } else {
                assert!(
                    loc.extents.iter().all(|e| e.length != 0),
                    "item {}: zero-length extent",
                    loc.item_id
                );
            }
        }
        assert_eq!(external, 1);
    }
}

/// The HEIF-A2 §6.11 checks of `miaf::check`, from the `meta` tree:
/// `no_of_input_items` against `NumTiles`, the `tilC` tail against the
/// `deti` storage form, a `tili` whose `iloc` names no `deti`.
#[test]
fn miaf_checks_tiled_items() {
    let ext = template(None);
    let bytes = main_file(200, 136, 64, ext);
    let f = HeifFile::parse(&bytes).unwrap();
    let id = f.primary_item().unwrap().id;
    let clauses = |m: &HeifFile| -> Vec<&'static str> {
        check(m, MiafProfile::Miaf)
            .unwrap()
            .violations
            .iter()
            .filter(|v| v.item_id == Some(id))
            .map(|v| v.clause)
            .collect()
    };
    assert!(clauses(&f).is_empty());
    // no_of_input_items 11 for 12 tiles.
    let mut m = f.clone();
    m.meta.as_mut().unwrap().data_references[0].payload[3] = 11;
    assert_eq!(clauses(&m), ["HEIF-A2 6.11.5.3"]);
    // The deti switched to in-file tiles: the tilC lacks its tail.
    let mut m = f.clone();
    let d = &mut m.meta.as_mut().unwrap().data_references[0];
    let (flags, payload) = DataEntryTiledItem::in_file(12, 0, 96);
    d.flags = flags;
    d.payload = payload;
    d.self_contained = true;
    assert_eq!(clauses(&m), ["HEIF-A2 6.11.3.2"]);
    // The iloc entry names no data reference.
    let mut m = f.clone();
    let meta = m.meta.as_mut().unwrap();
    let loc = meta.locations.iter_mut().find(|l| l.item_id == id).unwrap();
    loc.data_reference_index = 0;
    assert!(clauses(&m).contains(&"HEIF-A2 6.11.2"), "{:?}", clauses(&m));
}
