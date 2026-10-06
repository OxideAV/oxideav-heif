//! VVC image items (HEIF Annex L) through `oxideav-h266`: `vvcC`
//! records, `vvc1` items wrapped by the writer from the workspace
//! encoder's Annex B output, decoded through the contract path, the
//! direct item decoder and the framework codec (byte-identical), MIAF /
//! Annex L conformance, and a black-box VVC decoder cross-check.
#![cfg(feature = "registry")]

mod common;

use oxideav_core::{CodecId, CodecParameters, Frame, RuntimeContext};
use oxideav_heif::decode::{decode_primary, ItemDecoder};
use oxideav_heif::meta::ITEM_TYPE_VVC1;
use oxideav_heif::miaf::{check, MiafProfile};
use oxideav_heif::props::{Colr, Ispe, Pixi, Property};
use oxideav_heif::vvcdec::vvc_item_from_annex_b;
use oxideav_heif::{HeifFile, HeifWriter, PixelFormat};

/// An 8×8 8-bit 4:2:0 IDR picture coded by `oxideav-h266`'s encoder
/// (VPS, SPS, PPS, PH, one IDR_N_LP slice), as NAL units.
const VPS: [u8; 11] = [
    0x00, 0x71, 0x10, 0x00, 0x00, 0x03, 0x02, 0x5a, 0x80, 0x00, 0x40,
];
const SPS: [u8; 16] = [
    0x00, 0x79, 0x00, 0x0c, 0x04, 0x89, 0x22, 0x02, 0xdc, 0x3d, 0x30, 0x30, 0x10, 0x40, 0x00, 0x20,
];
const PPS: [u8; 8] = [0x00, 0x81, 0x00, 0x02, 0x44, 0x89, 0x86, 0x08];
const PH: [u8; 5] = [0x00, 0x99, 0x88, 0x00, 0xb8];
const SLICE: [u8; 24] = [
    0x00, 0x41, 0x20, 0x02, 0x30, 0xe3, 0x2b, 0xa6, 0x10, 0xba, 0xea, 0x81, 0x35, 0x9c, 0x7d, 0xdf,
    0x25, 0xd7, 0x6f, 0x11, 0x26, 0x45, 0xdc, 0x5e,
];

fn annex_b_8x8() -> Vec<u8> {
    let mut v = Vec::new();
    for n in [&VPS[..], &SPS, &PPS, &PH, &SLICE] {
        v.extend_from_slice(&[0, 0, 0, 1]);
        v.extend_from_slice(n);
    }
    v
}

fn fixture_128() -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vvc/still-128x128-q18.266"),
    )
    .unwrap()
}

/// Wrap one VVC access unit as a `vvic` still (the writer's brand
/// selection learns VVC with the encoder; this pins the reader).
fn wrap(annex_b: &[u8]) -> Vec<u8> {
    let (cfg, data, w, h, layout) = vvc_item_from_annex_b(annex_b).unwrap();
    assert_eq!(cfg.length_size, 4);
    assert!(cfg.ptl_present_flag);
    assert_eq!(cfg.nal_count(), 3, "VPS + SPS + PPS in the record");
    let mut wr = HeifWriter::new().with_brands(*b"vvic", vec![*b"mif1", *b"vvic", *b"miaf"]);
    let id = wr.add_coded_item(
        ITEM_TYPE_VVC1,
        data,
        vec![
            (Property::VvcC(cfg), true),
            (Property::Ispe(Ispe::new(w, h)), false),
            (Property::Pixi(Pixi::new(vec![layout.bit_depth; 3])), false),
            (Property::Colr(Colr::MIAF_DEFAULT), false),
        ],
    );
    wr.set_primary(id);
    wr.write_to_vec().unwrap()
}

fn planes_of(img: &oxideav_heif::HeifImage) -> Vec<u8> {
    let mut v = Vec::new();
    for p in &img.planes {
        v.extend_from_slice(&p.data);
    }
    v
}

#[test]
fn vvc_items_decode_through_every_path_byte_identical() {
    let mut ctx = RuntimeContext::new();
    oxideav_heif::register(&mut ctx);
    for (annex_b, side) in [(annex_b_8x8(), 8u32), (fixture_128(), 128)] {
        let bytes = wrap(&annex_b);
        assert!(oxideav_heif::probe(&bytes));
        let f = HeifFile::parse(&bytes).unwrap();
        assert!(f.file_type.has_brand(b"vvic"));
        assert!(f.file_type.classify().vvc);
        let primary = f.primary_item().unwrap();
        assert_eq!(primary.item_type, ITEM_TYPE_VVC1);
        assert!(primary.is_coded_image());
        // Header only.
        let info = oxideav_heif::info(&bytes).unwrap();
        assert_eq!((info.width, info.height), (side, side));
        assert_eq!(info.format, PixelFormat::Yuv420P);
        // Annex L conformance on top of MIAF's general requirements.
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{:#?}", rep.violations);
        // Contract path.
        let img = oxideav_heif::decode(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (side, side));
        assert_eq!(img.format, PixelFormat::Yuv420P);
        // Direct item decoder.
        let direct = decode_primary(&f, ItemDecoder::direct()).unwrap();
        assert_eq!(direct.frame.width, side);
        let mut direct_planes = Vec::new();
        for p in &direct.frame.tight().planes {
            direct_planes.extend_from_slice(&p.data);
        }
        assert_eq!(planes_of(&img), direct_planes);
        // Framework codec over the whole-file packet.
        let mut dparams = CodecParameters::video(CodecId::new("heif"));
        dparams.width = Some(side);
        dparams.height = Some(side);
        let mut dec = ctx.codecs.first_decoder(&dparams).unwrap();
        dec.send_packet(
            &oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 1), bytes.clone())
                .with_keyframe(true),
        )
        .unwrap();
        dec.flush().unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            panic!("video frame");
        };
        let (want, pf) = img.clone().into_video_frame();
        // Full-range 8-bit YCbCr carries the framework's YuvJ label.
        assert_eq!(pf, oxideav_core::PixelFormat::YuvJ420P);
        assert_eq!(vf.planes.len(), want.planes.len());
        for (a, b) in vf.planes.iter().zip(&want.planes) {
            assert_eq!((a.stride, &a.data), (b.stride, &b.data));
        }
        // The record survives the property layer byte-exact.
        let props =
            oxideav_heif::props::ItemProperties::resolve(f.meta().unwrap(), primary.id).unwrap();
        let cfg = props.vvcc().expect("vvcC");
        let (cfg2, _, _, _, _) = vvc_item_from_annex_b(&annex_b).unwrap();
        assert_eq!(cfg.to_bytes(), cfg2.to_bytes());
        assert_eq!(cfg.native_ptl.as_ref().unwrap().general_profile_idc, 1);
        let boxed = oxideav_heif::props::write::property_box(&Property::VvcC(cfg.clone()));
        assert_eq!(&boxed[4..8], b"vvcC");
        assert_eq!(boxed[8], 0, "FullBox version 0");
    }
}

#[test]
fn vvc_items_match_a_black_box_decoder() {
    if !common::have_binary("ffmpeg") {
        eprintln!("ffmpeg not present; black-box check skipped");
        return;
    }
    let dir = common::scratch_dir("vvc-blackbox");
    for (annex_b, name) in [(annex_b_8x8(), "s8"), (fixture_128(), "s128")] {
        let bytes = wrap(&annex_b);
        let img = oxideav_heif::decode(&bytes).unwrap();
        let ours = planes_of(&img);
        // The VVC decoder over the bare access unit: byte-exact.
        let es = dir.join(format!("{name}.266"));
        let raw = dir.join(format!("{name}.yuv"));
        std::fs::write(&es, &annex_b).unwrap();
        let st = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
            .arg(&es)
            .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
            .arg(&raw)
            .status()
            .unwrap();
        assert!(st.success(), "black-box VVC decoder refused {name}");
        assert_eq!(std::fs::read(&raw).unwrap(), ours, "{name}: planes differ");
        // The same tool over the HEIF container: reported, not asserted
        // (its HEIF demuxer may not route vvc1 items).
        let heif = dir.join(format!("{name}.heif"));
        let raw2 = dir.join(format!("{name}.heif.yuv"));
        std::fs::write(&heif, &bytes).unwrap();
        let out = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
            .arg(&heif)
            .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
            .arg(&raw2)
            .output()
            .unwrap();
        match (out.status.success(), std::fs::read(&raw2)) {
            (true, Ok(got)) if got == ours => {
                eprintln!("{name}: ffmpeg opens the vvic file, byte-exact")
            }
            (true, Ok(got)) => eprintln!(
                "{name}: ffmpeg opens the vvic file, {} vs {} bytes differ",
                got.len(),
                ours.len()
            ),
            _ => eprintln!(
                "{name}: ffmpeg does not decode the vvic container: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────── encode ─────────────────────────

use oxideav_heif::encode::{encode_still, encode_still_minimized, EncodeOptions, StillCodec};
use oxideav_heif::{HeifFrame, HeifPixelFormat};

/// A gentle 8-bit 4:2:0 picture (gradients + a mild texture): the
/// workspace VVC encoder's intra prediction is DC-only, so the
/// corpus' sawtooth pattern lands at ~23 dB at every QP; this one lets
/// a QP bound mean something.
fn gentle(w: u32, h: u32) -> HeifFrame {
    let mut f = HeifFrame::zeroed(
        w,
        h,
        HeifPixelFormat::new(oxideav_heif::Chroma::Yuv420, 8, false).unwrap(),
    )
    .unwrap();
    for y in 0..h {
        for x in 0..w {
            let g = (x * 200 / w.max(1) + y * 200 / h.max(1)) / 2 + 20;
            let t = ((x / 4 + y / 4) % 2) * 6;
            f.set_sample(0, x, y, (g + t).min(255) as u16);
        }
    }
    let (cw, ch) = f.plane_dims(1);
    for y in 0..ch {
        for x in 0..cw {
            f.set_sample(1, x, y, (100 + x * 60 / cw.max(1)) as u16);
            f.set_sample(2, x, y, (160 - y * 60 / ch.max(1)) as u16);
        }
    }
    f
}

fn psnr_y(a: &HeifFrame, b: &HeifFrame) -> f64 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let mut se = 0f64;
    for y in 0..a.height {
        for x in 0..a.width {
            let d = a.sample(0, x, y) as f64 - b.sample(0, x, y) as f64;
            se += d * d;
        }
    }
    let mse = se / (a.width as f64 * a.height as f64);
    10.0 * (255.0f64 * 255.0 / mse.max(1e-6)).log10()
}

fn vvc() -> EncodeOptions {
    EncodeOptions::default().with_codec(StillCodec::Vvc)
}

fn tight_bytes(f: &HeifFrame) -> Vec<u8> {
    let mut v = Vec::new();
    for p in &f.tight().planes {
        v.extend_from_slice(&p.data);
    }
    v
}

/// ffmpeg's VVC decoder over a coded item's access unit (record NAL
/// units + item data), as yuv420p planes of the *coded* picture.
fn ffmpeg_item(f: &HeifFile, item_id: u32, tag: &str) -> Option<Vec<u8>> {
    if !common::have_binary("ffmpeg") {
        return None;
    }
    let props = oxideav_heif::props::ItemProperties::resolve(f.meta().unwrap(), item_id).unwrap();
    let cfg = props.vvcc().expect("vvcC");
    let data = f.item_data(item_id).unwrap();
    let au = oxideav_heif::vvcc::access_unit_annex_b(cfg, &data, props.tols()).unwrap();
    let dir = common::scratch_dir(&format!("vvc-item-{tag}"));
    let es = dir.join("au.266");
    let raw = dir.join("au.yuv");
    std::fs::write(&es, &au).unwrap();
    let st = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(&es)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg(&raw)
        .status()
        .unwrap();
    assert!(
        st.success(),
        "{tag}: black-box VVC decoder refused item {item_id}"
    );
    let got = std::fs::read(&raw).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    Some(got)
}

/// Every coded `vvc1` item of `f` decodes byte-exact in the black-box
/// decoder (when present).
fn assert_items_black_box_exact(f: &HeifFile, tag: &str) {
    let meta = f.meta().unwrap();
    for it in meta.items.iter().filter(|i| i.item_type == ITEM_TYPE_VVC1) {
        let node = oxideav_heif::build_graph(f, it.id).unwrap();
        let ours = ItemDecoder::direct().decode_coded(f, &node).unwrap();
        if let Some(got) = ffmpeg_item(f, it.id, &format!("{tag}-{}", it.id)) {
            assert_eq!(
                got,
                tight_bytes(&ours),
                "{tag}: item {} differs from the black-box decode",
                it.id
            );
        }
    }
}

fn contract_equals_direct(bytes: &[u8], direct: &HeifFrame) {
    let img = oxideav_heif::decode(bytes).unwrap();
    assert_eq!(planes_of(&img), tight_bytes(direct));
}

#[test]
fn vvc_encode_round_trips_every_size() {
    for (w, h) in [
        (64u32, 64u32),
        (96, 80),
        (128, 128),
        (7, 5),
        (1, 1),
        (320, 192),
    ] {
        let src = gentle(w, h);
        let bytes = encode_still(&src, &vvc()).unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        let tag = format!("{w}x{h}");
        assert_eq!(f.file_type.major_brand, *b"vvic", "{tag}");
        assert!(
            f.file_type.has_brand(b"mif1") && f.file_type.has_brand(b"miaf"),
            "{tag}"
        );
        let primary = f.primary_item().unwrap();
        assert_eq!(primary.item_type, ITEM_TYPE_VVC1, "{tag}");
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{tag}: {:#?}", rep.violations);
        let props =
            oxideav_heif::props::ItemProperties::resolve(f.meta().unwrap(), primary.id).unwrap();
        let cfg = props.vvcc().unwrap();
        let (cw, ch) = (cfg.max_picture_width as u32, cfg.max_picture_height as u32);
        // Padded to the encoder's 64-sample grid, cropped back by clap.
        assert_eq!((cw % 64, ch % 64), (0, 0), "{tag}: coded {cw}x{ch}");
        assert_eq!(props.clap().is_some(), (cw, ch) != (w, h), "{tag}");
        assert_eq!(
            props.ispe().map(|i| (i.width, i.height)),
            Some((cw, ch)),
            "{tag}"
        );
        let info = oxideav_heif::info(&bytes).unwrap();
        assert_eq!((info.width, info.height), (w, h), "{tag}");
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        assert_eq!((img.frame.width, img.frame.height), (w, h), "{tag}");
        if w * h >= 64 {
            let p = psnr_y(&src, &img.frame.tight());
            assert!(p >= 30.0, "{tag}: PSNR_Y {p:.2} dB at qp 18");
        }
        contract_equals_direct(&bytes, &img.frame);
        assert_items_black_box_exact(&f, &tag);
    }
}

#[test]
fn vvc_refuses_pcm_and_checks_tool_options() {
    let src = gentle(64, 64);
    let pcm = vvc().with_hevc_mode("pcm".into());
    assert!(matches!(
        encode_still(&src, &pcm),
        Err(oxideav_heif::HeifError::Unsupported(_))
    ));
    let bad = vvc().with_vvc_options(vec![("nonsense".into(), "1".into())]);
    assert!(encode_still(&src, &bad).is_err());
    let bad = vvc().with_vvc_options(vec![("tiles".into(), "x".into())]);
    assert!(encode_still(&src, &bad).is_err());
    // Tool axes the encoder exposes: each stream decodes here and in
    // the black-box decoder, byte-exact.
    // A tile grid needs that many 128-sample CTUs (the encoder's CTU
    // size): 256x128 is 2x1 CTUs.
    for (name, (w, h), opts) in [
        (
            "tiles2x1+wpp",
            (256u32, 128u32),
            vec![("tiles", "2x1"), ("wpp", "1")],
        ),
        ("dep_quant", (128, 128), vec![("dep_quant", "1")]),
        ("sdh", (128, 128), vec![("sdh", "1")]),
        ("mtt", (128, 128), vec![("mtt_bt", "1"), ("mtt_tt", "1")]),
    ] {
        let big = gentle(w, h);
        let o = vvc().with_vvc_options(
            opts.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        let bytes = encode_still(&big, &o).unwrap_or_else(|e| panic!("{name}: {e}"));
        let f = HeifFile::parse(&bytes).unwrap();
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        let p = psnr_y(&big, &img.frame.tight());
        assert!(p >= 30.0, "{name}: PSNR_Y {p:.2} dB");
        assert_items_black_box_exact(&f, name);
    }
}

#[test]
fn vvc_alpha_grid_overlay_and_gain_map() {
    // Alpha: a colour item + an auxiliary VVC item under the CICP URN.
    let (w, h) = (128u32, 96u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[
                (x * 255 / w) as u8,
                (y * 255 / h) as u8,
                128,
                if x < w / 2 { 255 } else { 64 },
            ]);
        }
    }
    let bytes = oxideav_heif::encode_rgba8(w, h, &rgba, &vvc()).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "alpha: {:#?}", rep.violations);
    let meta = f.meta().unwrap();
    let alpha_items: Vec<_> = meta
        .items
        .iter()
        .filter(|i| {
            oxideav_heif::props::ItemProperties::resolve(meta, i.id)
                .ok()
                .and_then(|p| p.auxc().map(|a| a.aux_type.clone()))
                .as_deref()
                == Some(oxideav_heif::props::AUX_URN_ALPHA)
        })
        .collect();
    assert_eq!(alpha_items.len(), 1);
    assert_eq!(alpha_items[0].item_type, ITEM_TYPE_VVC1);
    let back = oxideav_heif::decode_rgba8(&bytes).unwrap();
    assert_eq!((back.width, back.height), (w, h));
    let alpha_err: f64 = rgba
        .chunks_exact(4)
        .zip(back.data.chunks_exact(4))
        .map(|(a, b)| (a[3] as f64 - b[3] as f64).abs())
        .sum::<f64>()
        / (w * h) as f64;
    assert!(alpha_err < 1.0, "alpha mean error {alpha_err:.3}");
    let img = oxideav_heif::decode(&bytes).unwrap();
    assert!(img.format.has_alpha(), "{:?}", img.format);
    assert_items_black_box_exact(&f, "alpha");

    // Grid: tiles are floored to twice the codec alignment (128 for
    // VVC), so 256x256 in 128-px tiles → four vvc1 tiles under a grid.
    let src = gentle(256, 256);
    let bytes = encode_still(&src, &vvc().with_grid_tile(Some(128))).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert!(check(&f, MiafProfile::Miaf).unwrap().is_conformant());
    let root = oxideav_heif::build_primary_graph(&f).unwrap();
    assert!(matches!(root.kind, oxideav_heif::ImageKind::Grid(_)));
    assert_eq!(root.inputs.len(), 4);
    assert!(root
        .inputs
        .iter()
        .all(|i| i.item.item_type == ITEM_TYPE_VVC1));
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    let p = psnr_y(&src, &img.frame.tight());
    assert!(p >= 30.0, "grid PSNR_Y {p:.2}");
    contract_equals_direct(&bytes, &img.frame);
    // Parallel tile decode is byte-identical.
    let par = decode_primary(&f, ItemDecoder::direct().with_threads(4)).unwrap();
    assert_eq!(par.frame, img.frame);
    assert_items_black_box_exact(&f, "grid");

    // Overlay: two coded VVC pictures composed by the writer.
    let a = oxideav_heif::encode::encode_vvc_picture_owned(gentle(64, 64), &vvc()).unwrap();
    let b = oxideav_heif::encode::encode_vvc_picture_owned(gentle(64, 64), &vvc()).unwrap();
    let mut wr = HeifWriter::new();
    let coded_props = |pic: &oxideav_heif::encode::CodedPicture| {
        vec![
            (pic.config.clone(), true),
            (
                Property::Ispe(Ispe::new(pic.coded_width, pic.coded_height)),
                false,
            ),
            (Property::Pixi(Pixi::new(vec![8, 8, 8])), false),
            (Property::Colr(Colr::MIAF_DEFAULT), false),
        ]
    };
    let ia = wr.add_coded_item(a.item_type, a.data.clone(), coded_props(&a));
    let ib = wr.add_coded_item(b.item_type, b.data.clone(), coded_props(&b));
    let desc = oxideav_heif::OverlayDescriptor::new([0, 0, 0, 0], 96, 64, vec![(0, 0), (32, 0)]);
    let io = wr
        .add_overlay(
            desc,
            &[ia, ib],
            vec![(Property::Colr(Colr::MIAF_DEFAULT), false)],
        )
        .unwrap();
    wr.set_primary(io);
    let bytes = wr.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert!(f.file_type.has_brand(b"vvic"));
    assert!(check(&f, MiafProfile::Miaf).unwrap().is_conformant());
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!((img.frame.width, img.frame.height), (96, 64));
    contract_equals_direct(&bytes, &img.frame);
    assert_items_black_box_exact(&f, "overlay");

    // Gain map: a tmap over a VVC base and a VVC gain-map item, the
    // metadata from the vendored black-box tool's file.
    let gm_src = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gainmap/gm_rgb.avif"),
    )
    .unwrap();
    let gm_file = HeifFile::parse(&gm_src).unwrap();
    let base = decode_primary(&gm_file, ItemDecoder::direct()).unwrap();
    let gm = base.gain_map.as_ref().unwrap();
    let Some(Colr::Nclx {
        matrix, full_range, ..
    }) = gm.colr.clone()
    else {
        panic!("gain map colr");
    };
    let spec = oxideav_heif::encode::GainMapSpec::new(
        gm.frame.clone(),
        gm.metadata.clone(),
        gm.alternate_colr.clone().unwrap(),
        matrix,
        full_range,
        None,
        12,
    );
    let opts = vvc().with_colr(base.nclx.clone()).with_gain_map(Some(spec));
    let bytes = encode_still(&base.frame, &opts).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert!(f.file_type.has_brand(b"tmap") && f.file_type.has_brand(b"vvic"));
    let rep = check(&f, MiafProfile::Miaf).unwrap();
    assert!(rep.is_conformant(), "{:#?}", rep.violations);
    let back = decode_primary(&f, ItemDecoder::direct()).unwrap();
    let bgm = back.gain_map.as_ref().expect("gain map attached");
    assert_eq!(bgm.metadata, gm.metadata);
    assert_eq!(bgm.alternate_colr, gm.alternate_colr);
    assert_eq!(
        (bgm.frame.width, bgm.frame.height),
        (gm.frame.width, gm.frame.height)
    );
    assert_eq!(
        f.meta()
            .unwrap()
            .item(bgm.gain_map_item_id)
            .unwrap()
            .item_type,
        ITEM_TYPE_VVC1
    );
    let mapped = decode_primary(&f, ItemDecoder::direct().tone_mapped()).unwrap();
    assert_eq!(
        (mapped.width(), mapped.height()),
        (back.width(), back.height())
    );
    assert_items_black_box_exact(&f, "tmap");
}

#[test]
fn vvc_framework_encoder_and_mini_match_the_contract_path() {
    use oxideav_core::{CodecOptions, VideoFrame, VideoPlane};
    let mut ctx = RuntimeContext::new();
    oxideav_heif::register(&mut ctx);
    let (w, h) = (72u32, 40u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[(x * 3) as u8, (y * 5) as u8, 90, 255]);
        }
    }
    let mut params = CodecParameters::video(CodecId::new("heif"));
    params.width = Some(w);
    params.height = Some(h);
    params.pixel_format = Some(oxideav_core::PixelFormat::Rgba);
    params.options = CodecOptions::new().set("codec", "vvc");
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    enc.send_frame(&Frame::Video(VideoFrame {
        pts: Some(0),
        planes: vec![VideoPlane {
            stride: w as usize * 4,
            data: rgba.clone(),
        }],
    }))
    .unwrap();
    enc.flush().unwrap();
    let via_codec = enc.receive_packet().unwrap().data;
    assert_eq!(
        via_codec,
        oxideav_heif::encode_rgba8(w, h, &rgba, &vvc()).unwrap()
    );
    let f = HeifFile::parse(&via_codec).unwrap();
    assert_eq!(f.file_type.major_brand, *b"vvic");
    // The framework decoder over that packet == decode().
    let mut dparams = CodecParameters::video(CodecId::new("heif"));
    dparams.width = Some(w);
    dparams.height = Some(h);
    let mut dec = ctx.codecs.first_decoder(&dparams).unwrap();
    dec.send_packet(
        &oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 1), via_codec.clone())
            .with_keyframe(true),
    )
    .unwrap();
    dec.flush().unwrap();
    let Frame::Video(vf) = dec.receive_frame().unwrap() else {
        panic!("video frame");
    };
    let img = oxideav_heif::decode(&via_codec).unwrap();
    let (want, _) = img.clone().into_video_frame();
    for (a, b) in vf.planes.iter().zip(&want.planes) {
        assert_eq!((a.stride, &a.data), (b.stride, &b.data));
    }
    // Low-overhead form: explicit vvc1 / vvcC types, `vvic` minor, the
    // same pixels.
    let src = gentle(64, 64);
    let regular = encode_still(&src, &vvc()).unwrap();
    let mini = encode_still_minimized(&src, &vvc()).unwrap();
    assert!(mini.len() < regular.len());
    let mf = HeifFile::parse(&mini).unwrap();
    // The raw ftyp is mif3 + vvic (O.2.1.1); the parsed file is the
    // O.4 equivalent, whose major brand is vvic.
    assert_eq!(&mini[8..12], b"mif3");
    assert_eq!(&mini[12..16], b"vvic");
    assert_eq!(mf.file_type.major_brand, *b"vvic");
    assert_eq!(mf.primary_item().unwrap().item_type, ITEM_TYPE_VVC1);
    let a = oxideav_heif::decode(&regular).unwrap();
    let b = oxideav_heif::decode(&mini).unwrap();
    assert_eq!(planes_of(&a), planes_of(&b));
    assert!(check(&mf, MiafProfile::Miaf).unwrap().is_conformant());
}

#[test]
fn vvc_image_sequence_round_trips() {
    use oxideav_core::Demuxer;
    let frames: Vec<oxideav_heif::Frame> = (0..3u32)
        .map(|i| {
            let mut f = gentle(64, 64);
            for y in 0..64 {
                for x in 0..64 {
                    let v = f.sample(0, x, y) as u32 + i * 10;
                    f.set_sample(0, x, y, v.min(255) as u16);
                }
            }
            oxideav_heif::Frame::new(
                oxideav_heif::HeifImage::from(f),
                Some(std::time::Duration::from_millis(100)),
                None,
                None,
            )
        })
        .collect();
    let bytes = oxideav_heif::encode_all(&frames, &vvc()).unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert!(
        f.file_type.has_brand(b"vvis"),
        "{:?}",
        f.file_type.all_brands()
    );
    assert!(f.file_type.has_brand(b"msf1"));
    let movie = oxideav_heif::sequence::parse_movie(&f).unwrap().unwrap();
    let track = &movie.tracks[0];
    let entry = track.primary_entry().unwrap();
    assert_eq!(entry.entry_type, *b"vvc1");
    assert!(entry.vvcc.is_some());
    let back = oxideav_heif::decode_all(&bytes).unwrap();
    let timed: Vec<_> = back.iter().filter(|fr| fr.delay.is_some()).collect();
    assert_eq!(timed.len(), 3);
    for (i, fr) in timed.iter().enumerate() {
        assert_eq!((fr.image.width(), fr.image.height()), (64, 64));
        assert_eq!(
            fr.delay,
            Some(std::time::Duration::from_millis(100)),
            "frame {i}"
        );
        let a: HeifFrame = fr.image.clone().into_frame().unwrap();
        let p = psnr_y(&frames[i].image.clone().into_frame().unwrap(), &a.tight());
        assert!(p >= 30.0, "frame {i}: PSNR_Y {p:.2}");
    }
    // The framework demuxer announces the track as an "h266" stream
    // with the vvcC record as extradata.
    let mut ctx = RuntimeContext::new();
    oxideav_heif::register(&mut ctx);
    let d = oxideav_heif::HeifDemuxer::from_bytes(bytes.clone(), &ctx.codecs).unwrap();
    let h266 = d
        .streams()
        .iter()
        .find(|s| s.params.codec_id.as_str() == "h266")
        .expect("h266 track stream");
    assert!(!h266.params.extradata.is_empty());
    assert_eq!(
        h266.params.pixel_format,
        Some(oxideav_core::PixelFormat::Yuv420P)
    );
}
