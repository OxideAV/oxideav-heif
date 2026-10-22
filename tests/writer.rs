//! Writer round trips through the oxideav encoders (`registry`): pixels
//! → `.heic` / `.avif` → this crate's reader → the same pixels, plus
//! MIAF conformance and a black-box decoder cross-check of the file we
//! wrote.
#![cfg(feature = "registry")]

mod common;

use oxideav_core::{CodecId, CodecParameters, Frame, PixelFormat, RuntimeContext};
use oxideav_heif::decode::{decode_item, decode_primary, ItemDecoder};
use oxideav_heif::encode::{encode_still, EncodeOptions, StillCodec};
use oxideav_heif::image::Chroma;
use oxideav_heif::miaf::{check, MiafProfile};
use oxideav_heif::props::{Imir, Irot, Property};
use oxideav_heif::{HeifFile, HeifFrame, HeifPixelFormat, HeifWriter};

/// A deterministic 4:2:0 test picture with structure in every plane.
fn picture(w: u32, h: u32) -> HeifFrame {
    let mut f = HeifFrame::zeroed(
        w,
        h,
        HeifPixelFormat::new(Chroma::Yuv420, 8, false).unwrap(),
    )
    .unwrap();
    for y in 0..h {
        for x in 0..w {
            f.set_sample(0, x, y, ((x * 7 + y * 3) % 256) as u16);
        }
    }
    let (cw, ch) = f.plane_dims(1);
    for y in 0..ch {
        for x in 0..cw {
            f.set_sample(1, x, y, ((x * 5 + 40) % 256) as u16);
            f.set_sample(2, x, y, ((y * 9 + 90) % 256) as u16);
        }
    }
    f
}

fn lossless() -> EncodeOptions {
    EncodeOptions::default().with_hevc_mode("pcm".into())
}

#[test]
fn hevc_lossless_round_trip_is_exact() {
    let src = picture(64, 48);
    let bytes = encode_still(&src, &lossless()).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert!(f.file_type.has_brand(b"heic"));
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, src);
    assert!(img.nclx_explicit);
}

#[test]
fn odd_sizes_get_a_clap_and_round_trip() {
    // 33x21 → padded to 48x32, clap back to 33x21 (odd → 4:4:4 promotion on read).
    let src = picture(33, 21);
    let bytes = encode_still(&src, &lossless()).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let node = oxideav_heif::derived::build_primary_graph(&f).unwrap();
    assert!(node.properties.clap().is_some());
    assert_eq!(node.output_size().unwrap(), (33, 21));
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!((img.width(), img.height()), (33, 21));
    assert_eq!(img.frame.format.chroma, Chroma::Yuv444);
    for y in 0..21 {
        for x in 0..33 {
            assert_eq!(img.frame.sample(0, x, y), src.sample(0, x, y));
            assert_eq!(img.frame.sample(1, x, y), src.sample(1, x / 2, y / 2));
        }
    }
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
}

#[test]
fn grid_thumbnail_alpha_metadata_round_trip() {
    let mut src = picture(80, 64);
    let mut alpha = HeifFrame::zeroed(
        80,
        64,
        HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap(),
    )
    .unwrap();
    for y in 0..64 {
        for x in 0..80 {
            alpha.set_sample(0, x, y, if x < 40 { 255 } else { (y * 4) as u16 });
        }
    }
    src = src.with_alpha_plane(&alpha).unwrap();
    let opts = lossless()
        .with_grid_tile(Some(32))
        .with_thumbnail_max_dim(Some(20))
        .with_exif(Some(b"II*\0\x08\0\0\0\0\0".to_vec()))
        .with_xmp(Some("<x:xmpmeta>t</x:xmpmeta>".into()));
    let bytes = encode_still(&src, &opts).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let meta = f.meta().unwrap();
    let primary = f.primary_item().unwrap();
    assert_eq!(primary.item_type, *b"grid");
    assert_eq!(
        meta.derivation_inputs(primary.id).len(),
        2,
        "80x64 with 64-px tiles (MIAF floor): 2 columns x 1 row"
    );
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!((img.width(), img.height()), (80, 64));
    assert!(img.frame.format.has_alpha);
    assert_eq!(img.frame.without_alpha(), src.without_alpha());
    assert_eq!(img.frame.alpha_as_frame().unwrap(), alpha);
    assert_eq!(img.thumbnail_ids.len(), 1);
    let thumb = decode_item(&f, img.thumbnail_ids[0], ItemDecoder::direct()).unwrap();
    assert_eq!((thumb.width(), thumb.height()), (20, 16));
    assert_eq!(img.exif.as_deref(), Some(&b"II*\0\x08\0\0\0\0\0"[..]));
    assert_eq!(img.xmp.as_deref(), Some("<x:xmpmeta>t</x:xmpmeta>"));
}

#[test]
fn transforms_are_written_as_essential_properties_on_the_coded_item() {
    let src = picture(48, 32);
    let opts = lossless().with_transforms(vec![
        Property::Irot(Irot::new(1)),
        Property::Imir(Imir::new(1)),
    ]);
    let bytes = encode_still(&src, &opts).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    // The transforms ride on the coded item itself (essential
    // properties), the shape third-party readers honour; no `iden`
    // wrapper is written.
    let primary = f.primary_item().unwrap();
    assert_eq!(primary.item_type, *b"hvc1");
    let props =
        oxideav_heif::props::ItemProperties::resolve(f.meta().unwrap(), primary.id).unwrap();
    assert!(props.irot().is_some() && props.imir().is_some());
    assert!(
        props.transformative().all(|e| e.essential),
        "transformative properties are essential"
    );
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    let expect = oxideav_heif::compose::apply_imir(
        &oxideav_heif::compose::apply_irot(&src, &Irot::new(1)).unwrap(),
        &Imir::new(1),
    )
    .unwrap();
    assert_eq!(img.frame, expect);

    // The identity-item path stays available for callers that want a
    // transform on a derived item.
    let mut w = HeifWriter::new();
    let colour = oxideav_heif::encode::to_yuv420_8(&src).unwrap();
    let pic = oxideav_heif::encode::encode_hevc_picture(&colour, "pcm", 0).unwrap();
    let base = w.add_coded_item(
        pic.item_type,
        pic.data,
        vec![
            (pic.config.clone(), true),
            (
                Property::Ispe(oxideav_heif::props::Ispe::new(48, 32)),
                false,
            ),
            (
                Property::Colr(oxideav_heif::props::Colr::MIAF_DEFAULT),
                false,
            ),
        ],
    );
    w.set_hidden(base, true);
    let iden = w.add_identity(
        base,
        vec![
            (
                Property::Ispe(oxideav_heif::props::Ispe::new(48, 32)),
                false,
            ),
            (Property::Irot(Irot::new(1)), true),
        ],
    );
    w.set_primary(iden);
    let f = HeifFile::from_vec(w.write_to_vec().unwrap()).unwrap();
    assert_eq!(f.primary_item().unwrap().item_type, *b"iden");
    assert!(check(&f, MiafProfile::Miaf).unwrap().is_conformant());
}

#[test]
fn av1_round_trip_is_exact() {
    let src = picture(64, 32);
    let opts = EncodeOptions::default().with_codec(StillCodec::Av1);
    let bytes = encode_still(&src, &opts).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert!(f.file_type.has_brand(b"avif"));
    assert_eq!(f.primary_item().unwrap().item_type, *b"av01");
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, src);
}

#[test]
fn framework_encoder_emits_a_decodable_file() {
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(32);
    params.height = Some(32);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.options = oxideav_core::CodecOptions::new().set("mode", "pcm");
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    let src = picture(32, 32);
    let (vf, _) = src.to_core().unwrap();
    enc.send_frame(&Frame::Video(vf)).unwrap();
    let pkt = enc.receive_packet().unwrap();
    assert!(pkt.is_keyframe());
    let mut dec = ctx
        .codecs
        .first_decoder(&CodecParameters::video(CodecId::new("heif")))
        .unwrap();
    dec.send_packet(&pkt).unwrap();
    let Frame::Video(out) = dec.receive_frame().unwrap() else {
        panic!("video frame expected");
    };
    let back = HeifFrame::from_core(&out, 32, 32, PixelFormat::Yuv420P).unwrap();
    assert_eq!(back, src);
    // The written file probes as HEIF.
    let mut cur = std::io::Cursor::new(pkt.data.clone());
    assert_eq!(ctx.containers.probe_input(&mut cur, None).unwrap(), "heif");
}

#[test]
fn written_file_decodes_in_a_black_box_decoder() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        return;
    }
    let src = picture(64, 48);
    let bytes = encode_still(&src, &lossless()).unwrap();
    let dir = std::env::temp_dir().join(format!("oxideav-heif-writer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let heic = dir.join("out.heic");
    let raw = dir.join("out.raw");
    std::fs::write(&heic, &bytes).unwrap();
    let st = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(&heic)
        .args(["-f", "rawvideo"])
        .arg(&raw)
        .status()
        .unwrap();
    assert!(st.success(), "black-box decoder refused the written file");
    let got = std::fs::read(&raw).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let mut ours = Vec::new();
    for p in &src.tight().planes {
        ours.extend_from_slice(&p.data);
    }
    assert_eq!(
        got, ours,
        "black-box decode of the written file differs from the source"
    );
}

/// AV1 items keep the picture's own (depth, chroma) pairing: a 10-bit
/// 4:2:0 and a monochrome source round-trip exactly (lossless), a
/// 4:4:4 + alpha one carries a monochrome alpha still, and a lossy
/// quality lands close to the source with a smaller file.
#[test]
fn av1_native_layouts_and_quality() {
    use oxideav_heif::rgb::to_rgb;
    let mut ten = HeifFrame::zeroed(
        64,
        48,
        HeifPixelFormat::new(Chroma::Yuv420, 10, false).unwrap(),
    )
    .unwrap();
    for y in 0..48 {
        for x in 0..64 {
            ten.set_sample(0, x, y, ((x * 13 + y * 7) % 1024) as u16);
        }
    }
    for p in 1..3 {
        for y in 0..24 {
            for x in 0..32 {
                ten.set_sample(p, x, y, ((x * 11 + y * 5 + p as u32 * 300) % 1024) as u16);
            }
        }
    }
    let av1 = EncodeOptions::default().with_codec(StillCodec::Av1);
    let f = HeifFile::from_vec(encode_still(&ten, &av1).unwrap()).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, ten, "10-bit 4:2:0 lossless");
    let mono = HeifFrame::filled(
        32,
        32,
        HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap(),
        77,
    )
    .unwrap();
    let f = HeifFile::from_vec(encode_still(&mono, &av1).unwrap()).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, mono, "monochrome lossless");
    // 4:4:4 + alpha: the alpha auxiliary is a monochrome av01 item.
    let mut src = HeifFrame::zeroed(
        32,
        32,
        HeifPixelFormat::new(Chroma::Yuv444, 8, true).unwrap(),
    )
    .unwrap();
    for y in 0..32 {
        for x in 0..32 {
            src.set_sample(0, x, y, (x * 8) as u16);
            src.set_sample(1, x, y, 128);
            src.set_sample(2, x, y, (y * 8) as u16);
            src.set_sample(3, x, y, (255 - x * 4) as u16);
        }
    }
    let f = HeifFile::from_vec(encode_still(&src, &av1).unwrap()).unwrap();
    let node = oxideav_heif::derived::build_primary_graph(&f).unwrap();
    let a = node.alpha.as_ref().unwrap();
    assert!(a.properties.av1c().unwrap().monochrome, "alpha coded 4:0:0");
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, src, "4:4:4 + alpha lossless");
    // Lossy: quality 40 is smaller than lossless and within a few codes.
    let lossless = encode_still(&src.without_alpha(), &av1).unwrap();
    let lossy = encode_still(
        &src.without_alpha(),
        &av1.clone().with_av1_quality(Some(40)),
    )
    .unwrap();
    assert!(
        lossy.len() < lossless.len(),
        "{} vs {}",
        lossy.len(),
        lossless.len()
    );
    let img = decode_primary(&HeifFile::parse(&lossy).unwrap(), ItemDecoder::direct()).unwrap();
    let (a, b) = (
        to_rgb(&img.frame, Some(&img.nclx)).unwrap(),
        to_rgb(&src.without_alpha(), Some(&img.nclx)).unwrap(),
    );
    let mse: f64 = a
        .data
        .iter()
        .zip(&b.data)
        .map(|(x, y)| (*x as f64 - *y as f64).powi(2))
        .sum::<f64>()
        / a.data.len() as f64;
    let psnr = 10.0 * (255.0f64 * 255.0 / mse).log10();
    assert!(psnr >= 30.0, "quality 40 PSNR {psnr:.1} dB");
    // quality 100 through the dial is lossless too.
    let q100 = encode_still(&src.without_alpha(), &av1.with_av1_quality(Some(100))).unwrap();
    let img = decode_primary(&HeifFile::parse(&q100).unwrap(), ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, src.without_alpha());
}

/// HEIF Amd 1:2025 §6.8.11 / §6.8.12 / §6.8.13 entity groups
/// (`stem`, `pymd`, `rgpa`) and the Amd 1 descriptive properties
/// (`reve`, `ndwt`, `dadj`, `stag`, `cexg`) write and read back typed;
/// MIAF Amd 1:2025 Annex A.3 / A.4 display order collapses `altr`
/// groups.
#[test]
fn amd1_entity_groups_and_properties_round_trip() {
    use oxideav_heif::meta::{PyramidInfo, StereoFallbackPosition};
    use oxideav_heif::props::{Cexg, Dadj, Ndwt, Reve, Stag, StereoAggressor};
    let mut w = HeifWriter::new();
    let mut ids = Vec::new();
    for i in 0..4u32 {
        let pic = oxideav_heif::encode::encode_hevc_picture(&picture(64, 48), "pcm", 0).unwrap();
        let mut props = vec![
            (pic.config.clone(), true),
            (
                Property::Ispe(oxideav_heif::props::Ispe::new(64, 48)),
                false,
            ),
        ];
        if i == 0 {
            props.push((
                Property::Reve(Reve::new(50_000, 3127, 3290, 10_000, 3127, 3290)),
                false,
            ));
            props.push((Property::Ndwt(Ndwt::new(2_030_000)), false));
            props.push((Property::Dadj(Dadj::new(-25)), false));
            props.push((
                Property::Stag(Stag::new(vec![StereoAggressor::new(4, 90, None)])),
                false,
            ));
            props.push((Property::Cexg(Cexg::new(1, 1, 64, 48, false, None)), false));
        }
        ids.push(w.add_coded_item(*b"hvc1", pic.data.clone(), props));
    }
    w.set_primary(ids[0]);
    w.add_stereo_with_fallback(100, ids[0], ids[1], ids[2], StereoFallbackPosition::Centre);
    w.add_pyramid(101, vec![ids[3], ids[0]], &[(64, 48)]);
    w.add_pyramid(102, vec![ids[3], ids[1]], &[(32, 24), (64, 48)]);
    w.add_entity_group_with_payload(*b"rgpa", 103, 0x2000, vec![], {
        let mut b = Vec::new();
        for v in [64u16, 48, 8, 4, 32, 16] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b
    });
    w.add_entity_group(*b"altr", 104, vec![ids[2], ids[1]]);
    let bytes = w.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let meta = f.meta().unwrap();
    let g = |id: u32| {
        meta.entity_groups
            .iter()
            .find(|g| g.group_id == id)
            .unwrap()
    };
    assert_eq!(
        g(100).stereo_with_fallback(),
        Some((ids[0], ids[1], ids[2], StereoFallbackPosition::Centre))
    );
    assert_eq!(g(100).stereo_pair(), Some((ids[0], ids[1])));
    let p = g(101).pyramid().unwrap().unwrap();
    assert_eq!(p.tile_sizes, vec![(64, 48)]);
    assert_eq!(p.tile_size(1), Some((64, 48)));
    assert_eq!(
        g(101).flags,
        PyramidInfo::FLAG_TILE_INFO_PRESENT | PyramidInfo::FLAG_TILE_INFO_CONSTANT
    );
    let p2 = g(102).pyramid().unwrap().unwrap();
    assert_eq!(p2.tile_size(0), Some((32, 24)));
    assert_eq!(p2.tile_size(1), Some((64, 48)));
    let area = g(103).region_partition_area().unwrap().unwrap();
    assert_eq!(
        (area.top, area.left, area.width, area.height),
        (8, 4, 32, 16)
    );
    assert!(g(104).pyramid().unwrap().is_none());
    // Display order: item 0, then the altr group at item 1's slot
    // (its entities in group order), item 3.
    assert_eq!(
        meta.display_order(),
        vec![vec![ids[0]], vec![ids[2], ids[1]], vec![ids[3]]]
    );
    let props = oxideav_heif::props::ItemProperties::resolve(meta, ids[0]).unwrap();
    assert_eq!(props.reve().map(|r| r.surround_luminance), Some(50_000));
    assert_eq!(
        props.ndwt().map(|n| n.diffuse_white_luminance),
        Some(2_030_000)
    );
    assert_eq!(props.dadj().map(|d| d.disparity_adjustment), Some(-25));
    assert_eq!(props.stags().len(), 1);
    assert_eq!(props.stags()[0].aggressors[0].severity, 90);
    assert_eq!(props.cexg().map(|c| c.tile_count()), Some(1));
    // Unknown-to-MIAF descriptive properties do not break conformance.
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, picture(64, 48));
}

/// `rd` / `tiles` without an explicit `ctb` pick the quadtree coder's
/// CTB size automatically (r462 matrix gap: the encoder refused them),
/// directly and through the framework options; an explicit `ctb` is
/// honoured and a size outside 16 / 32 / 64 is refused.
#[test]
fn hevc_rd_and_tiles_choose_the_ctb_automatically() {
    use oxideav_heif::encode::auto_ctb;
    assert_eq!(auto_ctb(4032, 3024, Some("4x4")), 64);
    assert_eq!(auto_ctb(96, 80, Some("4x4")), 16);
    assert_eq!(auto_ctb(96, 80, Some("2x2")), 64);
    assert_eq!(auto_ctb(96, 80, Some("3x3")), 32);
    assert_eq!(auto_ctb(96, 80, None), 64);
    let src = picture(128, 96);
    for (rd, tiles, ctb) in [
        (Some(2), None, None),
        (None, Some("2x2"), None),
        (Some(1), Some("4x2"), None),
        (Some(0), Some("2x2"), Some(16)),
    ] {
        let opts = EncodeOptions::default()
            .with_hevc_rd(rd)
            .with_hevc_tiles(tiles.map(str::to_string))
            .with_hevc_ctb(ctb);
        let bytes = encode_still(&src, &opts).unwrap_or_else(|e| panic!("{rd:?} {tiles:?}: {e}"));
        let img = decode_primary(&HeifFile::parse(&bytes).unwrap(), ItemDecoder::direct()).unwrap();
        assert_eq!((img.width(), img.height()), (128, 96));
    }
    let ctx = {
        let mut c = RuntimeContext::new();
        oxideav_h265::register(&mut c);
        oxideav_heif::register(&mut c);
        c
    };
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(128);
    params.height = Some(96);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.options = oxideav_core::CodecOptions::new()
        .set("rd", "1")
        .set("tiles", "2x2");
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    let (vf, _) = src.to_core().unwrap();
    enc.send_frame(&Frame::Video(vf)).unwrap();
    enc.flush().unwrap();
    let file = enc.receive_packet().unwrap().data;
    assert!(decode_primary(&HeifFile::parse(&file).unwrap(), ItemDecoder::direct()).is_ok());
    params.options = oxideav_core::CodecOptions::new().set("ctb", "48");
    assert!(
        ctx.codecs.first_encoder(&params).is_err() || {
            let mut e = ctx.codecs.first_encoder(&params).unwrap();
            e.send_frame(&Frame::Video(src.to_core().unwrap().0))
                .is_err()
        }
    );
}

/// HEIF Amd 2:2026 §11.3.5: `unrg` / `corg` groups over region items
/// (`rgan`, each with a `cdsc` to the image) are written, surfaced
/// through the typed accessors and `region_groups_of`, and the
/// clause rules (region items only, one image, `corg` ≥ 2) fire in
/// `check`.
#[test]
fn region_groups_round_trip_and_are_checked() {
    use oxideav_heif::meta::ITEM_TYPE_RGAN;
    let bytes = encode_still(&picture(64, 48), &lossless()).unwrap();
    let region_body = |x: u16, y: u16| -> Vec<u8> {
        // A minimal RegionItem body: version 0, flags 0 (16-bit
        // fields), reference size, one rectangle geometry (type 1).
        let mut b = vec![0u8, 0];
        for v in [64u16, 48] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.push(1); // region_count
        b.push(1); // geometry_type: rectangle
        for v in [x, y, 8, 8] {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b
    };
    let rebuild = |extra: &dyn Fn(&mut HeifWriter, u32, &[u32])| -> Vec<u8> {
        let f = HeifFile::parse(&bytes).unwrap();
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        let mut w = HeifWriter::new();
        let master =
            oxideav_heif::encode::encode_still_into(&mut w, &img.frame, &lossless()).unwrap();
        w.set_primary(master);
        let regions: Vec<u32> = (0..3)
            .map(|i| w.add_metadata_item(master, ITEM_TYPE_RGAN, None, region_body(i * 16, 0)))
            .collect();
        extra(&mut w, master, &regions);
        w.write_to_vec().unwrap()
    };
    let good = rebuild(&|w, _, r| {
        w.add_region_union(200, r.to_vec());
        w.add_compound_region(201, r[0], vec![r[1], r[2]]);
    });
    let f = HeifFile::parse(&good).unwrap();
    let meta = f.meta().unwrap();
    let primary = meta.primary_item_id.unwrap();
    let regions = meta.region_items_of(primary);
    assert_eq!(regions.len(), 3);
    let groups = meta.region_groups_of(primary);
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].region_union(), Some(regions.as_slice()));
    assert_eq!(groups[0].compound_region(), None);
    assert_eq!(
        groups[1].compound_region(),
        Some((regions[0], &regions[1..]))
    );
    assert_eq!(groups[1].region_union(), None);
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(
        !rep.violations
            .iter()
            .any(|v| v.clause.starts_with("HEIF-A2 11.3.5")),
        "{:?}",
        rep.violations
    );
    // Violations: a corg with one entity, a non-region entity, regions
    // of two different images.
    let bad = rebuild(&|w, master, r| {
        w.add_compound_region(202, r[0], vec![]);
        w.add_region_union(203, vec![r[0], master]);
        // A second image: an `iden` over the master.
        let other = w.add_identity(
            master,
            vec![(
                Property::Ispe(oxideav_heif::props::Ispe::new(64, 48)),
                false,
            )],
        );
        let far = w.add_metadata_item(other, ITEM_TYPE_RGAN, None, region_body(0, 16));
        w.add_region_union(204, vec![r[0], far]);
    });
    let f = HeifFile::parse(&bad).unwrap();
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    let clauses: Vec<&str> = rep
        .violations
        .iter()
        .filter(|v| v.clause.starts_with("HEIF-A2 11.3.5"))
        .map(|v| v.message.as_str())
        .collect();
    assert!(
        clauses.iter().any(|m| m.contains("corg group 202")),
        "{clauses:?}"
    );
    assert!(
        clauses.iter().any(|m| m.contains("unrg group 203")),
        "{clauses:?}"
    );
    assert!(
        clauses.iter().any(|m| m.contains("unrg group 204")),
        "{clauses:?}"
    );
    // Only the (short) corg is a group of the primary's regions: the
    // groups that reach the master item or another image's region are
    // not.
    let meta = f.meta().unwrap();
    let ids: Vec<u32> = meta
        .region_groups_of(meta.primary_item_id.unwrap())
        .iter()
        .map(|g| g.group_id)
        .collect();
    assert_eq!(ids, vec![202]);
}

/// The brands claim only what the file satisfies: `miaf` when the §7 /
/// §8 general requirements hold, the Annex A profile brands of the
/// primary's coding family when their codec constraints hold (MIAF
/// §7.3.2, A.3 – A.6, AVIF §7), and nothing of that on a file this
/// crate's own checker rejects. Profile brands are cumulative (a MIAF
/// HEVC Basic file is also an Advanced and an Extended one).
#[test]
fn brands_are_claimed_truthfully() {
    use oxideav_heif::ftyp::{
        BRAND_MA1A, BRAND_MA1B, BRAND_MIAB, BRAND_MIAF, BRAND_MIHA, BRAND_MIHB, BRAND_MIHE,
    };
    let brands = |bytes: &[u8]| -> Vec<String> {
        HeifFile::parse(bytes)
            .unwrap()
            .file_type
            .compatible_brands
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect()
    };
    let has = |bytes: &[u8], b: &[u8; 4]| HeifFile::parse(bytes).unwrap().file_type.has_brand(b);
    let pic = picture(64, 48);
    // HEVC 8-bit 4:2:0: every HEVC profile.
    let hevc8 = encode_still(&pic, &EncodeOptions::default()).unwrap();
    for b in [BRAND_MIAF, BRAND_MIHB, BRAND_MIHA, BRAND_MIHE] {
        assert!(
            has(&hevc8, &b),
            "hevc 8-bit lacks {}",
            String::from_utf8_lossy(&b)
        );
    }
    assert!(!has(&hevc8, &BRAND_MA1B) && !has(&hevc8, &BRAND_MIAB));
    // HEVC Main 10: Advanced + Extended, not Basic.
    let hevc10 = encode_still(&pic, &EncodeOptions::default().with_hevc_depth(Some(10))).unwrap();
    assert!(!has(&hevc10, &BRAND_MIHB));
    assert!(has(&hevc10, &BRAND_MIHA) && has(&hevc10, &BRAND_MIHE) && has(&hevc10, &BRAND_MIAF));
    // Lossless RGB rides an identity-matrix 4:4:4 item: Extended only.
    let rgb_bytes: Vec<u8> = (0..16 * 16)
        .flat_map(|i| [(i % 16 * 16) as u8, (i / 16 * 16) as u8, 200u8])
        .collect();
    let hevc444 = oxideav_heif::encode_rgb8(16, 16, &rgb_bytes, &lossless()).unwrap();
    assert!(!has(&hevc444, &BRAND_MIHB) && !has(&hevc444, &BRAND_MIHA));
    assert!(has(&hevc444, &BRAND_MIHE) && has(&hevc444, &BRAND_MIAF));
    let mut rgb = HeifFrame::zeroed(
        16,
        16,
        HeifPixelFormat::new(Chroma::Yuv444, 8, false).unwrap(),
    )
    .unwrap();
    for y in 0..16 {
        for x in 0..16 {
            rgb.set_sample(0, x, y, (x * 16) as u16);
            rgb.set_sample(1, x, y, (y * 16) as u16);
            rgb.set_sample(2, x, y, 200);
        }
    }
    // AV1 4:2:0 (seq_profile 0): Basic + Advanced; 4:4:4 (profile 1):
    // Advanced only.
    let av1 = encode_still(&pic, &EncodeOptions::default().with_codec(StillCodec::Av1)).unwrap();
    assert!(has(&av1, &BRAND_MA1B) && has(&av1, &BRAND_MA1A) && has(&av1, &BRAND_MIAF));
    let av1_444 =
        encode_still(&rgb, &EncodeOptions::default().with_codec(StillCodec::Av1)).unwrap();
    assert!(!has(&av1_444, &BRAND_MA1B) && has(&av1_444, &BRAND_MA1A));
    // AVC: the profile brand only with a Progressive / Constrained High
    // record (A.6.2); the record the registry encoder writes decides.
    let avc = encode_still(&pic, &EncodeOptions::default().with_codec(StillCodec::Avc)).unwrap();
    let f = HeifFile::parse(&avc).unwrap();
    let rep = oxideav_heif::miaf::check(&f, oxideav_heif::miaf::MiafProfile::AvcBasic).unwrap();
    assert_eq!(has(&avc, &BRAND_MIAB), rep.is_conformant());
    assert!(has(&avc, &BRAND_MIAF));
    // A hand-built file the checker rejects (the primary item is a
    // thumbnail, §7.3.2 — the shape of conformance file C005) gets
    // neither `miaf` nor a profile brand — but the caller's explicit
    // brands are written as given.
    let hvcc = HeifFile::parse(&hevc8)
        .unwrap()
        .meta()
        .unwrap()
        .properties_of(1)
        .into_iter()
        .map(|(_, raw, _)| Property::parse(raw).unwrap())
        .find(|p| matches!(p, Property::HvcC(_)))
        .unwrap();
    let data = HeifFile::parse(&hevc8).unwrap().item_data_owned(1).unwrap();
    let ispe = Property::Ispe(oxideav_heif::props::Ispe::new(64, 48));
    let coded_props = vec![(hvcc.clone(), true), (ispe.clone(), false)];
    let mut w = oxideav_heif::HeifWriter::new();
    let master = w.add_coded_item(*b"hvc1", data.clone(), coded_props.clone());
    let thumb = w.add_thumbnail(master, *b"hvc1", data.clone(), coded_props.clone());
    w.set_primary(thumb);
    let bad = w.write_to_vec().unwrap();
    assert!(!has(&bad, &BRAND_MIAF) && !has(&bad, &BRAND_MIHB));
    assert!(has(&bad, b"heic") && has(&bad, b"mif1"));
    let forced = w
        .clone()
        .with_brands(*b"heic", vec![*b"mif1", *b"miaf", *b"MiHB"])
        .write_to_vec()
        .unwrap();
    assert_eq!(brands(&forced), ["mif1", "miaf", "MiHB"]);
    // The sequence writer: a conformant sequence keeps `miaf` and gains
    // the cover's profile brands; one whose cover still fails the
    // checks keeps the structural brands only.
    let mut sw = oxideav_heif::writer::SequenceWriter::new(*b"hvc1", hvcc.clone(), 64, 48, 1000);
    sw.push_sample(data.clone(), 100, true);
    let mut cover = oxideav_heif::HeifWriter::new();
    let cid = cover.add_coded_item(*b"hvc1", Vec::new(), coded_props.clone());
    cover.set_primary(cid);
    sw.still = Some(cover);
    sw.cover_sample = Some(0);
    let seq = sw.write_to_vec().unwrap();
    assert!(has(&seq, &BRAND_MIAF) && has(&seq, b"msf1") && has(&seq, b"mif1"));
    assert!(has(&seq, &BRAND_MIHB));
    // (Every non-aliased item carries bytes: the master here is not
    // the cover_sample alias, so it keeps a real body.)
    let mut bad_cover = oxideav_heif::HeifWriter::new();
    let cid = bad_cover.add_coded_item(*b"hvc1", data.clone(), coded_props.clone());
    let thumb = bad_cover.add_thumbnail(cid, *b"hvc1", data.clone(), coded_props.clone());
    bad_cover.set_primary(thumb);
    sw.still = Some(bad_cover);
    let seq_bad = sw.write_to_vec().unwrap();
    assert!(!has(&seq_bad, &BRAND_MIAF) && !has(&seq_bad, &BRAND_MIHB));
    assert!(has(&seq_bad, b"msf1") && has(&seq_bad, b"hevc"));
    let _ = oxideav_heif::decode(&seq).unwrap();
}

/// Apply a transformative chain given as plain properties.
fn transformed(f: &HeifFrame, chain: &[Property]) -> HeifFrame {
    let entries: Vec<oxideav_heif::props::PropertyEntry> = chain
        .iter()
        .enumerate()
        .map(|(i, p)| oxideav_heif::props::PropertyEntry::new(i as u16 + 1, true, p.clone()))
        .collect();
    oxideav_heif::compose::apply_transforms(f, entries.iter(), false).unwrap()
}

/// A picture with an alpha plane (4:2:0, 8-bit).
fn picture_with_alpha(w: u32, h: u32) -> HeifFrame {
    let base = picture(w, h);
    let mut a =
        HeifFrame::zeroed(w, h, HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap()).unwrap();
    for y in 0..h {
        for x in 0..w {
            a.set_sample(0, x, y, ((x * 255) / w.max(1)) as u16);
        }
    }
    base.with_alpha_plane(&a).unwrap()
}

/// `TransformCarrier::Identity` writes the chain on an `iden` over the
/// un-rotated coded picture (alpha on the input without transforms, the
/// thumbnail with them): the file is MIAF-conformant, the `iden` is the
/// primary, and it decodes — pixels and alpha — exactly like the
/// default carrier's file, which decodes exactly like the transformed
/// source.
#[test]
fn identity_carrier_round_trips_like_the_coded_carrier() {
    use oxideav_heif::encode::TransformCarrier;
    let src = picture_with_alpha(48, 32);
    let chain = vec![Property::Irot(Irot::new(1)), Property::Imir(Imir::new(1))];
    let base = lossless()
        .with_transforms(chain.clone())
        .with_thumbnail_max_dim(Some(16));
    let coded = encode_still(&src, &base).unwrap();
    let iden = encode_still(
        &src,
        &base
            .clone()
            .with_transform_carrier(TransformCarrier::Identity),
    )
    .unwrap();
    let want = transformed(&src, &chain);
    for (name, bytes) in [("coded", &coded), ("iden", &iden)] {
        let f = HeifFile::parse(bytes).unwrap();
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{name}: {:#?}", rep.violations);
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        assert_eq!(img.frame, want, "{name}: pixels + alpha after the chain");
        let meta = f.meta().unwrap();
        let primary = meta.primary_item().unwrap();
        let primary_type = if name == "iden" { *b"iden" } else { *b"hvc1" };
        assert_eq!(primary.item_type, primary_type);
        // The thumbnail hangs off the primary and carries the chain.
        let thumbs = meta.thumbnails_of(primary.id);
        assert_eq!(thumbs.len(), 1, "{name}");
        let tprops: Vec<oxideav_heif::boxes::FourCc> = meta
            .properties_of(thumbs[0])
            .iter()
            .map(|(_, raw, _)| raw.box_type)
            .collect();
        assert!(
            tprops.contains(b"irot") && tprops.contains(b"imir"),
            "{name}: {tprops:?}"
        );
        if name == "iden" {
            // The coded input and its alpha carry no transforms; the
            // `iden` carries both, essential.
            let input = meta.derivation_inputs(primary.id)[0];
            for id in [input, meta.auxiliaries_of(input)[0]] {
                let types: Vec<_> = meta
                    .properties_of(id)
                    .iter()
                    .map(|(_, raw, _)| raw.box_type)
                    .collect();
                assert!(
                    !types.contains(b"irot") && !types.contains(b"imir"),
                    "{types:?}"
                );
            }
            let on_iden: Vec<(oxideav_heif::boxes::FourCc, bool)> = meta
                .properties_of(primary.id)
                .iter()
                .map(|(_, raw, e)| (raw.box_type, *e))
                .collect();
            assert!(on_iden.contains(&(*b"irot", true)) && on_iden.contains(&(*b"imir", true)));
            assert!(on_iden.contains(&(*b"ispe", false)));
        }
        // Framework path: same pixels.
        let frames = oxideav_heif::decode_all(bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].image.to_frame().unwrap(),
            want,
            "{name}: contract decode"
        );
    }
}

/// `EncodeOptions::depth_map` writes a depth auxiliary (HEIF §7.5.3) the
/// reader surfaces as `DecodedImage::depth`, exact under the lossless
/// mode on HEVC and AV1, at its own size; the alpha auxiliary stays
/// next to it.
#[test]
fn depth_map_round_trips_as_an_auxiliary() {
    let src = picture_with_alpha(32, 32);
    let mut depth = HeifFrame::zeroed(
        16,
        16,
        HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap(),
    )
    .unwrap();
    for y in 0..16 {
        for x in 0..16 {
            depth.set_sample(0, x, y, ((x + y) * 8) as u16);
        }
    }
    for codec in [StillCodec::Hevc, StillCodec::Av1] {
        let opts = lossless()
            .with_codec(codec)
            .with_depth_map(Some(depth.clone()));
        let bytes = encode_still(&src, &opts).unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{codec:?}: {:#?}", rep.violations);
        let meta = f.meta().unwrap();
        let primary = meta.primary_item().unwrap().id;
        let auxes = meta.auxiliaries_of(primary);
        assert_eq!(auxes.len(), 2, "{codec:?}: alpha + depth");
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        assert_eq!(img.frame, src, "{codec:?}: picture + alpha");
        let got = img
            .depth
            .as_ref()
            .unwrap_or_else(|| panic!("{codec:?}: no depth"));
        assert_eq!(
            (got.width, got.height),
            (16, 16),
            "{codec:?}: depth geometry"
        );
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(
                    got.sample(0, x, y),
                    depth.sample(0, x, y),
                    "{codec:?} ({x},{y})"
                );
            }
        }
        // The depth item: hidden, one channel, the codec's depth URN,
        // its own parameter sets on HEVC (ImageIO refuses shared ones).
        let depth_id = auxes
            .iter()
            .copied()
            .find(|id| {
                meta.properties_of(*id).iter().any(|(_, raw, _)| {
                    raw.box_type == *b"auxC" && String::from_utf8_lossy(&raw.body).contains("depth")
                        || raw.box_type == *b"auxC"
                            && String::from_utf8_lossy(&raw.body).contains("auxid:2")
                })
            })
            .expect("a depth auxC");
        assert!(meta.item(depth_id).unwrap().is_hidden());
        if codec == StillCodec::Hevc {
            let mut configs: Vec<Vec<u8>> = Vec::new();
            for id in [primary, auxes[0], auxes[1]] {
                let c = meta
                    .properties_of(id)
                    .into_iter()
                    .find(|(_, raw, _)| raw.box_type == *b"hvcC")
                    .map(|(_, raw, _)| raw.body.clone())
                    .unwrap();
                assert!(!configs.contains(&c), "parameter sets shared between items");
                configs.push(c);
            }
        }
    }
}

/// `encode_overlay` writes an `iovl` over per-layer coded items (alpha
/// auxiliaries where the layers have alpha), the canvas fill and the
/// offsets; lossless, the primary decodes to exactly what the
/// compositor produces from the source layers, is MIAF-conformant,
/// carries the transforms on the `iovl` (or on an `iden` over it) and a
/// thumbnail of the composed canvas when asked.
#[test]
fn overlay_round_trips_against_the_compositor() {
    use oxideav_heif::compose::{composite_overlay, OverlayInput};
    use oxideav_heif::derived::OverlayDescriptor;
    use oxideav_heif::encode::{encode_overlay, OverlayLayer, OverlaySpec, TransformCarrier};
    let a = picture(32, 24);
    let b = picture_with_alpha(16, 16);
    let spec = OverlaySpec::new(
        48,
        40,
        [20000, 40000, 60000, 65535],
        vec![
            OverlayLayer::new(a.clone(), 4, 2),
            OverlayLayer::new(b.clone(), 24, 20),
        ],
    );
    let desc = OverlayDescriptor::new(spec.fill, 48, 40, vec![(4, 2), (24, 20)]);
    let colr = EncodeOptions::default().colr;
    let want = composite_overlay(
        &desc,
        &[OverlayInput::new(&a, false), OverlayInput::new(&b, false)],
        Some(&colr),
    )
    .unwrap();
    for (codec, carrier) in [
        (StillCodec::Hevc, TransformCarrier::CodedItem),
        (StillCodec::Av1, TransformCarrier::CodedItem),
        (StillCodec::Hevc, TransformCarrier::Identity),
    ] {
        let chain = vec![Property::Irot(Irot::new(3))];
        let opts = lossless()
            .with_codec(codec)
            .with_transforms(chain.clone())
            .with_thumbnail_max_dim(Some(24))
            .with_transform_carrier(carrier);
        let bytes = encode_overlay(&spec, &opts).unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(
            rep.is_conformant(),
            "{codec:?} {carrier:?}: {:#?}",
            rep.violations
        );
        let meta = f.meta().unwrap();
        let primary = meta.primary_item().unwrap();
        let expect_type = if carrier == TransformCarrier::Identity {
            *b"iden"
        } else {
            *b"iovl"
        };
        assert_eq!(primary.item_type, expect_type, "{codec:?} {carrier:?}");
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        let rotated = transformed(&want, &chain);
        assert_eq!(
            (img.frame.width, img.frame.height),
            (40, 48),
            "{codec:?} {carrier:?}"
        );
        assert_eq!(img.frame, rotated, "{codec:?} {carrier:?}: composite");
        assert_eq!(
            meta.thumbnails_of(primary.id).len(),
            1,
            "{codec:?} {carrier:?}: thumbnail"
        );
        // The layers: two coded items, the second with an alpha auxiliary.
        let iovl = if carrier == TransformCarrier::Identity {
            meta.derivation_inputs(primary.id)[0]
        } else {
            primary.id
        };
        let inputs = meta.derivation_inputs(iovl);
        assert_eq!(inputs.len(), 2);
        assert_eq!(meta.auxiliaries_of(inputs[0]).len(), 0);
        assert_eq!(meta.auxiliaries_of(inputs[1]).len(), 1);
        // Contract path sees the same composite.
        let frames = oxideav_heif::decode_all(&bytes).unwrap();
        assert_eq!(
            frames[0].image.to_frame().unwrap(),
            rotated,
            "{codec:?} {carrier:?}"
        );
    }
    // Refusals: no layers; a gain map on the overlay path.
    assert!(encode_overlay(&OverlaySpec::new(8, 8, [0; 4], vec![]), &lossless()).is_err());
}

/// The framework `irot` / `imir` options write the chain on the primary
/// (essential), the decoded picture is the transformed source, and the
/// bytes equal the contract encode with the same `transforms`.
#[test]
fn framework_irot_imir_options_write_the_chain() {
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let src = picture(48, 32);
    let (vf, _) = src.to_core().unwrap();
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(48);
    params.height = Some(32);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.options = oxideav_core::CodecOptions::new()
        .set("mode", "pcm")
        .set("irot", "1")
        .set("imir", "horizontal");
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    enc.send_frame(&Frame::Video(vf)).unwrap();
    enc.flush().unwrap();
    let pkt = enc.receive_packet().unwrap();
    let chain = vec![Property::Irot(Irot::new(1)), Property::Imir(Imir::new(1))];
    let contract = encode_still(&src, &lossless().with_transforms(chain.clone())).unwrap();
    assert_eq!(pkt.data, contract, "framework bytes = contract bytes");
    let f = HeifFile::parse(&pkt.data).unwrap();
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, transformed(&src, &chain));
    // Refusals: irot out of range, an unknown imir.
    let bad = |k: &str, v: &str| {
        let mut p = params.clone();
        p.options = oxideav_core::CodecOptions::new().set(k, v);
        ctx.codecs.first_encoder(&p).is_err()
    };
    assert!(bad("irot", "4"));
    assert!(bad("imir", "diagonal"));
}

/// A `clap` in `transforms` on a picture the codec pads: the padding
/// crop and the caller's crop fold into ONE exact `clap` on the coded
/// item (and its alpha), the thumbnail is cut from the cropped picture
/// and carries the orientation only, the depth map and the gain map
/// carry the orientation only — every item passes the MIAF §7.3.6.7
/// order check and the primary decodes to the cropped, rotated source.
/// Chains out of that order are typed refusals (fuzz-found: two `clap`s
/// on one item).
#[test]
fn clap_folds_with_the_padding_and_stays_off_the_companions() {
    use oxideav_heif::props::{Clap, CropRect};
    let src = picture_with_alpha(46, 46);
    let crop = CropRect {
        x: 5,
        y: 3,
        width: 30,
        height: 20,
    };
    let chain = vec![
        Property::Clap(Clap::for_rect(46, 46, crop)),
        Property::Irot(Irot::new(1)),
    ];
    let mut depth =
        HeifFrame::zeroed(8, 8, HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap()).unwrap();
    for y in 0..8 {
        for x in 0..8 {
            depth.set_sample(0, x, y, (x * 32) as u16);
        }
    }
    let want = {
        let cropped = oxideav_heif::compose::crop(&src, 5, 3, 30, 20).unwrap();
        oxideav_heif::compose::apply_irot(&cropped, &Irot::new(1)).unwrap()
    };
    for codec in [StillCodec::Hevc, StillCodec::Av1] {
        let opts = lossless()
            .with_codec(codec)
            .with_transforms(chain.clone())
            .with_thumbnail_max_dim(Some(10))
            .with_depth_map(Some(depth.clone()));
        let bytes = encode_still(&src, &opts).unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{codec:?}: {:#?}", rep.violations);
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        assert_eq!(
            img.frame, want,
            "{codec:?}: cropped + rotated, alpha included"
        );
        let meta = f.meta().unwrap();
        let primary = meta.primary_item().unwrap().id;
        let types = |id: u32| -> Vec<oxideav_heif::boxes::FourCc> {
            meta.properties_of(id)
                .iter()
                .map(|(_, raw, _)| raw.box_type)
                .filter(|t| matches!(t, b"clap" | b"irot" | b"imir"))
                .collect()
        };
        // One clap on the primary and on its alpha; orientation only on
        // the thumbnail and the depth map.
        assert_eq!(
            types(primary),
            vec![*b"clap", *b"irot"],
            "{codec:?}: primary"
        );
        let auxes = meta.auxiliaries_of(primary);
        assert_eq!(auxes.len(), 2);
        for a in &auxes {
            let is_depth = meta.properties_of(*a).iter().any(|(_, raw, _)| {
                raw.box_type == *b"auxC" && String::from_utf8_lossy(&raw.body).contains("depth")
                    || raw.box_type == *b"auxC"
                        && String::from_utf8_lossy(&raw.body).contains("auxid:2")
            });
            let t = types(*a);
            if is_depth {
                // Orientation only — plus the map's own padding crop
                // when the codec pads it (HEVC: 8 → 16), never the
                // primary's.
                assert!(
                    t == vec![*b"irot"] || t == vec![*b"clap", *b"irot"],
                    "{codec:?}: depth map {t:?}"
                );
            } else if codec == StillCodec::Hevc {
                // HEVC pads 46 → 48: the folded clap, then irot.
                assert_eq!(t, vec![*b"clap", *b"irot"], "{codec:?}: alpha");
            }
        }
        let thumb = meta.thumbnails_of(primary)[0];
        let t = types(thumb);
        assert!(
            t.last() == Some(b"irot") && !t.contains(b"clap") || t == vec![*b"clap", *b"irot"],
            "{codec:?}: thumbnail {t:?}"
        );
        // The thumbnail pictures the cropped region: 30×20 → 10×7 (its
        // own padding clap when the codec needs one).
        let timg = oxideav_heif::decode::decode_item(&f, thumb, ItemDecoder::direct()).unwrap();
        assert_eq!(
            (timg.frame.width, timg.frame.height),
            (7, 10),
            "{codec:?}: thumbnail geometry (rotated)"
        );
        // Depth survives at its own size.
        assert_eq!(
            img.depth.as_ref().map(|d| (d.width, d.height)),
            Some((8, 8))
        );
    }
    // Refusals: the order and the multiplicity.
    for bad in [
        vec![
            Property::Irot(Irot::new(1)),
            Property::Clap(Clap::for_rect(46, 46, crop)),
        ],
        vec![Property::Irot(Irot::new(1)), Property::Irot(Irot::new(2))],
        vec![Property::Imir(Imir::new(0)), Property::Irot(Irot::new(1))],
    ] {
        assert!(encode_still(&src, &lossless().with_transforms(bad)).is_err());
    }
}

/// An overlay whose 4:2:0 layers mix odd and even sizes (fuzz-found): an
/// odd-sized layer comes out of its padding `clap` promoted to 4:4:4
/// (MIAF §7.3.6.7), an even-sized one stays 4:2:0, and the compositor
/// must take the mixed samplings (promoting the composite) rather than
/// refuse a MIAF-conformant file. Lossless, the composite equals the
/// compositor's own result on the source layers.
#[test]
fn overlay_composes_layers_of_mixed_chroma_after_their_transforms() {
    use oxideav_heif::compose::{composite_overlay, OverlayInput};
    use oxideav_heif::derived::OverlayDescriptor;
    use oxideav_heif::encode::{encode_overlay, OverlayLayer, OverlaySpec};
    let odd = picture_with_alpha(3, 15);
    let odd2 = picture_with_alpha(7, 15);
    let even = picture_with_alpha(14, 12);
    let spec = OverlaySpec::new(
        20,
        18,
        [1000, 2000, 3000, 65535],
        vec![
            OverlayLayer::new(odd.clone(), 0, 0),
            OverlayLayer::new(odd2.clone(), 4, 1),
            OverlayLayer::new(even.clone(), 6, 6),
        ],
    );
    let bytes = encode_overlay(&spec, &lossless()).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    let desc = OverlayDescriptor::new(spec.fill, 20, 18, vec![(0, 0), (4, 1), (6, 6)]);
    let want = composite_overlay(
        &desc,
        &[
            OverlayInput::new(&odd, false),
            OverlayInput::new(&odd2, false),
            OverlayInput::new(&even, false),
        ],
        Some(&EncodeOptions::default().colr),
    )
    .unwrap();
    assert_eq!(img.frame, want);
    // The contract path and the framework path agree.
    assert_eq!(
        oxideav_heif::decode(&bytes).unwrap().to_frame().unwrap(),
        want
    );
}

/// The `hvcC` and coded bytes of a small lossless HEVC still, for
/// hand-built files.
fn hevc_record_and_data() -> (Property, Vec<u8>) {
    let bytes = encode_still(&picture(64, 64), &lossless()).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let meta = f.meta().unwrap();
    let id = meta.primary_item_id.unwrap();
    let hvcc = meta
        .properties_of(id)
        .into_iter()
        .map(|(_, raw, _)| Property::parse(raw).unwrap())
        .find(|p| matches!(p, Property::HvcC(_)))
        .unwrap();
    (hvcc, f.item_data_owned(id).unwrap())
}

/// The `(version, flags)` of the file's `ipma` box.
fn ipma_version_flags(bytes: &[u8]) -> (u8, u32) {
    let pos = bytes
        .windows(4)
        .position(|w| w == b"ipma")
        .expect("ipma box");
    let fb = &bytes[pos + 4..pos + 8];
    (fb[0], u32::from_be_bytes([0, fb[1], fb[2], fb[3]]))
}

/// ISO/IEC 14496-12 §8.11.14: the `ipco` is a pool. A grid of 1 024
/// tiles coded alike carries one `hvcC`, one tile `ispe`, one `pixi`
/// and one `colr` — every tile's `ipma` row indexes the same entries
/// — and the `ipma` stays in its 7-bit form.
#[test]
fn ipco_is_a_pool_shared_by_every_item() {
    use oxideav_heif::derived::GridDescriptor;
    use oxideav_heif::props::{Colr, Ispe, Pixi};

    let (hvcc, data) = hevc_record_and_data();
    let tile_props = || {
        vec![
            (hvcc.clone(), true),
            (Property::Ispe(Ispe::new(64, 64)), false),
            (Property::Pixi(Pixi::new(vec![8, 8, 8])), false),
            (Property::Colr(Colr::MIAF_DEFAULT), false),
        ]
    };
    let mut w = HeifWriter::new();
    let tiles: Vec<u32> = (0..1024)
        .map(|_| w.add_coded_item(*b"hvc1", data.clone(), tile_props()))
        .collect();
    let grid = w
        .add_grid(
            GridDescriptor::new(32, 32, 64 * 32, 64 * 32),
            &tiles,
            vec![(Property::Colr(Colr::MIAF_DEFAULT), false)],
        )
        .unwrap();
    w.set_primary(grid);
    let bytes = w.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let meta = f.meta().unwrap();
    assert_eq!(meta.items.len(), 1025);
    // hvcC, tile ispe, pixi, colr, grid ispe.
    assert_eq!(meta.properties.len(), 5, "{:?}", meta.properties);
    assert_eq!(ipma_version_flags(&bytes), (0, 0));
    let first: Vec<(u16, bool)> = meta
        .properties_of(tiles[0])
        .into_iter()
        .map(|(i, _, e)| (i, e))
        .collect();
    assert_eq!(first.len(), 4);
    for t in &tiles[1..] {
        let row: Vec<(u16, bool)> = meta
            .properties_of(*t)
            .into_iter()
            .map(|(i, _, e)| (i, e))
            .collect();
        assert_eq!(row, first, "tile {t} indexes the shared entries");
    }
    // The tiles' bytes are carried as given, and a re-mux keeps them.
    assert_eq!(f.item_data(tiles[1023]).unwrap().as_ref(), &data[..]);
    let again = HeifWriter::from_files(&[&bytes])
        .unwrap()
        .write_to_vec()
        .unwrap();
    let g = HeifFile::parse(&again).unwrap();
    let gm = g.meta().unwrap();
    assert_eq!(gm.properties.len(), 5);
    assert_eq!(gm.items.len(), 1025);
    assert_eq!(check(&g, MiafProfile::Miaf).unwrap().violations, vec![]);
}

/// §8.11.14: `ipma` flags bit 0 selects 15-bit property indices, to be
/// set only when the `ipco` holds more than 127 entries. On each side
/// of the boundary the writer picks the right form and the reader
/// resolves every association.
#[test]
fn ipma_switches_to_15_bit_indices_past_127_properties() {
    use oxideav_heif::props::{Ispe, Pasp};

    let (hvcc, data) = hevc_record_and_data();
    let file_with = |extra: usize| {
        let mut props = vec![
            (hvcc.clone(), true),
            (Property::Ispe(Ispe::new(64, 64)), false),
        ];
        for i in 0..extra as u32 {
            props.push((Property::Pasp(Pasp::new(1 + i, 1)), false));
        }
        let mut w = HeifWriter::new();
        let id = w.add_coded_item(*b"hvc1", data.clone(), props);
        w.set_primary(id);
        (w.write_to_vec().unwrap(), id)
    };
    for (extra, total, large) in [(125usize, 127usize, false), (126, 128, true)] {
        let (bytes, id) = file_with(extra);
        assert_eq!(
            ipma_version_flags(&bytes),
            (0, large as u32),
            "{total} properties"
        );
        let f = HeifFile::parse(&bytes).unwrap();
        let meta = f.meta().unwrap();
        assert_eq!(meta.properties.len(), total);
        let row = meta.properties_of(id);
        assert_eq!(row.len(), total);
        // Indices resolve 1..=total in association order; every pasp
        // comes back with its own spacing.
        for (k, (idx, raw, _)) in row.iter().enumerate() {
            assert_eq!(*idx as usize, k + 1);
            if k >= 2 {
                match Property::parse(raw).unwrap() {
                    Property::Pasp(p) => assert_eq!(p.h_spacing as usize, k - 1),
                    other => panic!("{other:?}"),
                }
            }
        }
        // The file decodes (the pasp entries are descriptive).
        let img = oxideav_heif::decode(&bytes).unwrap();
        assert_eq!((img.width, img.height), (64, 64));
    }
    // association_count is one byte: a 256-entry row is refused.
    let mut props = vec![
        (hvcc.clone(), true),
        (Property::Ispe(Ispe::new(64, 64)), false),
    ];
    for i in 0..254u32 {
        props.push((Property::Pasp(Pasp::new(1 + i, 1)), false));
    }
    let mut w = HeifWriter::new();
    let id = w.add_coded_item(*b"hvc1", data.clone(), props);
    w.set_primary(id);
    let err = w.write_to_vec().unwrap_err().to_string();
    assert!(err.contains("255"), "{err}");
}

/// An `iloc` extent of length 0 means "to the end of the source"
/// (14496-12 §8.11.3.2.4.3), so no item is ever written with an empty
/// body — the writer refuses each body kind by type.
#[test]
fn empty_item_bodies_are_refused_at_write_time() {
    use oxideav_heif::props::Ispe;
    use oxideav_heif::writer::ItemBody;

    let (hvcc, data) = hevc_record_and_data();
    let props = || {
        vec![
            (hvcc.clone(), true),
            (Property::Ispe(Ispe::new(64, 64)), false),
        ]
    };
    let refused = |w: &HeifWriter, what: &str| {
        let err = w.write_to_vec().unwrap_err().to_string();
        assert!(err.contains("empty body"), "{what}: {err}");
    };
    // Coded.
    let mut w = HeifWriter::new();
    let id = w.add_coded_item(*b"hvc1", Vec::new(), props());
    w.set_primary(id);
    refused(&w, "coded");
    // Metadata items: an empty XMP packet, an empty raw Exif body.
    let mut w = HeifWriter::new();
    let id = w.add_coded_item(*b"hvc1", data.clone(), props());
    w.set_primary(id);
    w.add_xmp(id, "");
    refused(&w, "xmp");
    let mut w = HeifWriter::new();
    let id = w.add_coded_item(*b"hvc1", data.clone(), props());
    w.set_primary(id);
    w.add_exif_raw(id, Vec::new());
    refused(&w, "exif");
    // Constrained extents: one empty tile among full ones.
    let mut w = HeifWriter::new();
    let id = w
        .add_constrained_extents_item(
            *b"hvc1",
            1,
            2,
            64,
            48,
            vec![data.clone(), Vec::new()],
            props(),
        )
        .unwrap();
    w.set_primary(id);
    refused(&w, "cexg");
    // The same shapes with bytes write, and the alias path (a cover
    // still whose primary points at a track sample) still carries an
    // empty placeholder body that the sequence writer re-targets.
    let mut w = HeifWriter::new();
    let id = w.add_coded_item(*b"hvc1", data.clone(), props());
    w.set_primary(id);
    w.add_xmp(id, "<x:xmpmeta/>");
    let bytes = w.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    for loc in &f.meta().unwrap().locations {
        for e in &loc.extents {
            assert_ne!(e.length, 0, "item {}: no zero-length extent", loc.item_id);
        }
    }
    let mut cover = HeifWriter::new();
    let cid = cover.add_coded_item(*b"hvc1", Vec::new(), props());
    cover.set_primary(cid);
    assert!(matches!(cover.items()[0].body, ItemBody::Coded(ref d) if d.is_empty()));
    let mut sw = oxideav_heif::writer::SequenceWriter::new(*b"hvc1", hvcc.clone(), 64, 64, 10);
    sw.push_sample(data.clone(), 1, true);
    sw.still = Some(cover);
    sw.cover_sample = Some(0);
    let seq = sw.write_to_vec().unwrap();
    let f = HeifFile::parse(&seq).unwrap();
    let loc = &f.meta().unwrap().locations;
    assert!(loc
        .iter()
        .all(|l| l.extents.iter().all(|e| e.length == data.len() as u64)));
    let _ = oxideav_heif::decode(&seq).unwrap();
}

/// HEIF §6.6.2.2 / §6.6.2.3: a `grid` has 1..=256 rows and columns
/// (one byte each, minus one) and an `iovl` one or more inputs, both
/// over a non-empty canvas — a zero-row grid used to underflow the
/// `rows_minus_one` byte and a 257-row one to wrap it.
#[test]
fn degenerate_derived_items_are_refused() {
    use oxideav_heif::derived::{GridDescriptor, OverlayDescriptor};
    use oxideav_heif::props::Ispe;

    let (hvcc, data) = hevc_record_and_data();
    let props = || {
        vec![
            (hvcc.clone(), true),
            (Property::Ispe(Ispe::new(64, 64)), false),
        ]
    };
    let mut w = HeifWriter::new();
    let tile = w.add_coded_item(*b"hvc1", data.clone(), props());
    for (rows, cols, tiles) in [
        (0u16, 1u16, 0usize),
        (1, 0, 0),
        (257, 1, 257),
        (1, 257, 257),
    ] {
        let ids = vec![tile; tiles];
        let err = w
            .add_grid(GridDescriptor::new(rows, cols, 64, 64), &ids, Vec::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("1..=256"), "{rows}x{cols}: {err}");
    }
    let err = w
        .add_grid(GridDescriptor::new(1, 1, 0, 64), &[tile], Vec::new())
        .unwrap_err()
        .to_string();
    assert!(err.contains("zero output size"), "{err}");
    let err = w
        .add_overlay(
            OverlayDescriptor::new([0; 4], 64, 64, Vec::new()),
            &[],
            Vec::new(),
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("no input images"), "{err}");
    // The 1 × 1 grid and the one-layer overlay are the minimal legal
    // shapes and write.
    let g = w
        .add_grid(GridDescriptor::new(1, 1, 64, 64), &[tile], Vec::new())
        .unwrap();
    w.set_primary(g);
    assert!(w.write_to_vec().is_ok());
}
