//! HEVC tile items (HEIF B.2.5, §6.5.7): an `hvt1` item is a
//! rectangular set of tiles of the HEVC image item its `tbas` reference
//! names, placed by its `rloc` property; its reconstructed image is
//! that region of the image item's reconstructed image.
//!
//! The files are written here from the vendored pictures: the picture
//! as an `hvc1` item, four `hvt1` quadrants over it (`tbas`, `rloc`,
//! `ispe`, the picture's `hvcC`), and a `grid` of the four tile items.
//! Every tile must decode to its quadrant of the picture sample for
//! sample, the grid to the whole picture, on the contract path, the
//! item decoder under a thread budget and the framework path; the
//! shapes B.2.5 / §6.5.7 rule out are typed refusals.
#![cfg(feature = "registry")]

mod common;

use common::{fixture_bytes, vendored_root};
use oxideav_heif::decode::{ItemDecoder, decode_item};
use oxideav_heif::derived::GridDescriptor;
use oxideav_heif::props::{Ispe, ItemProperties, Property, Rloc};
use oxideav_heif::writer::{HeifWriter, ItemBody};
use oxideav_heif::{HeifError, HeifFile, HeifFrame};

/// The picture of `bundle` as a writer holding its `hvc1` item, with
/// that item's id, coded bytes and `hvcC`.
fn base(bundle: &str) -> (HeifWriter, u32, Vec<u8>, Property) {
    let bytes = fixture_bytes(&vendored_root(), bundle);
    let source = HeifFile::parse(&bytes).unwrap();
    let primary = source.primary_item().unwrap().id;
    let mut w = HeifWriter::new();
    let id = w.import_item(&source, primary).unwrap();
    let item = w.items().iter().find(|i| i.id == id).unwrap();
    let ItemBody::Coded(data) = &item.body else {
        panic!("{bundle}: one coded body");
    };
    let hvcc = item
        .properties
        .iter()
        .find(|(p, _)| matches!(p, Property::HvcC(_)))
        .unwrap()
        .0
        .clone();
    let data = data.clone();
    (w, id, data, hvcc)
}

fn tile_props(hvcc: &Property, w: u32, h: u32, at: Option<(u32, u32)>) -> Vec<(Property, bool)> {
    let mut v = vec![
        (hvcc.clone(), true),
        (Property::Ispe(Ispe::new(w, h)), false),
    ];
    if let Some((x, y)) = at {
        v.push((Property::Rloc(Rloc::new(x, y)), false));
    }
    v
}

/// `(x, y, w, h)` of `picture` sample by sample equals `tile`.
fn assert_region(picture: &HeifFrame, tile: &HeifFrame, (x, y): (u32, u32), what: &str) {
    assert_eq!(tile.format, picture.format, "{what}: layout");
    let (sx, sy) = picture.format.chroma.shift();
    for p in 0..tile.format.plane_count() {
        let chroma = p != 0 && Some(p) != tile.format.alpha_plane();
        let (ox, oy) = if chroma { (x >> sx, y >> sy) } else { (x, y) };
        let (pw, ph) = tile.plane_dims(p);
        for ty in 0..ph {
            for tx in 0..pw {
                assert_eq!(
                    tile.sample(p, tx, ty),
                    picture.sample(p, ox + tx, oy + ty),
                    "{what}: plane {p} ({tx},{ty})"
                );
            }
        }
    }
}

#[test]
fn tile_items_are_regions_of_their_image_item() {
    for bundle in [
        "still-image-with-exif",
        "still-yuv444",
        "still-10bit-main10",
        "still-monochrome",
    ] {
        let (mut w, pic, data, hvcc) = base(bundle);
        let clear = fixture_bytes(&vendored_root(), bundle);
        let source = HeifFile::parse(&clear).unwrap();
        let primary = source.primary_item().unwrap().id;
        // The reconstructed image of the image item (no transforms on
        // these pictures).
        let picture = decode_item(&source, primary, ItemDecoder::direct())
            .unwrap()
            .frame;
        let (pw, ph) = (picture.width, picture.height);
        assert!(pw % 4 == 0 && ph % 4 == 0, "{bundle}: {pw}x{ph}");
        let (tw, th) = (pw / 2, ph / 2);
        let mut tiles = Vec::new();
        for (x, y) in [(0, 0), (tw, 0), (0, th), (tw, th)] {
            // The item's data is the picture's slices (a tile item holds
            // the VCL NAL units of its tiles; these pictures have one
            // slice).
            let id = w.add_coded_item(
                *b"hvt1",
                data.clone(),
                tile_props(&hvcc, tw, th, Some((x, y))),
            );
            w.add_reference(*b"tbas", id, vec![pic]);
            tiles.push((id, (x, y)));
        }
        // An off-centre tile of another size.
        let odd = (tw / 2, th / 2, tw + 2, th - 2);
        let odd_id = w.add_coded_item(
            *b"hvt1",
            data.clone(),
            tile_props(&hvcc, odd.2, odd.3, Some((odd.0, odd.1))),
        );
        w.add_reference(*b"tbas", odd_id, vec![pic]);
        let ids: Vec<u32> = tiles.iter().map(|t| t.0).collect();
        let grid = w
            .add_grid(GridDescriptor::new(2, 2, pw, ph), &ids, Vec::new())
            .unwrap();
        w.set_primary(grid);
        let bytes = w.write_to_vec().unwrap();
        // `OXIDEAV_HEIF_WRITE_SEEDS=<dir>`: the smallest file as a fuzz seed.
        if bundle == "still-10bit-main10"
            && let Ok(dir) = std::env::var("OXIDEAV_HEIF_WRITE_SEEDS")
        {
            let _ = std::fs::create_dir_all(&dir);
            let path = std::path::Path::new(&dir).join("minimal.hvt1-tile-items.heic");
            std::fs::write(path, &bytes).unwrap();
        }
        let file = HeifFile::parse(&bytes).unwrap();
        // The items say what they are.
        let meta = file.meta().unwrap();
        for (id, at) in &tiles {
            assert_eq!(meta.references_from(*id, b"tbas"), [pic]);
            let props = ItemProperties::resolve(meta, *id).unwrap();
            assert_eq!(props.rloc(), Some(Rloc::new(at.0, at.1)));
        }
        // Every tile is its region; the grid of them is the picture.
        for threads in [1, 4] {
            let dec = || ItemDecoder::direct().with_threads(threads);
            for (id, at) in &tiles {
                let t = decode_item(&file, *id, dec()).unwrap().frame;
                assert_eq!((t.width, t.height), (tw, th));
                assert_region(&picture, &t, *at, &format!("{bundle} tile {id}"));
            }
            let t = decode_item(&file, odd_id, dec()).unwrap().frame;
            assert_eq!((t.width, t.height), (odd.2, odd.3));
            assert_region(&picture, &t, (odd.0, odd.1), &format!("{bundle} odd tile"));
            let whole = decode_item(&file, grid, dec()).unwrap().frame;
            assert_eq!(
                whole.tight(),
                picture.tight(),
                "{bundle}: grid of tile items"
            );
        }
        // The contract and the framework path.
        let image = oxideav_heif::decode(&bytes).unwrap();
        let want = oxideav_heif::decode(&clear).unwrap();
        assert_eq!(image.to_rgba8(), want.to_rgba8(), "{bundle}: contract");
        let all = oxideav_heif::decode_all(&bytes).unwrap();
        // The grid (primary), the picture, the four tiles, the odd one.
        assert_eq!(all.len(), 7, "{bundle}");
        {
            use oxideav_core::{Frame, RuntimeContext};
            let mut ctx = RuntimeContext::new();
            oxideav_h265::register(&mut ctx);
            oxideav_heif::register(&mut ctx);
            let mut d = ctx
                .containers
                .open_demuxer(
                    "heif",
                    Box::new(std::io::Cursor::new(bytes.clone())),
                    &ctx.codecs,
                )
                .unwrap();
            let stream = d.streams()[0].clone();
            assert_eq!(
                (stream.params.width, stream.params.height),
                (Some(pw), Some(ph)),
                "{bundle}"
            );
            let pkt = d.next_packet().unwrap();
            let mut dec = ctx.codecs.first_decoder(&stream.params).unwrap();
            dec.send_packet(&pkt).unwrap();
            let Frame::Video(v) = dec.receive_frame().unwrap() else {
                panic!("{bundle}: a picture");
            };
            let got = oxideav_heif::HeifImage::from_video_frame(&v, &stream.params).unwrap();
            assert_eq!(got.to_rgba8(), want.to_rgba8(), "{bundle}: framework");
        }
    }
}

/// Adds one tile item over the picture `(writer, picture id, coded
/// bytes, hvcC)` and returns its id.
type BuildTile<'a> = dyn Fn(&mut HeifWriter, u32, &[u8], &Property) -> u32 + 'a;

#[test]
fn tile_items_outside_the_rules_are_refused() {
    let bundle = "still-image-with-exif";
    let clear = fixture_bytes(&vendored_root(), bundle);
    let source = HeifFile::parse(&clear).unwrap();
    let picture = decode_item(
        &source,
        source.primary_item().unwrap().id,
        ItemDecoder::direct(),
    )
    .unwrap()
    .frame;
    let (pw, ph) = (picture.width, picture.height);
    // One tile item per case, built over a fresh copy of the picture.
    let case = |build: &BuildTile| -> HeifError {
        let (mut w, pic, data, hvcc) = base(bundle);
        let id = build(&mut w, pic, &data, &hvcc);
        w.set_primary(pic);
        let bytes = w.write_to_vec().unwrap();
        let file = HeifFile::parse(&bytes).unwrap();
        // The picture itself still decodes.
        assert!(decode_item(&file, pic, ItemDecoder::direct()).is_ok());
        decode_item(&file, id, ItemDecoder::direct()).unwrap_err()
    };
    // No rloc (B.2.5 requires one).
    let e = case(&|w, pic, data, hvcc| {
        let id = w.add_coded_item(*b"hvt1", data.to_vec(), tile_props(hvcc, 16, 16, None));
        w.add_reference(*b"tbas", id, vec![pic]);
        id
    });
    assert!(e.to_string().contains("rloc"), "{e}");
    // No tbas reference; two of them.
    let e = case(&|w, _, data, hvcc| {
        w.add_coded_item(
            *b"hvt1",
            data.to_vec(),
            tile_props(hvcc, 16, 16, Some((0, 0))),
        )
    });
    assert!(e.to_string().contains("tbas"), "{e}");
    let e = case(&|w, pic, data, hvcc| {
        let id = w.add_coded_item(
            *b"hvt1",
            data.to_vec(),
            tile_props(hvcc, 16, 16, Some((0, 0))),
        );
        w.add_reference(*b"tbas", id, vec![pic, pic]);
        id
    });
    assert!(e.to_string().contains("tbas"), "{e}");
    // tbas naming another tile item, or the item itself.
    let e = case(&|w, pic, data, hvcc| {
        let a = w.add_coded_item(
            *b"hvt1",
            data.to_vec(),
            tile_props(hvcc, 16, 16, Some((0, 0))),
        );
        w.add_reference(*b"tbas", a, vec![pic]);
        let b = w.add_coded_item(
            *b"hvt1",
            data.to_vec(),
            tile_props(hvcc, 16, 16, Some((0, 0))),
        );
        w.add_reference(*b"tbas", b, vec![a]);
        b
    });
    assert!(e.to_string().contains("not an HEVC image item"), "{e}");
    let e = case(&|w, _, data, hvcc| {
        let a = w.add_coded_item(
            *b"hvt1",
            data.to_vec(),
            tile_props(hvcc, 16, 16, Some((0, 0))),
        );
        w.add_reference(*b"tbas", a, vec![a]);
        a
    });
    assert!(e.to_string().contains("not an HEVC image item"), "{e}");
    // A region that leaves the picture.
    let e = case(&|w, pic, data, hvcc| {
        let id = w.add_coded_item(
            *b"hvt1",
            data.to_vec(),
            tile_props(hvcc, 16, 16, Some((pw - 8, ph - 8))),
        );
        w.add_reference(*b"tbas", id, vec![pic]);
        id
    });
    assert!(matches!(e, HeifError::InvalidData(_)), "{e}");
    assert!(e.to_string().contains("outside"), "{e}");
    let e = case(&|w, pic, data, hvcc| {
        let id = w.add_coded_item(
            *b"hvt1",
            data.to_vec(),
            tile_props(hvcc, 16, 16, Some((u32::MAX, u32::MAX))),
        );
        w.add_reference(*b"tbas", id, vec![pic]);
        id
    });
    assert!(e.to_string().contains("outside"), "{e}");
    // An offset off the 4:2:0 chroma grid (§6.5.7: aligned grids).
    let e = case(&|w, pic, data, hvcc| {
        let id = w.add_coded_item(
            *b"hvt1",
            data.to_vec(),
            tile_props(hvcc, 16, 16, Some((3, 2))),
        );
        w.add_reference(*b"tbas", id, vec![pic]);
        id
    });
    assert!(matches!(e, HeifError::Unsupported(_)), "{e}");
}
