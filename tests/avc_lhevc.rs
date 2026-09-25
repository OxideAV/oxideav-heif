//! `avc1` items (HEIF Annex E) through oxideav-h264 and `lhv1` items
//! (HEIF Annex B.2.2.1.3) through oxideav-h265's base-layer decode.
//!
//! No producer on this machine writes AVC-in-HEIF, so the `avc1`
//! items are built here from a black-box AVC encoder's Annex B output
//! wrapped by this crate's writer; the decode is checked byte-exact
//! against that encoder's own decoder, and the third-party readers'
//! verdicts on the file are printed (measurements, not assertions —
//! none is documented to read AVC image items). `lhv1` has no producer
//! at all: the item is a single-layer HEVC still re-labelled with an
//! `lhvC` / `oinf` / `tols`, which is a valid base layer.
#![cfg(feature = "registry")]

mod common;

use std::path::Path;
use std::process::Command;

use common::{have_binary, scratch_dir};
use oxideav_heif::decode::{decode_primary, ItemDecoder};
use oxideav_heif::encode::{avc_item_from_annex_b, encode_still, EncodeOptions};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};
use oxideav_heif::lhvc::{
    LayerDependency, LhevcConfig, OperatingPoint, OperatingPointLayer, OperatingPointPtl,
    OperatingPoints,
};
use oxideav_heif::meta::{ITEM_TYPE_AVC1, ITEM_TYPE_LHV1};
use oxideav_heif::miaf::{check, MiafProfile};
use oxideav_heif::props::{Colr, Ispe, Pixi, Property};
use oxideav_heif::{HeifError, HeifFile, HeifWriter};

fn picture(w: u32, h: u32, chroma: Chroma, depth: u8) -> HeifFrame {
    let mut f =
        HeifFrame::zeroed(w, h, HeifPixelFormat::new(chroma, depth, false).unwrap()).unwrap();
    let max = (1u32 << depth) - 1;
    for p in 0..f.format.plane_count() {
        let (pw, ph) = f.plane_dims(p);
        for y in 0..ph {
            for x in 0..pw {
                let v = ((x * 7 + y * 3 + p as u32 * 40) * max / (pw + ph + 40)) % (max + 1);
                f.set_sample(p, x, y, v as u16);
            }
        }
    }
    f
}

fn planes_concat(f: &HeifFrame) -> Vec<u8> {
    let t = f.tight();
    t.planes
        .iter()
        .flat_map(|p| p.data.iter().copied())
        .collect()
}

fn pix_fmt(chroma: Chroma, depth: u8) -> String {
    let base = match chroma {
        Chroma::Mono => "gray",
        Chroma::Yuv420 => "yuv420p",
        Chroma::Yuv422 => "yuv422p",
        Chroma::Yuv444 => "yuv444p",
    };
    if depth == 8 {
        base.to_string()
    } else {
        format!("{base}{depth}le")
    }
}

/// Encode `src` to one AVC access unit with the black-box encoder.
fn x264_annex_b(dir: &Path, tag: &str, src: &HeifFrame, profile: &str) -> Option<Vec<u8>> {
    let raw = dir.join(format!("{tag}.yuv"));
    std::fs::write(&raw, planes_concat(src)).unwrap();
    let out = dir.join(format!("{tag}.h264"));
    let st = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-f", "rawvideo"])
        .args([
            "-pix_fmt",
            &pix_fmt(src.format.chroma, src.format.bit_depth),
        ])
        .args(["-s", &format!("{}x{}", src.width, src.height)])
        .arg("-i")
        .arg(&raw)
        .args(["-frames:v", "1", "-c:v", "libx264", "-profile:v", profile])
        .args([
            "-x264-params",
            "keyint=1:aud=1",
            "-crf",
            "16",
            "-preset",
            "medium",
        ])
        .args(["-f", "h264"])
        .arg(&out)
        .output()
        .ok()?;
    if !st.status.success() {
        eprintln!(
            "SKIP: black-box AVC encoder refused {tag} ({profile}): {}",
            String::from_utf8_lossy(&st.stderr).trim()
        );
        return None;
    }
    std::fs::read(&out).ok()
}

/// The black-box decoder's planar output for an Annex B stream.
fn ffmpeg_raw(dir: &Path, tag: &str, h264: &Path, fmt: &str) -> Vec<u8> {
    let out = dir.join(format!("{tag}.raw"));
    let st = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(h264)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", fmt])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "{}",
        String::from_utf8_lossy(&st.stderr)
    );
    std::fs::read(&out).unwrap()
}

fn reader_verdict(bin: &str, args: &[&str], file: &Path) -> String {
    if !have_binary(bin) {
        return "absent".into();
    }
    match Command::new(bin).args(args).arg(file).output() {
        Ok(o) if o.status.success() => "opens".into(),
        Ok(_) => "refuses".into(),
        Err(_) => "absent".into(),
    }
}

#[test]
fn avc1_items_decode_byte_exact_against_the_black_box_decoder() {
    if !have_binary("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed (AVC producer)");
        return;
    }
    let dir = scratch_dir("avc");
    let cases = [
        ("baseline_96x80", 96, 80, Chroma::Yuv420, 8, "baseline"),
        ("main_96x80", 96, 80, Chroma::Yuv420, 8, "main"),
        // 4:2:0 needs even extents at the encoder; 62×50 is not
        // macroblock-aligned, so the SPS crop is exercised.
        ("high_62x50", 62, 50, Chroma::Yuv420, 8, "high"),
        ("high422_96x80", 96, 80, Chroma::Yuv422, 8, "high422"),
        ("high444_63x61", 63, 61, Chroma::Yuv444, 8, "high444"),
        ("high10_96x80", 96, 80, Chroma::Yuv420, 10, "high10"),
    ];
    let mut done = 0;
    for (tag, w, h, chroma, depth, profile) in cases {
        let src = picture(w, h, chroma, depth);
        let Some(annex_b) = x264_annex_b(&dir, tag, &src, profile) else {
            continue;
        };
        let (cfg, data, iw, ih, layout) = avc_item_from_annex_b(&annex_b).unwrap();
        assert_eq!((iw, ih), (w, h), "{tag}: SPS size");
        assert_eq!((layout.chroma, layout.bit_depth), (chroma, depth), "{tag}");
        assert_eq!(cfg.layout().unwrap(), layout, "{tag}: avcC trailer layout");
        let mut wr = HeifWriter::new();
        let id = wr.add_coded_item(
            ITEM_TYPE_AVC1,
            data,
            vec![
                (Property::AvcC(cfg.clone()), true),
                (
                    Property::Ispe(Ispe {
                        width: w,
                        height: h,
                    }),
                    false,
                ),
                (
                    Property::Pixi(Pixi {
                        bits_per_channel: vec![depth; chroma.colour_planes()],
                    }),
                    false,
                ),
                (Property::Colr(Colr::MIAF_DEFAULT), false),
            ],
        );
        wr.set_primary(id);
        let bytes = wr.write_to_vec().unwrap();
        let path = dir.join(format!("{tag}.heif"));
        std::fs::write(&path, &bytes).unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{tag}: {:#?}", rep.violations);
        assert_eq!(
            f.meta().unwrap().item(id).unwrap().item_type,
            ITEM_TYPE_AVC1
        );
        let props = oxideav_heif::props::ItemProperties::resolve(f.meta().unwrap(), id).unwrap();
        assert_eq!(props.avcc().map(|c| c.profile_idc), Some(cfg.profile_idc));
        // Ours vs the black-box decoder of the same stream: byte-exact.
        let img = decode_primary(&f, ItemDecoder::direct())
            .unwrap_or_else(|e| panic!("{tag}: decode: {e}"));
        assert_eq!((img.width(), img.height()), (w, h), "{tag}");
        assert_eq!(img.frame.format, layout, "{tag}");
        let theirs = ffmpeg_raw(
            &dir,
            tag,
            &dir.join(format!("{tag}.h264")),
            &pix_fmt(chroma, depth),
        );
        let ours = planes_concat(&img.frame);
        assert_eq!(ours.len(), theirs.len(), "{tag}: raw size");
        assert!(
            ours == theirs,
            "{tag}: planes differ from the black-box decoder"
        );
        // Framework path: demuxer + "heif" codec route avc1 too.
        let mut ctx = oxideav_core::RuntimeContext::new();
        oxideav_h264::register(&mut ctx);
        oxideav_heif::register(&mut ctx);
        let mut d = ctx
            .containers
            .open_demuxer(
                "heif",
                Box::new(std::io::Cursor::new(bytes.clone())),
                &ctx.codecs,
            )
            .unwrap();
        let pkt = d.next_packet().unwrap();
        let mut dec = ctx.codecs.first_decoder(&d.streams()[0].params).unwrap();
        dec.send_packet(&pkt).unwrap();
        dec.flush().unwrap();
        assert!(
            matches!(dec.receive_frame(), Ok(oxideav_core::Frame::Video(_))),
            "{tag}"
        );
        // Third-party readers: a measurement.
        eprintln!(
            "avc1 HEIF {tag}: heif-info {}, ffmpeg {}, sips {}, magick {}",
            reader_verdict("heif-info", &[], &path),
            reader_verdict(
                "ffmpeg",
                &["-nostdin", "-loglevel", "error", "-f", "null", "-", "-i"],
                &path
            ),
            reader_verdict("sips", &["-g", "pixelWidth"], &path),
            reader_verdict("magick", &["identify"], &path),
        );
        done += 1;
    }
    assert!(done >= 1, "no AVC profile could be produced");
}

/// An `lhv1` item: the base layer decodes through the HEVC decoder,
/// an output layer set with enhancement layers is a typed refusal
/// unless the caller settles for the base.
#[test]
fn lhv1_base_layer_decodes_and_enhancement_layers_refuse_typed() {
    let src = picture(64, 48, Chroma::Yuv420, 8);
    let opts = EncodeOptions {
        hevc_mode: "pcm".into(),
        ..EncodeOptions::default()
    };
    let hevc = HeifFile::from_vec(encode_still(&src, &opts).unwrap()).unwrap();
    let base = decode_primary(&hevc, ItemDecoder::direct()).unwrap();
    let meta = hevc.meta().unwrap();
    let pid = hevc.primary_item().unwrap().id;
    let props = oxideav_heif::props::ItemProperties::resolve(meta, pid).unwrap();
    let hvcc = props.hvcc().unwrap().clone();
    let au = hevc.item_data_owned(pid).unwrap();
    // A second "layer": one VCL NAL unit re-labelled nuh_layer_id 1,
    // appended to the access unit (the filter must drop it).
    let nals = oxideav_heif::hvcc::split_length_prefixed(&au, hvcc.length_size).unwrap();
    let mut layered: Vec<Vec<u8>> = nals.iter().map(|n| n.to_vec()).collect();
    let mut enh = nals[0].to_vec();
    enh[1] |= 1 << 3; // nuh_layer_id = 1 (low bit of byte 0 is the msb; byte 1 bits 7..3 are the rest)
    layered.push(enh);
    let refs: Vec<&[u8]> = layered.iter().map(Vec::as_slice).collect();
    let au_layered = oxideav_heif::hvcc::join_length_prefixed(&refs, 4).unwrap();
    let lhvc = LhevcConfig {
        configuration_version: 1,
        min_spatial_segmentation_idc: 0,
        parallelism_type: 0,
        num_temporal_layers: 1,
        temporal_id_nested: true,
        length_size: 4,
        arrays: hvcc.arrays.clone(),
        raw: Vec::new(),
    };
    let layer = |id: u8, out: bool| OperatingPointLayer {
        ptl_idx: 1,
        layer_id: id,
        is_output_layer: out,
        is_alternate_output_layer: false,
    };
    let op = |ols: u16, layers: Vec<OperatingPointLayer>| OperatingPoint {
        output_layer_set_idx: ols,
        max_temporal_id: 0,
        layers,
        min_pic_size: (64, 48),
        max_pic_size: (64, 48),
        max_chroma_format: 1,
        max_bit_depth_minus8: 0,
        frame_rate: None,
        bit_rate: None,
    };
    let oinf = OperatingPoints {
        scalability_mask: 0b100,
        ptls: vec![OperatingPointPtl {
            profile_space: 0,
            tier_flag: false,
            profile_idc: hvcc.general_profile_idc,
            profile_compatibility_flags: hvcc.general_profile_compatibility_flags,
            constraint_indicator_flags: hvcc.general_constraint_indicator_flags,
            level_idc: hvcc.general_level_idc,
        }],
        operating_points: vec![
            op(0, vec![layer(0, true)]),
            op(1, vec![layer(0, false), layer(1, true)]),
        ],
        layers: vec![
            LayerDependency {
                layer_id: 0,
                direct_ref_layer_ids: vec![],
                dimension_identifiers: vec![(2, 0)],
            },
            LayerDependency {
                layer_id: 1,
                direct_ref_layer_ids: vec![0],
                dimension_identifiers: vec![(2, 1)],
            },
        ],
        raw: Vec::new(),
    };
    let build = |tols: u16| {
        let mut w = HeifWriter::new();
        let id = w.add_coded_item(
            ITEM_TYPE_LHV1,
            au_layered.clone(),
            vec![
                (Property::LhvC(lhvc.clone()), true),
                (Property::Oinf(oinf.clone()), false),
                (Property::Tols(tols), true),
                (
                    Property::Ispe(Ispe {
                        width: 64,
                        height: 48,
                    }),
                    false,
                ),
                (
                    Property::Pixi(Pixi {
                        bits_per_channel: vec![8, 8, 8],
                    }),
                    false,
                ),
                (Property::Colr(Colr::MIAF_DEFAULT), false),
            ],
        );
        w.set_primary(id);
        (id, HeifFile::from_vec(w.write_to_vec().unwrap()).unwrap())
    };
    // tols 0: the base layer, pixel-exact with the hvc1 decode.
    let (id, f) = build(0);
    let m = f.meta().unwrap();
    let p = oxideav_heif::props::ItemProperties::resolve(m, id).unwrap();
    assert_eq!(p.lhvc().map(|c| c.arrays.len()), Some(hvcc.arrays.len()));
    assert_eq!(p.oinf().map(|o| o.output_layers(1)), Some(vec![1]));
    assert_eq!(p.tols(), Some(0));
    let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
    assert_eq!(img.frame, base.frame);
    // tols 1: enhancement layer requested → typed refusal; fallback
    // yields the base.
    let (id1, f1) = build(1);
    match decode_primary(&f1, ItemDecoder::direct()) {
        Err(e) => {
            assert!(matches!(e, HeifError::Unsupported(_)), "{e}");
            assert_eq!(e.layered_hevc_info(), Some((id1, 1)), "{e}");
        }
        Ok(_) => panic!("expected the L-HEVC refusal"),
    }
    let img = decode_primary(&f1, ItemDecoder::direct().base_layer_fallback()).unwrap();
    assert_eq!(img.frame, base.frame);
    // The demuxer predicts the base layout for the still stream.
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let d = ctx
        .containers
        .open_demuxer(
            "heif",
            Box::new(std::io::Cursor::new(f.into_bytes())),
            &ctx.codecs,
        )
        .unwrap();
    assert_eq!(
        d.streams()[0].params.pixel_format,
        Some(oxideav_core::PixelFormat::YuvJ420P)
    );
}
