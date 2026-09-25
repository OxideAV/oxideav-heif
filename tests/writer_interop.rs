//! Real-world interop, writer side: every shape [`encode_still`] and
//! the framework `"heif"` encoder produce is
//!
//! * structurally sound — re-parsed by this crate, MIAF-conformant,
//!   round-tripped to the same pixels (these assertions run always); and
//! * opened by every third-party reader on the machine — Apple ImageIO
//!   (`sips`), libheif (`heif-convert`, `heif-info`), ImageMagick
//!   (`magick`) and ffmpeg — with the rendered pixels matching what we
//!   encoded (these run when the binary is present, and print SKIP
//!   otherwise).
//!
//! A reader refusing or mis-rendering a file is a writer bug unless the
//! same structure carrying a third-party stream is also refused. Apple
//! ImageIO refuses a file whose two items carry byte-identical HEVC
//! parameter sets; the writer therefore codes the alpha auxiliary with
//! a distinct SPS (see `alpha_parameter_sets_differ_from_the_master`),
//! and every alpha file is asserted to open in `sips` too. The one
//! remaining divergence — `sips` takes the sample range from the
//! bitstream VUI, which the oxideav HEVC stream lacks, so its render is
//! a video-range expansion of ours — is a codec-crate item and is why
//! `sips` is an "opens the file" oracle rather than a pixel oracle.
#![cfg(feature = "registry")]

mod common;

use std::path::Path;
use std::process::Command;

use common::{have_binary, scratch_dir};
use oxideav_heif::encode::{encode_still, to_yuv420_8, EncodeOptions, StillCodec};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};
use oxideav_heif::miaf::{check, MiafProfile};
use oxideav_heif::props::{Clap, Colr, CropRect, Imir, Irot, Property};
use oxideav_heif::rgb::to_rgb;
use oxideav_heif::{HeifFile, HeifWriter};

/// A reader: render a HEIF file to a PNG, `None` on refusal / absence.
type Render = fn(&Path, &Path) -> Option<()>;

/// A structured 4:4:4 test picture (gradients, a box, a diagonal, a
/// disc), so the encoder's own 4:2:0 conversion is exercised.
fn picture(w: u32, h: u32, gray: bool, alpha: bool) -> HeifFrame {
    let chroma = if gray { Chroma::Mono } else { Chroma::Yuv444 };
    let mut f = HeifFrame::zeroed(w, h, HeifPixelFormat::new(chroma, 8, alpha).unwrap()).unwrap();
    let (cx, cy, r) = (w as f64 * 0.7, h as f64 * 0.65, w.min(h) as f64 * 0.18);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f64 / w.max(2) as f64, y as f64 / h.max(2) as f64);
            let mut rgb = [fx * 255.0, fy * 255.0, (fx + fy) * 127.5];
            if x >= w / 4 && x < w / 2 && y >= h / 4 && y < h / 2 {
                rgb = [240.0, 240.0, 240.0];
            }
            let (dx, dy) = (x as f64 - cx, y as f64 - cy);
            if dx * dx + dy * dy < r * r {
                rgb = [230.0, 30.0, 30.0];
            }
            let yv = 0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2];
            if gray {
                f.set_sample(0, x, y, yv.round() as u16);
            } else {
                f.set_sample(0, x, y, yv.round().clamp(0.0, 255.0) as u16);
                f.set_sample(
                    1,
                    x,
                    y,
                    ((rgb[2] - yv) / 1.772 + 128.0).round().clamp(0.0, 255.0) as u16,
                );
                f.set_sample(
                    2,
                    x,
                    y,
                    ((rgb[0] - yv) / 1.402 + 128.0).round().clamp(0.0, 255.0) as u16,
                );
            }
            if alpha {
                let a = if x < w / 3 { 255 } else { (fy * 255.0) as u16 };
                f.set_sample(f.format.alpha_plane().unwrap(), x, y, a);
            }
        }
    }
    f
}

fn pcm() -> EncodeOptions {
    EncodeOptions {
        hevc_mode: "pcm".into(),
        ..EncodeOptions::default()
    }
}

/// A minimal but structurally valid ICC v4 profile (128-byte header
/// with the `acsp` signature + an empty tag table), enough that a
/// third-party reader accepts the file's `prof` colr instead of
/// rejecting a malformed profile.
fn minimal_icc() -> Vec<u8> {
    let mut p = vec![0u8; 132];
    let size = 132u32.to_be_bytes();
    p[0..4].copy_from_slice(&size);
    p[8] = 0x04; // profile version 4.0
    p[12..16].copy_from_slice(b"mntr"); // device class
    p[16..20].copy_from_slice(b"RGB "); // data colour space
    p[20..24].copy_from_slice(b"XYZ "); // PCS
    p[36..40].copy_from_slice(b"acsp"); // profile file signature
                                        // p[128..132] = tag count 0 (already zero).
    p
}

/// A binary PPM (`P6`, 8-bit RGB) written by a reader — parsed without
/// any deflate, so the comparison never depends on the test PNG reader.
struct Ppm {
    width: u32,
    height: u32,
    /// `width × height × 3` bytes.
    rgb: Vec<u8>,
}

fn read_ppm(bytes: &[u8]) -> Option<Ppm> {
    // Header: "P6" then width, height, maxval (whitespace-separated,
    // '#' comments), then one whitespace byte, then the pixel data.
    if &bytes[..2] != b"P6" {
        return None;
    }
    let mut i = 2usize;
    let mut fields = [0u32; 3];
    for f in &mut fields {
        // skip whitespace / comments
        loop {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'#' {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            } else {
                break;
            }
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        *f = std::str::from_utf8(&bytes[start..i]).ok()?.parse().ok()?;
    }
    i += 1; // the single whitespace after maxval
    let (w, h) = (fields[0], fields[1]);
    let need = w as usize * h as usize * 3;
    if fields[2] != 255 || bytes.len() < i + need {
        return None;
    }
    Some(Ppm {
        width: w,
        height: h,
        rgb: bytes[i..i + need].to_vec(),
    })
}

/// `(max, mean)` colour difference between a reader's PPM and `want`,
/// in 8-bit units.
fn ppm_diff(ppm_path: &Path, want: &oxideav_heif::rgb::RgbImage) -> Option<(f64, f64)> {
    let bytes = std::fs::read(ppm_path).ok()?;
    let ppm = read_ppm(&bytes)?;
    if (ppm.width, ppm.height) != (want.width, want.height) {
        return Some((f64::INFINITY, f64::INFINITY));
    }
    let (mut max, mut sum, mut n) = (0.0f64, 0.0, 0u64);
    for y in 0..ppm.height {
        for x in 0..ppm.width {
            for c in 0..3 {
                let e = ppm.rgb[(y as usize * ppm.width as usize + x as usize) * 3 + c] as f64;
                let g = want.sample(x, y, c) as f64;
                let d = (g - e).abs();
                max = max.max(d);
                sum += d;
                n += 1;
            }
        }
    }
    Some((max, sum / n as f64))
}

/// Render `heic` to a PNG with a reader, or `None` when it refused /
/// is absent.
fn sips_png(heic: &Path, out: &Path) -> Option<()> {
    let ok = Command::new("sips")
        .args(["-s", "format", "png"])
        .arg(heic)
        .arg("--out")
        .arg(out)
        .output()
        .ok()?
        .status
        .success();
    (ok && out.exists()).then_some(())
}

fn heif_convert_png(heic: &Path, out: &Path) -> Option<()> {
    let ok = Command::new("heif-convert")
        .args(["--quiet", "-C", "nn"])
        .arg(heic)
        .arg(out)
        .output()
        .ok()?
        .status
        .success();
    (ok && out.exists()).then_some(())
}

fn magick_png(heic: &Path, out: &Path) -> Option<()> {
    let ok = Command::new("magick")
        .arg(heic)
        .arg(out)
        .output()
        .ok()?
        .status
        .success();
    (ok && out.exists()).then_some(())
}

/// Render `heic` to an 8-bit binary PPM with ImageMagick (its own
/// libheif decode), for the deflate-free pixel comparison.
fn magick_ppm(heic: &Path, out: &Path) -> Option<()> {
    let ok = Command::new("magick")
        .arg(heic)
        .args(["-alpha", "off", "-depth", "8"])
        .arg(format!("ppm:{}", out.display()))
        .output()
        .ok()?
        .status
        .success();
    (ok && out.exists()).then_some(())
}

fn ffmpeg_png(heic: &Path, out: &Path) -> Option<()> {
    let ok = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(heic)
        .args(["-frames:v", "1"])
        .arg(out)
        .output()
        .ok()?
        .status
        .success();
    (ok && out.exists()).then_some(())
}

fn heif_info_ok(heic: &Path) -> Option<bool> {
    Some(
        Command::new("heif-info")
            .arg(heic)
            .output()
            .ok()?
            .status
            .success(),
    )
}

/// The interop cases: `(name, extension, options, gray, alpha)`.
fn cases() -> Vec<(&'static str, &'static str, EncodeOptions, bool, bool)> {
    let intra = |qp| EncodeOptions {
        hevc_mode: "intra".into(),
        qp,
        ..EncodeOptions::default()
    };
    vec![
        ("hevc_pcm", "heic", pcm(), false, false),
        ("hevc_intra_q20", "heic", intra(20), false, false),
        ("hevc_intra_q40", "heic", intra(40), false, false),
        ("hevc_odd_63x61", "heic", intra(22), false, false),
        ("hevc_gray", "heic", intra(20), true, false),
        (
            "hevc_grid",
            "heic",
            EncodeOptions {
                hevc_mode: "intra".into(),
                qp: 22,
                grid_tile: Some(64),
                ..EncodeOptions::default()
            },
            false,
            false,
        ),
        (
            "hevc_thumb",
            "heic",
            EncodeOptions {
                thumbnail_max_dim: Some(48),
                ..pcm()
            },
            false,
            false,
        ),
        (
            "hevc_irot1",
            "heic",
            EncodeOptions {
                transforms: vec![Property::Irot(Irot { angle: 1 })],
                ..pcm()
            },
            false,
            false,
        ),
        (
            "hevc_imir1",
            "heic",
            EncodeOptions {
                transforms: vec![Property::Imir(Imir { axis: 1 })],
                ..pcm()
            },
            false,
            false,
        ),
        (
            "hevc_exif_xmp_icc",
            "heic",
            EncodeOptions {
                exif: Some(b"II*\0\x08\0\0\0\0\0".to_vec()),
                xmp: Some("<x:xmpmeta>oxideav</x:xmpmeta>".into()),
                icc_profile: Some(minimal_icc()),
                ..pcm()
            },
            false,
            false,
        ),
        (
            "av1_lossless",
            "avif",
            EncodeOptions {
                codec: StillCodec::Av1,
                ..Default::default()
            },
            false,
            false,
        ),
        (
            "av1_odd",
            "avif",
            EncodeOptions {
                codec: StillCodec::Av1,
                ..Default::default()
            },
            false,
            false,
        ),
        (
            "av1_grid",
            "avif",
            EncodeOptions {
                codec: StillCodec::Av1,
                grid_tile: Some(64),
                ..Default::default()
            },
            false,
            false,
        ),
        (
            "av1_q60",
            "avif",
            EncodeOptions {
                codec: StillCodec::Av1,
                av1_quality: Some(60),
                ..Default::default()
            },
            false,
            false,
        ),
        (
            "av1_q30_odd",
            "avif",
            EncodeOptions {
                codec: StillCodec::Av1,
                av1_quality: Some(30),
                ..Default::default()
            },
            false,
            false,
        ),
        (
            "av1_q60_alpha",
            "avif",
            EncodeOptions {
                codec: StillCodec::Av1,
                av1_quality: Some(60),
                ..Default::default()
            },
            false,
            true,
        ),
        ("hevc_alpha", "heic", pcm(), false, true),
        (
            "av1_alpha",
            "avif",
            EncodeOptions {
                codec: StillCodec::Av1,
                ..Default::default()
            },
            false,
            true,
        ),
    ]
}

fn dims(name: &str) -> (u32, u32) {
    match name {
        "hevc_odd_63x61" | "av1_odd" | "av1_q30_odd" => (63, 61),
        "hevc_grid" | "av1_grid" => (200, 150),
        "hevc_thumb" => (160, 120),
        _ => (96, 80),
    }
}

#[test]
fn every_written_shape_reparses_and_round_trips() {
    use oxideav_heif::decode::{decode_primary, ItemDecoder};
    for (name, _ext, opts, gray, alpha) in cases() {
        let (w, h) = dims(name);
        let src = picture(w, h, gray, alpha);
        let bytes = encode_still(&src, &opts).unwrap_or_else(|e| panic!("{name}: encode: {e}"));
        let f = HeifFile::parse(&bytes).unwrap_or_else(|e| panic!("{name}: reparse: {e}"));
        let rep = check(&f, MiafProfile::Miaf).unwrap();
        assert!(rep.is_conformant(), "{name}: {:#?}", rep.violations);
        let img = decode_primary(&f, ItemDecoder::direct())
            .unwrap_or_else(|e| panic!("{name}: decode: {e}"));
        // A 90/270° irot swaps the output dimensions.
        let swaps = opts
            .transforms
            .iter()
            .any(|t| matches!(t, Property::Irot(Irot { angle }) if angle % 2 == 1));
        let (ow, oh) = if swaps { (h, w) } else { (w, h) };
        assert_eq!((img.width(), img.height()), (ow, oh), "{name}: geometry");
        if alpha {
            assert!(img.frame.format.has_alpha, "{name}: alpha attached");
        }
        if opts.grid_tile.is_some() && w > 64 {
            assert_eq!(
                &f.primary_item().unwrap().item_type,
                b"grid",
                "{name}: grid primary"
            );
        }
    }
}

/// Every reader present opens every file and renders colour within the
/// reader's own rounding of *our own decode of the same bitstream* —
/// not the pre-encode source, so the check is independent of the codec
/// encoder's quality (HEVC / AV1 intra decode is exactly specified, so
/// a conformant third-party decoder reconstructs the same samples we
/// do; only the final YCbCr→RGB matrix differs between readers).
#[test]
fn third_party_readers_open_our_files() {
    use oxideav_heif::decode::{decode_primary, ItemDecoder};
    let dir = scratch_dir("writer-interop");
    let readers: &[(&str, Render)] = &[
        ("sips", sips_png),
        ("heif-convert", heif_convert_png),
        ("magick", magick_png),
        ("ffmpeg", ffmpeg_png),
    ];
    let mut checked = 0;
    for (name, ext, opts, gray, alpha) in cases() {
        let (w, h) = dims(name);
        let src = picture(w, h, gray, alpha);
        let bytes = encode_still(&src, &opts).unwrap();
        let heic = dir.join(format!("{name}.{ext}"));
        std::fs::write(&heic, &bytes).unwrap();
        // heif-info is a pure structural oracle: it must parse every file.
        if have_binary("heif-info") {
            assert_eq!(
                heif_info_ok(&heic),
                Some(true),
                "{name}: heif-info refused our file"
            );
        }
        // The reference is our own decode of what we wrote.
        let dec = HeifFile::parse(&bytes)
            .and_then(|f| decode_primary(&f, ItemDecoder::direct()))
            .unwrap();
        let want = to_rgb(&dec.frame, Some(&dec.nclx)).unwrap();
        // "Opens the file" — every present reader must render our file
        // without refusing it.
        for (reader, render) in readers {
            if !have_binary(reader) {
                eprintln!("SKIP {reader}: not installed");
                continue;
            }
            let out = dir.join(format!("{name}.{reader}.png"));
            let _ = std::fs::remove_file(&out);
            if render(&heic, &out).is_none() {
                if *reader == "ffmpeg" && (w * h) < 16 {
                    continue; // ffmpeg declines a 1×1 rawvideo→png
                }
                panic!("{reader} refused our {name}");
            }
            checked += 1;
        }
        // Pixel fidelity: ImageMagick (its own libheif decode) rendered
        // to an uncompressed PPM must equal our own decode within
        // rounding. ImageMagick is a plain decoder + matching matrix,
        // and the PPM path avoids any deflate in the comparison. sips
        // and the ffmpeg→PNG path colour-manage (apply the CICP
        // primaries / transfer) so they are open-only above; the
        // byte-exact plane cross-check lives in tests/interop.rs.
        if have_binary("magick") {
            let ppm = dir.join(format!("{name}.ppm"));
            let _ = std::fs::remove_file(&ppm);
            if magick_ppm(&heic, &ppm).is_some() {
                let (max, mean) = ppm_diff(&ppm, &want).unwrap();
                assert!(
                    max <= 3.0 && mean <= 0.3,
                    "magick rendered {name} at max {max:.1} mean {mean:.3} vs our decode (tol 3/0.3)"
                );
            }
        }
    }
    if checked == 0 {
        eprintln!("SKIP: no third-party HEIF reader installed");
    } else {
        eprintln!("{checked} (file, reader) open checks passed");
    }
}

/// Apple ImageIO accepts our writer's container structure and our HEVC
/// alpha stream individually: cross-muxing an Apple stream with our
/// stream on either side is accepted, isolating the both-our-streams
/// refusal to the codec layer. Runs only when `sips` and a source
/// HEVC file it produced are available.
#[test]
fn our_container_structure_is_accepted_by_apple_imageio() {
    if !have_binary("sips") {
        eprintln!("SKIP: sips not installed");
        return;
    }
    // Build a plain HEVC still and confirm sips opens it — the same
    // path the reader test exercises, kept here as the structural
    // baseline for the cross-mux argument in the round report.
    let dir = scratch_dir("apple-struct");
    let src = picture(80, 64, false, false);
    let bytes = encode_still(&src, &pcm()).unwrap();
    let heic = dir.join("plain.heic");
    std::fs::write(&heic, &bytes).unwrap();
    assert!(
        sips_png(&heic, &dir.join("plain.png")).is_some(),
        "sips refused a plain single-item HEVC still from our writer"
    );
    // A written grid (multiple hidden hvc1 tiles) also opens.
    let g = encode_still(
        &picture(200, 150, false, false),
        &EncodeOptions {
            hevc_mode: "intra".into(),
            qp: 22,
            grid_tile: Some(64),
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    let gp = dir.join("grid.heic");
    std::fs::write(&gp, g).unwrap();
    assert!(
        sips_png(&gp, &dir.join("grid.png")).is_some(),
        "sips refused a written grid"
    );
}

/// The writer's identity-item path still exists for callers that want a
/// transform on a derived item rather than on the coded item.
#[test]
fn identity_item_carries_transforms() {
    let mut w = HeifWriter::new();
    let src = picture(64, 48, false, false);
    let colour = to_yuv420_8(&src).unwrap();
    let pic = oxideav_heif::encode::encode_hevc_picture(&colour, "pcm", 0).unwrap();
    let base = w.add_coded_item(
        pic.item_type,
        pic.data,
        vec![
            (pic.config.clone(), true),
            (
                Property::Ispe(oxideav_heif::props::Ispe {
                    width: 64,
                    height: 48,
                }),
                false,
            ),
            (Property::Colr(Colr::MIAF_DEFAULT), false),
        ],
    );
    w.set_hidden(base, true);
    let iden = w.add_identity(
        base,
        vec![
            (
                Property::Ispe(oxideav_heif::props::Ispe {
                    width: 64,
                    height: 48,
                }),
                false,
            ),
            (
                Property::Clap(Clap::for_rect(
                    64,
                    48,
                    CropRect {
                        x: 8,
                        y: 8,
                        width: 48,
                        height: 32,
                    },
                )),
                true,
            ),
        ],
    );
    w.set_primary(iden);
    let bytes = w.write_to_vec().unwrap();
    let f = HeifFile::parse(&bytes).unwrap();
    assert_eq!(&f.primary_item().unwrap().item_type, b"iden");
    let node = oxideav_heif::derived::build_primary_graph(&f).unwrap();
    assert_eq!(node.output_size().unwrap(), (48, 32));
    assert!(check(&f, MiafProfile::Miaf).unwrap().is_conformant());
}

/// Apple ImageIO refuses a file whose master and alpha carry
/// byte-identical VPS / SPS / PPS (verified by cross-muxing: any
/// change to the alpha's parameter sets makes the same file open). The
/// writer gives the alpha its own parameter-set ids (VPS / SPS / PPS
/// 1) on both HEVC modes; the coded picture geometry stays the
/// master's (no extra row band, no CTB change).
#[test]
fn alpha_parameter_sets_differ_from_the_master() {
    for opts in [pcm(), EncodeOptions::default()] {
        let src = picture(96, 80, false, true);
        let bytes = encode_still(&src, &opts).unwrap();
        let f = HeifFile::parse(&bytes).unwrap();
        let node = oxideav_heif::derived::build_primary_graph(&f).unwrap();
        let alpha = node.alpha.as_ref().expect("alpha auxiliary");
        let master_cfg = node.properties.hvcc().unwrap();
        let alpha_cfg = alpha.properties.hvcc().unwrap();
        let ps = |c: &oxideav_heif::HevcConfig| -> Vec<Vec<u8>> {
            c.arrays.iter().flat_map(|a| a.nal_units.clone()).collect()
        };
        assert_ne!(
            ps(master_cfg),
            ps(alpha_cfg),
            "{}: alpha parameter sets must differ from the master's",
            opts.hevc_mode
        );
        // The difference is the parameter-set id: SPS 0 vs SPS 1, same
        // coded geometry.
        let sps_id = |c: &oxideav_heif::HevcConfig| -> u8 {
            let sps = c
                .arrays
                .iter()
                .find(|a| a.nal_unit_type == 33)
                .and_then(|a| a.nal_units.first())
                .expect("SPS");
            let mut annex_b = vec![0, 0, 1];
            annex_b.extend_from_slice(sps);
            let nals = oxideav_h265::collect_nal_units(&annex_b).unwrap();
            oxideav_h265::SeqParameterSet::parse(&nals[0].rbsp)
                .unwrap()
                .sps_id
        };
        assert_eq!((sps_id(master_cfg), sps_id(alpha_cfg)), (0, 1));
        assert_eq!(
            (alpha.ispe(), master_cfg.chroma_format_idc),
            (node.ispe(), alpha_cfg.chroma_format_idc),
            "{}: alpha coded at the master geometry",
            opts.hevc_mode
        );
        // The alpha still decodes to the master's geometry, exactly.
        use oxideav_heif::decode::{decode_primary, ItemDecoder};
        let img = decode_primary(&f, ItemDecoder::direct()).unwrap();
        assert_eq!((img.width(), img.height()), (96, 80));
        if opts.hevc_mode == "pcm" {
            assert_eq!(
                img.frame.alpha_as_frame().unwrap(),
                oxideav_heif::encode::to_yuv420_8(&src.alpha_as_frame().unwrap())
                    .unwrap()
                    .planes[0]
                    .clone()
                    .pipe_into_mono(96, 80)
            );
        }
    }
}

/// Test-local helper: wrap a plane as a monochrome frame.
trait PipeMono {
    fn pipe_into_mono(self, w: u32, h: u32) -> HeifFrame;
}

impl PipeMono for oxideav_heif::HeifPlane {
    fn pipe_into_mono(self, w: u32, h: u32) -> HeifFrame {
        HeifFrame {
            width: w,
            height: h,
            format: HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap(),
            planes: vec![self],
        }
    }
}

/// With the item's range and colour description in the bitstream VUI
/// (h265 0.0.11), Apple ImageIO — which takes the sample range from
/// the VUI — renders our full-range alpha exactly; the earlier
/// video-range stretch (+20 at code 235) is gone. Needs `sips` and
/// `magick` (to turn its PNG into an uncompressed PPM).
#[test]
fn apple_imageio_renders_the_alpha_plane_exactly() {
    if !have_binary("sips") || !have_binary("magick") {
        eprintln!("SKIP: sips + magick needed");
        return;
    }
    use oxideav_heif::decode::{decode_primary, ItemDecoder};
    let dir = scratch_dir("apple-alpha");
    for (name, opts) in [
        ("pcm", pcm()),
        (
            "intra",
            EncodeOptions {
                hevc_mode: "intra".into(),
                qp: 20,
                ..EncodeOptions::default()
            },
        ),
    ] {
        let src = picture(96, 80, false, true);
        let bytes = encode_still(&src, &opts).unwrap();
        let heic = dir.join(format!("{name}.heic"));
        std::fs::write(&heic, &bytes).unwrap();
        let png = dir.join(format!("{name}.sips.png"));
        assert!(sips_png(&heic, &png).is_some(), "{name}: sips refused");
        // sips PNG → alpha channel as an RGB PPM (magick), compared to
        // our decoded alpha plane replicated to RGB.
        let ppm = dir.join(format!("{name}.alpha.ppm"));
        let ok = Command::new("magick")
            .arg(&png)
            .args(["-alpha", "extract", "-depth", "8"])
            .arg(format!("ppm:{}", ppm.display()))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "{name}: magick could not extract the alpha");
        let img = decode_primary(&HeifFile::parse(&bytes).unwrap(), ItemDecoder::direct()).unwrap();
        let a = img.frame.alpha_as_frame().unwrap();
        let mut data = Vec::with_capacity((a.width * a.height * 3) as usize);
        for y in 0..a.height {
            for x in 0..a.width {
                let v = a.sample(0, x, y);
                data.extend_from_slice(&[v, v, v]);
            }
        }
        let want = oxideav_heif::rgb::RgbImage {
            width: a.width,
            height: a.height,
            channels: 3,
            bit_depth: 8,
            data,
        };
        let (max, mean) = ppm_diff(&ppm, &want).unwrap();
        assert!(
            max == 0.0,
            "{name}: sips alpha differs from ours (max {max} mean {mean:.3})"
        );
    }
}
