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
    EncodeOptions {
        hevc_mode: "pcm".into(),
        ..EncodeOptions::default()
    }
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
    let opts = EncodeOptions {
        grid_tile: Some(32),
        thumbnail_max_dim: Some(20),
        exif: Some(b"II*\0\x08\0\0\0\0\0".to_vec()),
        xmp: Some("<x:xmpmeta>t</x:xmpmeta>".into()),
        ..lossless()
    };
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
    let opts = EncodeOptions {
        transforms: vec![
            Property::Irot(Irot { angle: 1 }),
            Property::Imir(Imir { axis: 1 }),
        ],
        ..lossless()
    };
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
        &oxideav_heif::compose::apply_irot(&src, &Irot { angle: 1 }).unwrap(),
        &Imir { axis: 1 },
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
                Property::Ispe(oxideav_heif::props::Ispe {
                    width: 48,
                    height: 32,
                }),
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
                Property::Ispe(oxideav_heif::props::Ispe {
                    width: 48,
                    height: 32,
                }),
                false,
            ),
            (Property::Irot(Irot { angle: 1 }), true),
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
    let opts = EncodeOptions {
        codec: StillCodec::Av1,
        ..EncodeOptions::default()
    };
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
    let av1 = EncodeOptions {
        codec: StillCodec::Av1,
        ..EncodeOptions::default()
    };
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
        &EncodeOptions {
            av1_quality: Some(40),
            ..av1.clone()
        },
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
    let q100 = encode_still(
        &src.without_alpha(),
        &EncodeOptions {
            av1_quality: Some(100),
            ..av1
        },
    )
    .unwrap();
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
                Property::Ispe(oxideav_heif::props::Ispe {
                    width: 64,
                    height: 48,
                }),
                false,
            ),
        ];
        if i == 0 {
            props.push((
                Property::Reve(Reve {
                    surround_luminance: 50_000,
                    surround_light_x: 3127,
                    surround_light_y: 3290,
                    periphery_luminance: 10_000,
                    periphery_light_x: 3127,
                    periphery_light_y: 3290,
                }),
                false,
            ));
            props.push((
                Property::Ndwt(Ndwt {
                    diffuse_white_luminance: 2_030_000,
                }),
                false,
            ));
            props.push((
                Property::Dadj(Dadj {
                    disparity_adjustment: -25,
                }),
                false,
            ));
            props.push((
                Property::Stag(Stag {
                    aggressors: vec![StereoAggressor {
                        aggressor_type: 4,
                        severity: 90,
                        sub_type_uri: None,
                    }],
                }),
                false,
            ));
            props.push((
                Property::Cexg(Cexg {
                    rows: 1,
                    columns: 1,
                    tile_width: 64,
                    tile_height: 48,
                    large_fields: false,
                    extent_config: None,
                }),
                false,
            ));
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
        let opts = EncodeOptions {
            hevc_rd: rd,
            hevc_tiles: tiles.map(str::to_string),
            hevc_ctb: ctb,
            ..EncodeOptions::default()
        };
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
                Property::Ispe(oxideav_heif::props::Ispe {
                    width: 64,
                    height: 48,
                }),
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
