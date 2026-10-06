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
