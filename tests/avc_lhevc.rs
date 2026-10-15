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
                (Property::Ispe(Ispe::new(w, h)), false),
                (
                    Property::Pixi(Pixi::new(vec![depth; chroma.colour_planes()])),
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
    let opts = EncodeOptions::default().with_hevc_mode("pcm".into());
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
    let lhvc = LhevcConfig::new(1, 0, 0, 1, true, 4, hvcc.arrays.clone(), Vec::new());
    let layer = |id: u8, out: bool| OperatingPointLayer::new(1, id, out, false);
    let op = |ols: u16, layers: Vec<OperatingPointLayer>| {
        OperatingPoint::new(ols, 0, layers, (64, 48), (64, 48), 1, 0, None, None)
    };
    let oinf = OperatingPoints::new(
        0b100,
        vec![OperatingPointPtl::new(
            0,
            false,
            hvcc.general_profile_idc,
            hvcc.general_profile_compatibility_flags,
            hvcc.general_constraint_indicator_flags,
            hvcc.general_level_idc,
        )],
        vec![
            op(0, vec![layer(0, true)]),
            op(1, vec![layer(0, false), layer(1, true)]),
        ],
        vec![
            LayerDependency::new(0, vec![], vec![(2, 0)]),
            LayerDependency::new(1, vec![0], vec![(2, 1)]),
        ],
        Vec::new(),
    );
    let build = |tols: u16| {
        let mut w = HeifWriter::new();
        let id = w.add_coded_item(
            ITEM_TYPE_LHV1,
            au_layered.clone(),
            vec![
                (Property::LhvC(lhvc.clone()), true),
                (Property::Oinf(oinf.clone()), false),
                (Property::Tols(tols), true),
                (Property::Ispe(Ispe::new(64, 48)), false),
                (Property::Pixi(Pixi::new(vec![8, 8, 8])), false),
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

/// Annex B stream of an `avc1` item: the `avcC` SPS / PPS arrays
/// followed by the item's length-prefixed NAL units, start-coded.
fn avc_item_annex_b(f: &HeifFile, id: u32) -> Vec<u8> {
    let props = oxideav_heif::props::ItemProperties::resolve(f.meta().unwrap(), id).unwrap();
    let cfg = props.avcc().unwrap();
    let data = f.item_data_owned(id).unwrap();
    let mut out = Vec::new();
    for nal in cfg
        .sps
        .iter()
        .chain(cfg.pps.iter())
        .map(|v| v.as_slice())
        .chain(oxideav_heif::hvcc::split_length_prefixed(&data, cfg.length_size).unwrap())
    {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

/// PSNR (dB) of `a` against `b` over 8-bit samples.
fn psnr8(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| {
            let d = *x as f64 - *y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// `StillCodec::Avc`: this crate's own AVC items (HEIF Annex E) —
/// CAVLC Baseline and CABAC Main at a QP, macroblock padding + `clap`,
/// alpha, grid tiles, a thumbnail, the `avci` brand — decode in this
/// crate and byte-exact in the black-box AVC decoder on every item's
/// stream, at a PSNR the QP predicts; the `mif3` form decodes to the
/// same picture; `encode_all` writes an `avcs` sequence the demuxer
/// plays back; the `"heif"` muxer takes an `"h264"` Annex B stream.
#[test]
fn avc1_items_written_by_this_crate_decode_in_both_decoders() {
    use oxideav_heif::encode::{encode_still_minimized, StillCodec};
    let dir = scratch_dir("avc_write");
    let ffmpeg = have_binary("ffmpeg");
    struct Case {
        tag: &'static str,
        w: u32,
        h: u32,
        alpha: bool,
        cabac: bool,
        qp: u8,
        grid: Option<u32>,
        thumb: Option<u32>,
        min_psnr: f64,
    }
    let cases = [
        Case {
            tag: "cavlc_96x80",
            w: 96,
            h: 80,
            alpha: false,
            cabac: false,
            qp: 20,
            grid: None,
            thumb: None,
            min_psnr: 38.0,
        },
        Case {
            tag: "cabac_62x50_crop",
            w: 62,
            h: 50,
            alpha: false,
            cabac: true,
            qp: 16,
            grid: None,
            thumb: None,
            min_psnr: 40.0,
        },
        Case {
            tag: "alpha_thumb_120x90",
            w: 120,
            h: 90,
            alpha: true,
            cabac: true,
            qp: 22,
            grid: None,
            thumb: Some(32),
            min_psnr: 36.0,
        },
        Case {
            tag: "grid_300x200",
            w: 300,
            h: 200,
            alpha: false,
            cabac: false,
            qp: 24,
            grid: Some(128),
            thumb: None,
            min_psnr: 34.0,
        },
    ];
    for c in &cases {
        let mut src = picture(c.w, c.h, Chroma::Yuv420, 8);
        if c.alpha {
            let mut a = HeifFrame::zeroed(
                c.w,
                c.h,
                HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap(),
            )
            .unwrap();
            for y in 0..c.h {
                for x in 0..c.w {
                    a.set_sample(0, x, y, ((x * 2 + y) % 256) as u16);
                }
            }
            src = src.with_alpha_plane(&a).unwrap();
        }
        let mut opts = EncodeOptions::default()
            .with_codec(StillCodec::Avc)
            .with_qp(c.qp)
            .with_grid_tile(c.grid)
            .with_thumbnail_max_dim(c.thumb);
        if c.cabac {
            opts = opts.with_avc_options(vec![("cabac".into(), "1".into())]);
        }
        let bytes = encode_still(&src, &opts).unwrap_or_else(|e| panic!("{}: {e}", c.tag));
        let path = dir.join(format!("{}.heif", c.tag));
        std::fs::write(&path, &bytes).unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        assert!(
            f.file_type.has_brand(&oxideav_heif::ftyp::BRAND_AVCI),
            "{}: avci brand",
            c.tag
        );
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{}: {:#?}", c.tag, rep.violations);
        let img = decode_primary(&f, ItemDecoder::direct())
            .unwrap_or_else(|e| panic!("{}: decode: {e}", c.tag));
        assert_eq!((img.width(), img.height()), (c.w, c.h), "{}", c.tag);
        assert_eq!(img.frame.format.has_alpha, c.alpha, "{}", c.tag);
        assert_eq!(
            (img.frame.format.chroma, img.frame.format.bit_depth),
            (Chroma::Yuv420, 8)
        );
        // Fidelity: luma PSNR against the source at this QP; alpha exact
        // where the QP is low enough to be transparent on a ramp.
        let p = psnr8(
            &img.frame.tight().planes[0].data,
            &src.tight().planes[0].data,
        );
        assert!(p >= c.min_psnr, "{}: luma PSNR {p:.2} dB", c.tag);
        eprintln!(
            "avc write {}: {} bytes, luma PSNR {p:.2} dB",
            c.tag,
            bytes.len()
        );
        // Every avc1 item byte-exact against the black-box decoder.
        let meta = f.meta().unwrap();
        let mut items = 0;
        for it in &meta.items {
            if it.item_type != ITEM_TYPE_AVC1 {
                continue;
            }
            items += 1;
            let ours = oxideav_heif::decode::decode_item(&f, it.id, ItemDecoder::direct())
                .unwrap_or_else(|e| panic!("{}: item {}: {e}", c.tag, it.id));
            let cfg = oxideav_heif::props::ItemProperties::resolve(meta, it.id)
                .unwrap()
                .avcc()
                .cloned()
                .unwrap();
            assert_eq!(cfg.profile_idc, if c.cabac { 77 } else { 66 }, "{}", c.tag);
            if !ffmpeg {
                continue;
            }
            let annex_b = avc_item_annex_b(&f, it.id);
            let h264 = dir.join(format!("{}_{}.h264", c.tag, it.id));
            std::fs::write(&h264, &annex_b).unwrap();
            // The raw decoder emits the coded (macroblock-aligned) picture;
            // this crate applies the item's `clap`. Crop theirs to ours.
            let coded_w = ours.frame.width.div_ceil(16) * 16;
            let coded_h = ours.frame.height.div_ceil(16) * 16;
            let theirs = ffmpeg_raw(&dir, &format!("{}_{}", c.tag, it.id), &h264, "yuv420p");
            let (w, h) = (ours.frame.width as usize, ours.frame.height as usize);
            let (cw, ch) = (coded_w as usize, coded_h as usize);
            assert_eq!(
                theirs.len(),
                cw * ch * 3 / 2,
                "{}: item {} raw size",
                c.tag,
                it.id
            );
            let mut cropped = Vec::with_capacity(w * h * 3 / 2);
            for y in 0..h {
                cropped.extend_from_slice(&theirs[y * cw..y * cw + w]);
            }
            let (hw, hh, hcw, hch) = (w.div_ceil(2), h.div_ceil(2), cw / 2, ch / 2);
            for plane in 0..2 {
                let base = cw * ch + plane * hcw * hch;
                for y in 0..hh {
                    cropped.extend_from_slice(&theirs[base + y * hcw..base + y * hcw + hw]);
                }
            }
            // The master item's decode carries its alpha auxiliary; the
            // stream itself is the colour picture.
            let mine = planes_concat(&ours.frame.without_alpha());
            assert_eq!(
                mine.len(),
                cropped.len(),
                "{}: item {} planes",
                c.tag,
                it.id
            );
            assert!(
                mine == cropped,
                "{}: item {} differs from the black-box decoder",
                c.tag,
                it.id
            );
        }
        let expected_items = 1
            + usize::from(c.alpha)
            + usize::from(c.thumb.is_some())
            + c.grid
                .map_or(0, |t| (c.w.div_ceil(t) * c.h.div_ceil(t)) as usize - 1);
        assert_eq!(items, expected_items, "{}: avc1 item count", c.tag);
        // Low-overhead form (no clap in a MinimizedImageBox, so
        // macroblock-aligned pictures only): the same picture.
        if c.grid.is_none() && c.thumb.is_none() && c.w % 16 == 0 && c.h % 16 == 0 {
            let mini = encode_still_minimized(&src, &opts).unwrap();
            let mf = HeifFile::parse(&mini).unwrap();
            let mimg = decode_primary(&mf, ItemDecoder::direct()).unwrap();
            assert_eq!(mimg.frame, img.frame, "{}: mif3 form", c.tag);
        }
        // Third-party readers: a measurement.
        eprintln!(
            "avc1 HEIF written {}: heif-info {}, ffmpeg {}, sips {}, magick {}",
            c.tag,
            reader_verdict("heif-info", &[], &path),
            reader_verdict(
                "ffmpeg",
                &["-nostdin", "-loglevel", "error", "-f", "null", "-", "-i"],
                &path
            ),
            reader_verdict("sips", &["-g", "pixelWidth"], &path),
            reader_verdict("magick", &["identify"], &path),
        );
    }
    // Registry "heif" encoder: codec=avc.
    {
        let mut ctx = oxideav_core::RuntimeContext::new();
        oxideav_h264::register(&mut ctx);
        oxideav_heif::register(&mut ctx);
        let src = picture(64, 48, Chroma::Yuv420, 8);
        let (vf, pf) = src.to_core().unwrap();
        let mut params = oxideav_core::CodecParameters::video(oxideav_core::CodecId::new("heif"));
        params.width = Some(64);
        params.height = Some(48);
        params.pixel_format = Some(pf);
        params.options = oxideav_core::CodecOptions::new()
            .set("codec", "avc")
            .set("qp", "18");
        let mut enc = ctx.codecs.first_encoder(&params).unwrap();
        enc.send_frame(&oxideav_core::Frame::Video(vf)).unwrap();
        enc.flush().unwrap();
        let pkt = enc.receive_packet().unwrap();
        let f = HeifFile::parse(&pkt.data).unwrap();
        assert!(f.file_type.has_brand(&oxideav_heif::ftyp::BRAND_AVCI));
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        assert!(
            psnr8(
                &img.frame.tight().planes[0].data,
                &src.tight().planes[0].data
            ) > 40.0
        );
    }
    // encode_all: an avcs image sequence of three frames — as one
    // IDR + P session (the default) and as three IDR access units
    // (`gop=1`): H.264 §7.4.3 asks consecutive IDR pictures for distinct
    // idr_pic_id values, which the registry encoder rotates itself
    // (oxideav-h264 0.1.9), so every sample stays its own picture.
    for gop1 in [false, true] {
        let mut opts = EncodeOptions::default()
            .with_codec(StillCodec::Avc)
            .with_qp(20);
        if gop1 {
            opts = opts.with_avc_options(vec![("gop".into(), "1".into())]);
        }
        let frames: Vec<oxideav_heif::Frame> = (0..3u32)
            .map(|i| {
                let mut p = picture(64, 48, Chroma::Yuv420, 8);
                for y in 0..48 {
                    for x in 0..64 {
                        p.set_sample(0, x, y, (p.sample(0, x, y) + i as u16 * 20) % 256);
                    }
                }
                oxideav_heif::Frame::new(
                    oxideav_heif::HeifImage::from_frame(
                        p,
                        oxideav_heif::ColorInfo::default(),
                        oxideav_heif::Metadata::default(),
                    ),
                    Some(std::time::Duration::from_millis(100)),
                    None,
                    None,
                )
            })
            .collect();
        let bytes = oxideav_heif::encode_all(&frames, &opts).unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        assert!(
            f.file_type.has_brand(&oxideav_heif::ftyp::BRAND_AVCS),
            "avcs brand"
        );
        let out =
            oxideav_heif::decode_all_with(&bytes, &oxideav_heif::DecodeOptions::default()).unwrap();
        // The cover item plus the three samples, each a distinct picture.
        assert_eq!(
            out.iter().filter(|f| f.track_id.is_some()).count(),
            3,
            "gop1 {gop1}: three track frames"
        );
        for fr in &out {
            assert_eq!((fr.image.width, fr.image.height), (64, 48));
        }
        // Every sample a sync sample under gop=1, only the first otherwise.
        let movie = oxideav_heif::sequence::parse_movie(&f).unwrap().unwrap();
        let track = movie.tracks.iter().find(|t| t.is_visual()).unwrap();
        let syncs = track.samples.iter().filter(|s| s.is_sync).count();
        assert_eq!(syncs, if gop1 { 3 } else { 1 }, "gop1 {gop1}: sync samples");
        let seq = dir.join(format!("sequence-gop1-{gop1}.heifs"));
        std::fs::write(&seq, &bytes).unwrap();
        // The track is an ordinary ISO-BMFF `avc1` track: the black-box
        // demuxer + decoder play all three samples, byte-exact with ours.
        if ffmpeg {
            let raw = dir.join("sequence.yuv");
            let st = Command::new("ffmpeg")
                .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
                .arg(&seq)
                .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
                .arg(&raw)
                .status()
                .unwrap();
            assert!(st.success(), "ffmpeg refused the avcs sequence");
            let theirs = std::fs::read(&raw).unwrap();
            let mut ours = Vec::new();
            for fr in out.iter().filter(|f| f.track_id.is_some()) {
                let hf = fr.image.to_frame().unwrap();
                ours.extend(planes_concat(&hf));
            }
            assert_eq!(
                theirs.len(),
                ours.len(),
                "avcs sequence: raw size (three 64x48 frames)"
            );
            assert!(
                ours == theirs,
                "avcs sequence: samples differ from the black-box decoder"
            );
        }
    }
    // The "heif" muxer over an "h264" Annex B stream from the registry
    // AVC encoder: an IDR + P session, then an all-IDR one (`gop=1`).
    for gop in ["30", "1"] {
        let mut ctx = oxideav_core::RuntimeContext::new();
        oxideav_h264::register(&mut ctx);
        oxideav_heif::register(&mut ctx);
        let mut params = oxideav_core::CodecParameters::video(oxideav_core::CodecId::new("h264"));
        params.width = Some(64);
        params.height = Some(48);
        let stream = oxideav_core::StreamInfo {
            index: 0,
            time_base: oxideav_core::TimeBase::new(1, 10),
            duration: Some(2),
            start_time: Some(0),
            params,
        };
        let path = dir.join(format!("muxed-gop{gop}.heifs"));
        let sink: Box<dyn oxideav_core::WriteSeek> =
            Box::new(std::fs::File::create(&path).unwrap());
        let mut mx = ctx
            .containers
            .open_muxer("heif", sink, std::slice::from_ref(&stream))
            .unwrap();
        mx.write_header().unwrap();
        // A registry AVC encoder session (IDR + P) feeds Annex B packets.
        let mut eparams = oxideav_core::CodecParameters::video(oxideav_core::CodecId::new("h264"));
        eparams.width = Some(64);
        eparams.height = Some(48);
        eparams.pixel_format = Some(oxideav_core::PixelFormat::Yuv420P);
        eparams.options = oxideav_core::CodecOptions::new()
            .set("rc", "cqp")
            .set("qp", "20")
            .set("gop", gop);
        let mut enc = ctx.codecs.first_encoder(&eparams).unwrap();
        let mut n = 0;
        let mut sources = Vec::new();
        for i in 0..3i64 {
            let mut p = picture(64, 48, Chroma::Yuv420, 8);
            for y in 0..48 {
                for x in 0..64 {
                    p.set_sample(0, x, y, (p.sample(0, x, y) + i as u16 * 20) % 256);
                }
            }
            sources.push(p.clone());
            let (mut vf, _) = p.to_core().unwrap();
            vf.pts = Some(i);
            enc.send_frame(&oxideav_core::Frame::Video(vf)).unwrap();
            while let Ok(p) = enc.receive_packet() {
                let pkt = p.with_stream_index(0).with_duration(1);
                mx.write_packet(&pkt).unwrap();
                n += 1;
            }
        }
        enc.flush().unwrap();
        while let Ok(p) = enc.receive_packet() {
            mx.write_packet(&p.with_stream_index(0).with_duration(1))
                .unwrap();
            n += 1;
        }
        assert_eq!(n, 3, "three access units muxed");
        mx.write_trailer().unwrap();
        drop(mx);
        let bytes = std::fs::read(&path).unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        assert!(
            f.file_type.has_brand(&oxideav_heif::ftyp::BRAND_AVCS),
            "muxer: avcs brand"
        );
        let out =
            oxideav_heif::decode_all_with(&bytes, &oxideav_heif::DecodeOptions::default()).unwrap();
        let samples: Vec<&oxideav_heif::Frame> =
            out.iter().filter(|f| f.track_id.is_some()).collect();
        assert_eq!(samples.len(), 3, "muxer gop {gop}: three samples");
        let movie = oxideav_heif::sequence::parse_movie(&f).unwrap().unwrap();
        let track = movie.tracks.iter().find(|t| t.is_visual()).unwrap();
        let syncs = track.samples.iter().filter(|s| s.is_sync).count();
        assert_eq!(
            syncs,
            if gop == "1" { 3 } else { 1 },
            "muxer gop {gop}: syncs"
        );
        // Each sample is its own picture: it tracks its own source
        // (the three sources differ by 20 luma codes).
        for (i, fr) in samples.iter().enumerate() {
            let hf = fr.image.to_frame().unwrap();
            let p = psnr8(
                &hf.tight().planes[0].data,
                &sources[i].tight().planes[0].data,
            );
            assert!(p > 35.0, "muxer gop {gop}: sample {i} luma PSNR {p:.2} dB");
        }
        if ffmpeg {
            let raw = dir.join(format!("muxed-gop{gop}.yuv"));
            let st = Command::new("ffmpeg")
                .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
                .arg(&path)
                .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
                .arg(&raw)
                .status()
                .unwrap();
            assert!(
                st.success(),
                "muxer gop {gop}: the black-box decoder refused the file"
            );
            let theirs = std::fs::read(&raw).unwrap();
            let mut ours = Vec::new();
            for fr in &samples {
                ours.extend(planes_concat(&fr.image.to_frame().unwrap()));
            }
            assert_eq!(
                theirs.len(),
                ours.len(),
                "muxer gop {gop}: three pictures out"
            );
            assert!(
                ours == theirs,
                "muxer gop {gop}: samples differ from the black-box decoder"
            );
        }
    }
}
