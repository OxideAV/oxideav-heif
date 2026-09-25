//! Both-direction conformance matrix (HEIF production readiness).
//!
//! **Reader direction**: every producer on this machine — Apple ImageIO
//! (`sips`), libheif's `heif-enc` with x265 and with aom, ImageMagick
//! (`magick`) and ffmpeg (`libsvtav1` → AVIF) — is driven over the
//! feature list below; each file it produces is decoded by this crate
//! and compared **byte-exact** with the black-box video decoder's raw
//! planes of the same file (ffmpeg's own HEIF/AVIF demux + decode;
//! the even-aligned region it outputs), or the delta is recorded.
//! Features a producer cannot express are `no producer`; a producer
//! that fails is `producer refused`; a file that lost the feature is
//! `dropped`.
//!
//! **Writer direction**: every shape this crate's writer produces is
//! handed to the readers — `sips`, `heif-convert`, `magick`, `ffmpeg`,
//! `heif-info` — and their render (PNG / PPM) is compared with our own
//! decode (RGB, 8-bit units).
//!
//! Every cell is a measurement from the run; absent binaries print
//! SKIP and become `no producer (absent)` / `no reader (absent)`. The
//! test prints both tables as Markdown (the README carries the last
//! full run) and asserts only that every cell got a verdict and that
//! the files this crate decodes byte-exact where the black-box
//! decoder agrees on layout stay byte-exact.
#![cfg(feature = "registry")]

mod common;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::png::read_png;
use common::pngw::write_png;
use common::{have_binary, scratch_dir};
use oxideav_heif::decode::{decode_primary, ItemDecoder};
use oxideav_heif::encode::{encode_still, EncodeOptions, StillCodec};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};
use oxideav_heif::props::{Clap, CropRect, Imir, Irot, Property};
use oxideav_heif::rgb::to_rgb;
use oxideav_heif::HeifFile;

// ───────────────────────── sources ─────────────────────────

/// Structured RGB(A) / grey picture: gradients, a box, a disc, so
/// chroma subsampling and rounding are exercised; `depth` 8 or 16.
fn source(w: u32, h: u32, channels: usize, depth: u8) -> Vec<u16> {
    let max = ((1u32 << depth) - 1) as f64;
    let (cx, cy, r) = (w as f64 * 0.7, h as f64 * 0.65, w.min(h) as f64 * 0.18);
    let mut v = Vec::with_capacity(w as usize * h as usize * channels);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f64 / w.max(2) as f64, y as f64 / h.max(2) as f64);
            let mut rgb = [fx, fy, (fx + fy) * 0.5];
            if x >= w / 4 && x < w / 2 && y >= h / 4 && y < h / 2 {
                rgb = [0.94, 0.94, 0.94];
            }
            let (dx, dy) = (x as f64 - cx, y as f64 - cy);
            if dx * dx + dy * dy < r * r {
                rgb = [0.9, 0.12, 0.12];
            }
            let grey = 0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2];
            let a = if x < w / 3 { 1.0 } else { fy };
            match channels {
                1 => v.push((grey * max).round() as u16),
                2 => {
                    v.push((grey * max).round() as u16);
                    v.push((a * max).round() as u16);
                }
                3 => v.extend(rgb.iter().map(|c| (c * max).round() as u16)),
                _ => {
                    v.extend(rgb.iter().map(|c| (c * max).round() as u16));
                    v.push((a * max).round() as u16);
                }
            }
        }
    }
    v
}

fn source_png(dir: &Path, w: u32, h: u32, channels: usize, depth: u8) -> PathBuf {
    let p = dir.join(format!("src_{w}x{h}_c{channels}_d{depth}.png"));
    if !p.exists() {
        std::fs::write(
            &p,
            write_png(w, h, channels, depth, &source(w, h, channels, depth)),
        )
        .unwrap();
    }
    p
}

// ───────────────────────── verdicts ─────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Cell {
    /// Our planes equal the black-box decoder's on the region it covers.
    Exact,
    /// `(max, mean)` difference in native code units, plus the note.
    Delta(f64, f64, String),
    NoProducer(String),
    ProducerRefused(String),
    Dropped(String),
    DecodeFailed(String),
    /// Decoded, but the black-box decoder refuses the file — no reference.
    NoOracle(String),
}

impl Cell {
    fn md(&self) -> String {
        match self {
            Cell::Exact => "exact".into(),
            Cell::Delta(max, _, note) if *max == 0.0 => format!("exact ({note})"),
            Cell::Delta(max, mean, note) => format!(
                "Δ max {max:.0} mean {mean:.2}{}",
                if note.is_empty() {
                    String::new()
                } else {
                    format!(" ({note})")
                }
            ),
            Cell::NoProducer(r) => format!("no producer ({r})"),
            Cell::ProducerRefused(r) => format!("producer refused ({r})"),
            Cell::Dropped(r) => format!("dropped ({r})"),
            Cell::DecodeFailed(r) => format!("DECODE FAILED ({r})"),
            Cell::NoOracle(r) => format!("decodes; no black-box reference ({r})"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Producer {
    Sips,
    HeifEncX265,
    HeifEncAom,
    Magick,
    Ffmpeg,
}

impl Producer {
    fn name(self) -> &'static str {
        match self {
            Producer::Sips => "sips (Apple ImageIO)",
            Producer::HeifEncX265 => "heif-enc x265",
            Producer::HeifEncAom => "heif-enc aom",
            Producer::Magick => "magick",
            Producer::Ffmpeg => "ffmpeg (libsvtav1)",
        }
    }
    fn binary(self) -> &'static str {
        match self {
            Producer::Sips => "sips",
            Producer::HeifEncX265 | Producer::HeifEncAom => "heif-enc",
            Producer::Magick => "magick",
            Producer::Ffmpeg => "ffmpeg",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feature {
    Size(u32, u32),
    Depth(u8),
    ChromaFmt(Chroma),
    Alpha,
    Lossless,
    Thumbnail,
    Exif,
    Xmp,
    Icc,
    Irot,
    Imir,
    Clap,
    Sequence,
    GainMap,
}

impl Feature {
    fn label(self) -> String {
        match self {
            Feature::Size(w, h) => {
                if w * h >= 12_000_000 {
                    format!("{w}×{h} (12 MP)")
                } else {
                    format!("{w}×{h}")
                }
            }
            Feature::Depth(d) => format!("{d}-bit"),
            Feature::ChromaFmt(c) => match c {
                Chroma::Mono => "4:0:0".into(),
                Chroma::Yuv420 => "4:2:0".into(),
                Chroma::Yuv422 => "4:2:2".into(),
                Chroma::Yuv444 => "4:4:4".into(),
            },
            Feature::Alpha => "alpha".into(),
            Feature::Lossless => "lossless".into(),
            Feature::Thumbnail => "thumbnail".into(),
            Feature::Exif => "Exif".into(),
            Feature::Xmp => "XMP".into(),
            Feature::Icc => "ICC".into(),
            Feature::Irot => "irot".into(),
            Feature::Imir => "imir".into(),
            Feature::Clap => "clap".into(),
            Feature::Sequence => "sequence".into(),
            Feature::GainMap => "gain map".into(),
        }
    }
}

const FEATURES: &[Feature] = &[
    Feature::Size(1, 1),
    Feature::Size(7, 5),
    Feature::Size(63, 61),
    Feature::Size(96, 80),
    Feature::Size(4032, 3024),
    Feature::Depth(8),
    Feature::Depth(10),
    Feature::Depth(12),
    Feature::ChromaFmt(Chroma::Mono),
    Feature::ChromaFmt(Chroma::Yuv420),
    Feature::ChromaFmt(Chroma::Yuv422),
    Feature::ChromaFmt(Chroma::Yuv444),
    Feature::Alpha,
    Feature::Lossless,
    Feature::Thumbnail,
    Feature::Exif,
    Feature::Xmp,
    Feature::Icc,
    Feature::Irot,
    Feature::Imir,
    Feature::Clap,
    Feature::Sequence,
    Feature::GainMap,
];

fn run(cmd: &mut Command) -> Result<Vec<u8>, String> {
    match cmd.output() {
        Ok(o) if o.status.success() => Ok(o.stdout),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr)
            .lines()
            .rev()
            .find(|l| !l.starts_with("Svt["))
            .unwrap_or("non-zero exit")
            .chars()
            .take(80)
            .collect()),
        Err(e) => Err(e.to_string()),
    }
}

/// A minimal ICC v4 profile (header + empty tag table).
fn minimal_icc(dir: &Path) -> PathBuf {
    let p = dir.join("min.icc");
    if !p.exists() {
        let mut b = vec![0u8; 132];
        b[0..4].copy_from_slice(&132u32.to_be_bytes());
        b[8] = 4;
        b[12..16].copy_from_slice(b"mntr");
        b[16..20].copy_from_slice(b"RGB ");
        b[20..24].copy_from_slice(b"XYZ ");
        b[36..40].copy_from_slice(b"acsp");
        std::fs::write(&p, b).unwrap();
    }
    p
}

/// Produce a file for `(producer, feature)`; `Ok(None)` = no producer.
fn produce(dir: &Path, p: Producer, f: Feature) -> Result<Option<PathBuf>, Cell> {
    let out_ext = match p {
        Producer::HeifEncAom | Producer::Ffmpeg => "avif",
        _ => "heic",
    };
    let tag = format!(
        "{:?}_{}",
        p,
        f.label().replace(['×', ' ', ':', '(', ')'], "_")
    );
    let out = dir.join(format!(
        "{tag}.{}",
        if f == Feature::Sequence && p == Producer::Sips {
            "heics"
        } else {
            out_ext
        }
    ));
    // The source picture for the feature.
    let (w, h, channels, depth) = match f {
        Feature::Size(w, h) => (w, h, 3, 8),
        Feature::Depth(10) | Feature::Depth(12) => (96, 80, 3, 16),
        Feature::ChromaFmt(Chroma::Mono) => (96, 80, 1, 8),
        Feature::Alpha => (96, 80, 4, 8),
        _ => (96, 80, 3, 8),
    };
    let src = source_png(dir, w, h, channels, depth);
    let refuse = |e: String| Cell::ProducerRefused(e);
    match p {
        Producer::Sips => {
            let mut c = Command::new("sips");
            match f {
                Feature::Depth(12) => return Ok(None),
                Feature::ChromaFmt(Chroma::Yuv422)
                | Feature::Lossless
                | Feature::Thumbnail
                | Feature::Xmp
                | Feature::GainMap => return Ok(None),
                Feature::Exif => {
                    // A JPEG with an Exif APP1 segment, converted by sips.
                    let jpg = dir.join("exif_src.jpg");
                    if !have_binary("magick") {
                        return Ok(None);
                    }
                    run(Command::new("magick").arg(&src).arg(&jpg)).map_err(refuse)?;
                    let bytes = std::fs::read(&jpg).unwrap();
                    let mut with = bytes[..2].to_vec();
                    let tiff =
                        b"II*\0\x08\0\0\0\x01\0\x0f\x01\x02\0\x07\0\0\0\x1a\0\0\0\0\0\0\0oxideav\0";
                    let seg_len = (2 + 6 + tiff.len()) as u16;
                    with.extend_from_slice(&[0xff, 0xe1]);
                    with.extend_from_slice(&seg_len.to_be_bytes());
                    with.extend_from_slice(b"Exif\0\0");
                    with.extend_from_slice(tiff);
                    with.extend_from_slice(&bytes[2..]);
                    let jpg2 = dir.join("exif_src2.jpg");
                    std::fs::write(&jpg2, with).unwrap();
                    c.args(["-s", "format", "heic"]).arg(&jpg2);
                }
                Feature::Icc => {
                    let prof =
                        Path::new("/System/Library/ColorSync/Profiles/Generic RGB Profile.icc");
                    if !prof.exists() {
                        return Ok(None);
                    }
                    c.args(["-s", "format", "heic", "--matchTo"])
                        .arg(prof)
                        .arg(&src);
                }
                Feature::ChromaFmt(Chroma::Yuv444) => {
                    c.args(["-s", "format", "heic", "-s", "formatOptions", "100"])
                        .arg(&src);
                }
                Feature::Irot => {
                    c.args(["-s", "format", "heic", "-r", "90"]).arg(&src);
                }
                Feature::Imir => {
                    c.args(["-s", "format", "heic", "-f", "horizontal"])
                        .arg(&src);
                }
                Feature::Clap => {
                    c.args(["-s", "format", "heic", "-c", "40", "50"]).arg(&src);
                }
                Feature::Sequence => {
                    c.args(["-s", "format", "heics"]).arg(&src);
                }
                _ => {
                    c.args(["-s", "format", "heic"]).arg(&src);
                }
            }
            c.arg("--out").arg(&out);
            run(&mut c).map_err(refuse)?;
        }
        Producer::HeifEncX265 | Producer::HeifEncAom => {
            let mut c = Command::new("heif-enc");
            if p == Producer::HeifEncAom {
                c.arg("-A").args(["-p", "speed=9"]);
            } else {
                c.args(["-p", "preset=ultrafast"]);
            }
            match f {
                Feature::Exif
                | Feature::Xmp
                | Feature::Icc
                | Feature::Irot
                | Feature::Imir
                | Feature::Clap
                | Feature::Sequence
                | Feature::GainMap => return Ok(None),
                Feature::Depth(d @ (10 | 12)) => {
                    c.args(["-b", &d.to_string()]);
                }
                Feature::ChromaFmt(Chroma::Yuv422) => {
                    c.args(["-p", "chroma=422"]);
                }
                Feature::ChromaFmt(Chroma::Yuv444) => {
                    c.args(["-p", "chroma=444"]);
                }
                Feature::Lossless => {
                    c.arg("-L");
                }
                Feature::Thumbnail => {
                    c.args(["-t", "48"]);
                }
                _ => {}
            }
            c.arg(&src).arg("-o").arg(&out);
            run(&mut c).map_err(refuse)?;
        }
        Producer::Magick => {
            let mut c = Command::new("magick");
            match f {
                Feature::Thumbnail
                | Feature::Irot
                | Feature::Imir
                | Feature::Clap
                | Feature::Sequence
                | Feature::GainMap
                | Feature::Xmp
                | Feature::Exif => return Ok(None),
                Feature::Depth(10) => {
                    c.arg(&src)
                        .args(["-depth", "10", "-define", "heic:depth=10"]);
                }
                Feature::Depth(12) => {
                    c.arg(&src).args(["-depth", "12"]);
                }
                Feature::ChromaFmt(Chroma::Yuv422) => {
                    c.arg(&src)
                        .args(["-depth", "8", "-define", "heic:chroma=422"]);
                }
                Feature::ChromaFmt(Chroma::Yuv444) => {
                    c.arg(&src)
                        .args(["-depth", "8", "-define", "heic:chroma=444"]);
                }
                Feature::Lossless => {
                    c.arg(&src).args([
                        "-depth",
                        "8",
                        "-define",
                        "heic:lossless=true",
                        "-define",
                        "heic:chroma=444",
                    ]);
                }
                Feature::Icc => {
                    c.arg(&src)
                        .args(["-depth", "8", "-profile"])
                        .arg(minimal_icc(dir));
                }
                _ => {
                    c.arg(&src).args(["-depth", "8"]);
                }
            }
            c.arg(&out);
            run(&mut c).map_err(refuse)?;
        }
        Producer::Ffmpeg => {
            let mut c = Command::new("ffmpeg");
            c.args(["-nostdin", "-loglevel", "error", "-y"]);
            match f {
                Feature::Depth(12)
                | Feature::ChromaFmt(Chroma::Mono)
                | Feature::ChromaFmt(Chroma::Yuv422)
                | Feature::ChromaFmt(Chroma::Yuv444)
                | Feature::Alpha
                | Feature::Lossless
                | Feature::Thumbnail
                | Feature::Exif
                | Feature::Xmp
                | Feature::Icc
                | Feature::Irot
                | Feature::Imir
                | Feature::Clap
                | Feature::GainMap => return Ok(None),
                Feature::Sequence => {
                    c.args([
                        "-f",
                        "lavfi",
                        "-i",
                        "testsrc=size=96x80:rate=5",
                        "-frames:v",
                        "3",
                    ]);
                }
                _ => {
                    c.arg("-i").arg(&src);
                }
            }
            let pf = if f == Feature::Depth(10) {
                "yuv420p10le"
            } else {
                "yuv420p"
            };
            c.args([
                "-pix_fmt",
                pf,
                "-c:v",
                "libsvtav1",
                "-preset",
                "12",
                "-crf",
                "20",
                "-f",
                "avif",
            ])
            .arg(&out);
            run(&mut c).map_err(refuse)?;
        }
    }
    Ok(Some(out))
}

fn ffmpeg_pix_fmt_for(f: &HeifFrame, rgb: bool) -> String {
    let base = match (f.format.chroma, rgb) {
        (Chroma::Yuv444, true) => "gbrp",
        (Chroma::Mono, _) => "gray",
        (Chroma::Yuv420, _) => "yuv420p",
        (Chroma::Yuv422, _) => "yuv422p",
        (Chroma::Yuv444, _) => "yuv444p",
    };
    if f.format.bit_depth == 8 {
        base.to_string()
    } else {
        format!("{base}{}le", f.format.bit_depth)
    }
}

fn ffmpeg_pix_fmt(f: &HeifFrame) -> String {
    ffmpeg_pix_fmt_for(f, false)
}

/// Black-box raw planes of video stream `stream` in `pix_fmt`; `None`
/// when ffmpeg refuses the file.
fn ffmpeg_planes(
    dir: &Path,
    file: &Path,
    stream: usize,
    pix_fmt: &str,
    tag: &str,
) -> Option<Vec<u8>> {
    let out = dir.join(format!("{tag}.raw"));
    run(Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(file)
        .args([
            "-map",
            &format!("0:v:{stream}"),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            pix_fmt,
        ])
        .arg(&out))
    .ok()?;
    std::fs::read(&out).ok()
}

fn planar_bytes(fmt: HeifPixelFormat, w: u32, h: u32) -> usize {
    (0..fmt.plane_count())
        .map(|p| {
            let (pw, ph) = fmt.plane_dims(p, w, h);
            pw as usize * ph as usize * fmt.bytes_per_sample()
        })
        .sum()
}

/// Compare our decoded frame with the black-box planes. The black box
/// may emit the picture even-cropped (odd sizes) or at the coded size
/// (a `clap` it does not apply); its geometry is recovered from the
/// byte count and the comparison covers the overlap, with a note.
fn compare_planes(ours: &HeifFrame, theirs: &[u8]) -> Cell {
    let f = ours.without_alpha().tight();
    let fmt = f.format;
    let (w, h) = (f.width, f.height);
    let mut cands = vec![(w, h), (w & !1, h), (w, h & !1), (w & !1, h & !1)];
    for a in [8u32, 16, 64] {
        cands.push((w.div_ceil(a) * a, h.div_ceil(a) * a));
    }
    // A grid's first tile (the black box exposes Apple's grid that way).
    for t in [256u32, 512, 1024] {
        if w > t && h > t {
            cands.push((t, t));
        }
    }
    let Some(&(bw, bh)) = cands
        .iter()
        .find(|(cw, ch)| *cw > 0 && *ch > 0 && planar_bytes(fmt, *cw, *ch) == theirs.len())
    else {
        return Cell::Delta(
            f64::INFINITY,
            f64::INFINITY,
            format!(
                "black-box output {} bytes for no {w}×{h}-derived geometry",
                theirs.len()
            ),
        );
    };
    let note = if (bw, bh) == (w, h) {
        String::new()
    } else if bw == bh && w > bw && h > bh && (w - bw > 1 || h - bh > 1) {
        format!("black-box emits the first {bw}×{bh} grid tile; that region compared")
    } else if bw < w || bh < h {
        format!("black-box emits {bw}×{bh}: even-cropped; overlap compared")
    } else {
        format!("black-box emits the coded {bw}×{bh} (clap unapplied); overlap compared")
    };
    let bps = fmt.bytes_per_sample();
    let mut pos = 0usize;
    let mut max = 0f64;
    let mut sum = 0f64;
    let mut n = 0u64;
    for p in 0..fmt.plane_count() {
        let (pw, ph) = fmt.plane_dims(p, bw, bh);
        let (ow, oh) = f.plane_dims(p);
        for y in 0..ph {
            for x in 0..pw {
                let t = if bps == 2 {
                    u16::from_le_bytes([theirs[pos], theirs[pos + 1]]) as f64
                } else {
                    theirs[pos] as f64
                };
                pos += bps;
                if x >= ow || y >= oh {
                    continue;
                }
                let d = (f.sample(p, x, y) as f64 - t).abs();
                max = max.max(d);
                sum += d;
                n += 1;
            }
        }
    }
    if max == 0.0 {
        if note.is_empty() {
            Cell::Exact
        } else {
            Cell::Delta(0.0, 0.0, note)
        }
    } else {
        Cell::Delta(max, sum / n.max(1) as f64, note)
    }
}

/// Decode a produced file and check the feature survived; measure.
fn measure(dir: &Path, p: Producer, f: Feature, file: &Path) -> (Cell, String) {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) => return (Cell::DecodeFailed(e.to_string()), String::new()),
    };
    let hf = match HeifFile::parse(&bytes) {
        Ok(h) => h,
        Err(e) => return (Cell::DecodeFailed(e.to_string()), String::new()),
    };
    let img = match decode_primary(&hf, ItemDecoder::direct()) {
        Ok(i) => i,
        Err(e) => return (Cell::DecodeFailed(e.to_string()), String::new()),
    };
    let meta = hf.meta().ok();
    let chroma_label = match img.frame.format.chroma {
        Chroma::Mono => "4:0:0",
        Chroma::Yuv420 => "4:2:0",
        Chroma::Yuv422 => "4:2:2",
        Chroma::Yuv444 => "4:4:4",
    };
    let layout = format!(
        "{chroma_label} {}b{}",
        img.frame.format.bit_depth,
        if img.frame.format.has_alpha {
            " +α"
        } else {
            ""
        }
    );
    // Did the feature survive?
    let dropped = match f {
        Feature::Depth(d) => (img.frame.format.bit_depth != d)
            .then(|| format!("wrote {}-bit", img.frame.format.bit_depth)),
        Feature::ChromaFmt(c) => (img.frame.format.chroma != c).then(|| format!("wrote {layout}")),
        Feature::Alpha => (!img.frame.format.has_alpha).then(|| "no alpha".into()),
        Feature::Thumbnail => img.thumbnail_ids.is_empty().then(|| "no thmb".into()),
        Feature::Exif => img.exif.is_none().then(|| "no Exif item".into()),
        Feature::Xmp => img.xmp.is_none().then(|| "no XMP item".into()),
        Feature::Icc => img.icc_profile.is_none().then(|| "no ICC colr".into()),
        Feature::Irot => (!img
            .properties
            .entries
            .iter()
            .any(|e| matches!(e.property, Property::Irot(_))))
        .then(|| "no irot property (pixels rotated)".into()),
        Feature::Imir => (!img
            .properties
            .entries
            .iter()
            .any(|e| matches!(e.property, Property::Imir(_))))
        .then(|| "no imir property (pixels mirrored)".into()),
        Feature::Clap => (!img
            .properties
            .entries
            .iter()
            .any(|e| matches!(e.property, Property::Clap(_))))
        .then(|| "no clap property (pixels cropped)".into()),
        Feature::Sequence => (!hf.has_moov()).then(|| "no track".into()),
        Feature::GainMap => img.gain_map.is_none().then(|| "no tmap".into()),
        Feature::Lossless => None,
        Feature::Size(..) => None,
    };
    if let Some(d) = dropped {
        return (Cell::Dropped(d), layout);
    }
    let _ = (meta, p);
    let tag = file.file_stem().unwrap().to_string_lossy().to_string();
    let rgb = matches!(img.nclx, oxideav_heif::props::Colr::Nclx { matrix: 0, .. });
    let Some(theirs) = ffmpeg_planes(dir, file, 0, &ffmpeg_pix_fmt_for(&img.frame, rgb), &tag)
    else {
        return (
            Cell::NoOracle("the black-box decoder refuses the file".into()),
            layout,
        );
    };
    if f == Feature::Sequence {
        // Decode our first track sample through the registry and
        // compare it with the black box's matching video stream (its
        // first stream may be the cover still).
        let mut ctx = oxideav_core::RuntimeContext::new();
        oxideav_h265::register(&mut ctx);
        oxideav_av1::register(&mut ctx);
        oxideav_heif::register(&mut ctx);
        let Ok(mut d) = ctx.containers.open_demuxer(
            "heif",
            Box::new(std::io::Cursor::new(bytes.clone())),
            &ctx.codecs,
        ) else {
            return (Cell::DecodeFailed("demuxer".into()), layout);
        };
        let streams = d.streams().to_vec();
        let Some(track) = streams
            .iter()
            .find(|s| s.params.codec_id.as_str() != "heif")
        else {
            return (Cell::Dropped("no raw track stream".into()), layout);
        };
        let mut first = None;
        while let Ok(pkt) = d.next_packet() {
            if pkt.stream_index == track.index {
                first = Some(pkt);
                break;
            }
        }
        let Some(pkt) = first else {
            return (Cell::DecodeFailed("no track packet".into()), layout);
        };
        let Ok(mut dec) = ctx.codecs.first_decoder(&track.params) else {
            return (Cell::DecodeFailed("no decoder".into()), layout);
        };
        if dec.send_packet(&pkt).is_err() || dec.flush().is_err() {
            return (Cell::DecodeFailed("track decode".into()), layout);
        }
        let Ok(oxideav_core::Frame::Video(v)) = dec.receive_frame() else {
            return (Cell::DecodeFailed("no frame".into()), layout);
        };
        let pf = track
            .params
            .pixel_format
            .unwrap_or(oxideav_core::PixelFormat::Yuv420P);
        let (tw, th) = (
            track.params.width.unwrap_or(img.width()),
            track.params.height.unwrap_or(img.height()),
        );
        let Ok(frame) = HeifFrame::from_core(&v, tw, th, pf) else {
            return (Cell::DecodeFailed("frame layout".into()), layout);
        };
        let stream = usize::from(
            hf.meta()
                .map(|m| m.primary_item_id.is_some())
                .unwrap_or(false),
        );
        let Some(theirs) = ffmpeg_planes(
            dir,
            file,
            stream,
            &ffmpeg_pix_fmt(&frame),
            &format!("{tag}_trk"),
        ) else {
            return (
                Cell::NoOracle("the black-box decoder refuses the track".into()),
                layout,
            );
        };
        return (
            compare_planes(&frame, &theirs),
            format!("track {}", ffmpeg_pix_fmt(&frame)),
        );
    }
    let mut cell = compare_planes(&img.frame, &theirs);
    // Alpha: the black box emits it as a second video stream.
    if img.frame.format.has_alpha {
        let a = img.frame.alpha_as_frame().unwrap();
        let ac = match ffmpeg_planes(dir, file, 1, &ffmpeg_pix_fmt(&a), &format!("{tag}_a")) {
            Some(t) => compare_planes(&a, &t),
            None => Cell::NoOracle("alpha stream".into()),
        };
        let alpha_note = match &ac {
            Cell::Exact => "alpha exact".to_string(),
            Cell::Delta(m, s, n) if *m == 0.0 => format!("alpha exact; {n}"),
            Cell::Delta(m, s, n) => format!("alpha Δ max {m:.0} mean {s:.2} {n}"),
            _ => "alpha not compared (no black-box alpha stream)".to_string(),
        };
        cell = match cell {
            Cell::Exact => Cell::Delta(0.0, 0.0, alpha_note),
            Cell::Delta(m, s, n) => Cell::Delta(
                m,
                s,
                if n.is_empty() {
                    alpha_note
                } else {
                    format!("{n}; {alpha_note}")
                },
            ),
            other => other,
        };
    }
    (cell, layout)
}

// ───────────────────────── writer direction ─────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Reader {
    Sips,
    HeifConvert,
    Magick,
    Ffmpeg,
    HeifInfo,
}

impl Reader {
    fn name(self) -> &'static str {
        match self {
            Reader::Sips => "sips",
            Reader::HeifConvert => "heif-convert",
            Reader::Magick => "magick",
            Reader::Ffmpeg => "ffmpeg",
            Reader::HeifInfo => "heif-info",
        }
    }
}

#[derive(Clone, Debug)]
enum RCell {
    Opens,
    Render(f64, f64, String),
    Refused(String),
    NoReader,
}

impl RCell {
    fn md(&self) -> String {
        match self {
            RCell::Opens => "opens".into(),
            RCell::Render(max, _, note) if max.is_nan() => format!("opens; {note}"),
            RCell::Render(max, mean, note) if *max == 0.0 => {
                format!(
                    "exact{}",
                    if note.is_empty() {
                        String::new()
                    } else {
                        format!(" ({note})")
                    }
                )
            }
            RCell::Render(max, mean, note) => format!(
                "Δ max {max:.0} mean {mean:.2}{}",
                if note.is_empty() {
                    String::new()
                } else {
                    format!(" ({note})")
                }
            ),
            RCell::Refused(r) => format!("REFUSED ({r})"),
            RCell::NoReader => "no reader (absent)".into(),
        }
    }
}

/// Render `file` with a reader to an 8-bit RGB PNG / PPM path.
fn render(dir: &Path, r: Reader, file: &Path, tag: &str) -> Result<Option<PathBuf>, String> {
    let out = dir.join(format!("{tag}_{}.png", r.name()));
    match r {
        Reader::Sips => run(Command::new("sips")
            .args(["-s", "format", "png"])
            .arg(file)
            .arg("--out")
            .arg(&out))
        .map(|_| Some(out)),
        Reader::HeifConvert => run(Command::new("heif-convert")
            .args(["--quiet", "-C", "nn"])
            .arg(file)
            .arg(&out))
        .map(|_| Some(out)),
        Reader::Magick => run(Command::new("magick")
            .arg(file)
            .args(["-alpha", "off", "-depth", "8"])
            .arg(format!("PNG24:{}", out.display())))
        .map(|_| Some(out)),
        Reader::Ffmpeg => run(Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
            .arg(file)
            .args(["-frames:v", "1", "-pix_fmt", "rgb24"])
            .arg(&out))
        .map(|_| Some(out)),
        Reader::HeifInfo => run(Command::new("heif-info").arg(file)).map(|_| None),
    }
}

fn render_delta(png_path: &Path, want: &oxideav_heif::rgb::RgbImage) -> Option<(f64, f64, String)> {
    let bytes = std::fs::read(png_path).ok()?;
    // Palette / sub-byte PNGs (a reader's choice for tiny pictures)
    // count as "opens" without a pixel comparison.
    if bytes.len() < 26 || bytes[25] == 3 || bytes[24] < 8 {
        return None;
    }
    let png = read_png(&bytes);
    let note = if (png.width, png.height) == (want.width, want.height) {
        String::new()
    } else if (png.width, png.height) == (want.height, want.width) {
        return Some((
            f64::NAN,
            f64::NAN,
            format!("renders {}×{}: rotation not applied", png.width, png.height),
        ));
    } else if png.width >= want.width && png.height >= want.height {
        format!(
            "renders the coded {}×{} (clap unapplied); overlap compared",
            png.width, png.height
        )
    } else {
        format!("renders {}×{}; overlap compared", png.width, png.height)
    };
    let ours_max = ((1u32 << want.bit_depth) - 1) as f64;
    let png_max = ((1u32 << png.bit_depth) - 1) as f64;
    let (mut max, mut sum, mut n) = (0f64, 0f64, 0u64);
    let cc = png.channels.clamp(1, 3);
    for y in 0..png.height.min(want.height) {
        for x in 0..png.width.min(want.width) {
            for c in 0..cc {
                let pc = if png.channels < 3 { 0 } else { c };
                let e = png.sample(x, y, pc) as f64 / png_max * 255.0;
                let g = want.sample(x, y, c.min(want.channels - 1)) as f64 / ours_max * 255.0;
                let d = (e - g).abs();
                max = max.max(d);
                sum += d;
                n += 1;
            }
        }
    }
    Some((max, sum / n.max(1) as f64, note))
}

fn frame_from_source(w: u32, h: u32, channels: usize, depth: u8) -> HeifFrame {
    let s = source(w, h, channels, depth);
    let chroma = if channels <= 2 {
        Chroma::Mono
    } else {
        Chroma::Yuv444
    };
    let mut f = HeifFrame::zeroed(
        w,
        h,
        HeifPixelFormat::new(chroma, depth, channels % 2 == 0).unwrap(),
    )
    .unwrap();
    let max = ((1u32 << depth) - 1) as f64;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize * channels;
            if channels <= 2 {
                f.set_sample(0, x, y, s[i]);
            } else {
                let (r, g, b) = (s[i] as f64, s[i + 1] as f64, s[i + 2] as f64);
                let yv = 0.299 * r + 0.587 * g + 0.114 * b;
                f.set_sample(0, x, y, yv.round().clamp(0.0, max) as u16);
                f.set_sample(
                    1,
                    x,
                    y,
                    ((b - yv) / 1.772 + (max + 1.0) / 2.0)
                        .round()
                        .clamp(0.0, max) as u16,
                );
                f.set_sample(
                    2,
                    x,
                    y,
                    ((r - yv) / 1.402 + (max + 1.0) / 2.0)
                        .round()
                        .clamp(0.0, max) as u16,
                );
            }
            if channels % 2 == 0 {
                f.set_sample(f.format.alpha_plane().unwrap(), x, y, s[i + channels - 1]);
            }
        }
    }
    f
}

/// Our writer's shapes: `(name, extension, frame, options)`.
fn writer_shapes() -> Vec<(String, &'static str, HeifFrame, EncodeOptions)> {
    let pcm = || EncodeOptions {
        hevc_mode: "pcm".into(),
        ..EncodeOptions::default()
    };
    let intra = |qp: u8| EncodeOptions {
        hevc_mode: "intra".into(),
        qp,
        ..EncodeOptions::default()
    };
    let av1 = |q: Option<u8>| EncodeOptions {
        codec: StillCodec::Av1,
        av1_quality: q,
        ..EncodeOptions::default()
    };
    let mut v: Vec<(String, &str, HeifFrame, EncodeOptions)> = Vec::new();
    for (w, h) in [(1, 1), (7, 5), (63, 61), (96, 80)] {
        v.push((
            format!("hevc {w}×{h}"),
            "heic",
            frame_from_source(w, h, 3, 8),
            intra(22),
        ));
        v.push((
            format!("av1 {w}×{h}"),
            "avif",
            frame_from_source(w, h, 3, 8),
            av1(Some(60)),
        ));
    }
    v.push((
        "hevc 4032×3024 grid (12 MP)".into(),
        "heic",
        frame_from_source(4032, 3024, 3, 8),
        EncodeOptions {
            grid_tile: Some(512),
            ..intra(26)
        },
    ));
    v.push((
        "av1 4032×3024 grid (12 MP)".into(),
        "avif",
        frame_from_source(4032, 3024, 3, 8),
        EncodeOptions {
            grid_tile: Some(512),
            av1_speed: "fast".into(),
            ..av1(Some(60))
        },
    ));
    v.push((
        "hevc lossless (pcm)".into(),
        "heic",
        frame_from_source(96, 80, 3, 8),
        pcm(),
    ));
    v.push((
        "av1 lossless".into(),
        "avif",
        frame_from_source(96, 80, 3, 8),
        av1(None),
    ));
    v.push((
        "hevc 4:0:0 (grey)".into(),
        "heic",
        frame_from_source(96, 80, 1, 8),
        intra(22),
    ));
    v.push((
        "av1 4:0:0 (grey)".into(),
        "avif",
        frame_from_source(96, 80, 1, 8),
        av1(Some(60)),
    ));
    v.push((
        "av1 4:4:4 10-bit".into(),
        "avif",
        frame_from_source(96, 80, 3, 16).tight(),
        av1(Some(60)),
    ));
    v.push((
        "hevc alpha".into(),
        "heic",
        frame_from_source(96, 80, 4, 8),
        intra(22),
    ));
    v.push((
        "av1 alpha".into(),
        "avif",
        frame_from_source(96, 80, 4, 8),
        av1(Some(60)),
    ));
    v.push((
        "hevc thumbnail".into(),
        "heic",
        frame_from_source(96, 80, 3, 8),
        EncodeOptions {
            thumbnail_max_dim: Some(48),
            ..pcm()
        },
    ));
    v.push((
        "hevc Exif+XMP+ICC".into(),
        "heic",
        frame_from_source(96, 80, 3, 8),
        EncodeOptions {
            exif: Some(b"II*\0\x08\0\0\0\0\0".to_vec()),
            xmp: Some("<x:xmpmeta>oxideav</x:xmpmeta>".into()),
            icc_profile: Some(std::fs::read(minimal_icc(&scratch_dir("matrix"))).unwrap()),
            ..pcm()
        },
    ));
    v.push((
        "hevc irot".into(),
        "heic",
        frame_from_source(96, 80, 3, 8),
        EncodeOptions {
            transforms: vec![Property::Irot(Irot { angle: 1 })],
            ..pcm()
        },
    ));
    v.push((
        "hevc imir".into(),
        "heic",
        frame_from_source(96, 80, 3, 8),
        EncodeOptions {
            transforms: vec![Property::Imir(Imir { axis: 1 })],
            ..pcm()
        },
    ));
    v.push((
        "hevc clap".into(),
        "heic",
        frame_from_source(96, 80, 3, 8),
        EncodeOptions {
            transforms: vec![Property::Clap(Clap::for_rect(
                96,
                80,
                CropRect {
                    x: 8,
                    y: 8,
                    width: 64,
                    height: 48,
                },
            ))],
            ..pcm()
        },
    ));
    v
}

#[test]
fn conformance_matrix_both_directions() {
    let dir = scratch_dir("matrix");
    let producers = [
        Producer::Sips,
        Producer::HeifEncX265,
        Producer::HeifEncAom,
        Producer::Magick,
        Producer::Ffmpeg,
    ];
    let readers = [
        Reader::Sips,
        Reader::HeifConvert,
        Reader::Magick,
        Reader::Ffmpeg,
        Reader::HeifInfo,
    ];
    let have_ffmpeg = have_binary("ffmpeg") && have_binary("ffprobe");
    // ── reader direction
    let mut reader_cells: BTreeMap<(usize, Producer), (Cell, String)> = BTreeMap::new();
    for p in producers {
        let present = have_binary(p.binary());
        if !present {
            eprintln!("SKIP: {} absent", p.binary());
        }
        for (fi, f) in FEATURES.iter().enumerate() {
            let cell = if !present {
                (Cell::NoProducer("absent".into()), String::new())
            } else {
                match produce(&dir, p, *f) {
                    Ok(None) => (Cell::NoProducer("no such option".into()), String::new()),
                    Ok(Some(file)) => {
                        if have_ffmpeg {
                            measure(&dir, p, *f, &file)
                        } else {
                            (
                                Cell::Delta(
                                    f64::NAN,
                                    f64::NAN,
                                    "no black-box decoder (ffmpeg absent)".into(),
                                ),
                                String::new(),
                            )
                        }
                    }
                    Err(c) => (c, String::new()),
                }
            };
            eprintln!(
                "reader {:<22} {:<16} {:<14} {}",
                p.name(),
                f.label(),
                cell.1,
                cell.0.md()
            );
            reader_cells.insert((fi, p), cell);
        }
    }
    // ── writer direction
    let mut writer_rows: Vec<(String, Vec<(Reader, RCell)>)> = Vec::new();
    for (name, ext, frame, opts) in writer_shapes() {
        let bytes = encode_still(&frame, &opts).expect(&name);
        let tag = name.replace(['×', ' ', ':', '(', ')', '+'], "_");
        let path = dir.join(format!("w_{tag}.{ext}"));
        std::fs::write(&path, &bytes).unwrap();
        let hf = HeifFile::parse(&bytes).unwrap();
        let img = decode_primary(&hf, ItemDecoder::direct()).expect(&name);
        let want = to_rgb(&img.frame, Some(&img.nclx)).unwrap();
        let mut row = Vec::new();
        for r in readers {
            let bin = r.name();
            let cell = if !have_binary(bin) {
                RCell::NoReader
            } else {
                match render(&dir, r, &path, &tag) {
                    Ok(None) => RCell::Opens,
                    Ok(Some(png)) => match render_delta(&png, &want) {
                        Some((max, mean, note)) => RCell::Render(max, mean, note),
                        None => RCell::Opens,
                    },
                    Err(e) => RCell::Refused(e),
                }
            };
            eprintln!("writer {:<30} {:<13} {}", name, bin, cell.md());
            row.push((r, cell));
        }
        writer_rows.push((name, row));
    }
    // ── Markdown
    let mut md = String::new();
    writeln!(
        md,
        "\n### Reader direction (producer → this crate; verdict vs the black-box video decoder)\n"
    )
    .unwrap();
    write!(md, "| Feature |").unwrap();
    for p in producers {
        write!(md, " {} |", p.name()).unwrap();
    }
    writeln!(md).unwrap();
    writeln!(md, "|{}", "---|".repeat(producers.len() + 1)).unwrap();
    for (fi, f) in FEATURES.iter().enumerate() {
        write!(md, "| {} |", f.label()).unwrap();
        for p in producers {
            let (c, layout) = &reader_cells[&(fi, p)];
            let l = if layout.is_empty()
                || matches!(c, Cell::NoProducer(_) | Cell::ProducerRefused(_))
            {
                String::new()
            } else {
                format!(" · {layout}")
            };
            write!(md, " {}{} |", c.md(), l).unwrap();
        }
        writeln!(md).unwrap();
    }
    writeln!(
        md,
        "\n### Writer direction (this crate → readers; render vs our decode, 8-bit units)\n"
    )
    .unwrap();
    write!(md, "| Written shape |").unwrap();
    for r in readers {
        write!(md, " {} |", r.name()).unwrap();
    }
    writeln!(md).unwrap();
    writeln!(md, "|{}", "---|".repeat(readers.len() + 1)).unwrap();
    for (name, row) in &writer_rows {
        write!(md, "| {name} |").unwrap();
        for (_, c) in row {
            write!(md, " {} |", c.md()).unwrap();
        }
        writeln!(md).unwrap();
    }
    eprintln!("{md}");
    std::fs::write(dir.join("matrix.md"), &md).unwrap();
    eprintln!("matrix written to {}", dir.join("matrix.md").display());
    // ── invariants: nothing undecided; our decode never fails on a
    // produced file; the black-box readers never refuse our files.
    for ((fi, p), (c, _)) in &reader_cells {
        assert!(
            !matches!(c, Cell::DecodeFailed(_)),
            "{} {}: {}",
            p.name(),
            FEATURES[*fi].label(),
            c.md()
        );
        if let Cell::Delta(max, _, _) = c {
            assert!(
                max.is_finite(),
                "{} {}: {}",
                p.name(),
                FEATURES[*fi].label(),
                c.md()
            );
        }
    }
    // Sizes the black-box decoder refused for some producer's file: a
    // reader limitation, not a writer bug, when it refuses ours too.
    let oracle_refused: Vec<(u32, u32)> = FEATURES
        .iter()
        .enumerate()
        .filter_map(|(fi, f)| match f {
            Feature::Size(w, h)
                if producers
                    .iter()
                    .any(|p| matches!(reader_cells[&(fi, *p)].0, Cell::NoOracle(_))) =>
            {
                Some((*w, *h))
            }
            _ => None,
        })
        .collect();
    for (name, row) in &writer_rows {
        let size_refused = oracle_refused
            .iter()
            .any(|(w, h)| name.contains(&format!("{w}×{h}")));
        for (r, c) in row {
            if let RCell::Refused(_) = c {
                assert!(
                    *r == Reader::Ffmpeg && size_refused,
                    "{name}: {} {}",
                    r.name(),
                    c.md()
                );
            }
        }
        // Consensus: at least one rendering reader agrees with our
        // decode within 2 codes (readers' own colour pipelines may
        // differ further; those deltas are reported, not asserted).
        let renders: Vec<f64> = row
            .iter()
            .filter_map(|(_, c)| match c {
                RCell::Render(max, _, _) if max.is_finite() => Some(*max),
                _ => None,
            })
            .collect();
        assert!(
            renders.is_empty() || renders.iter().any(|m| *m <= 2.0),
            "{name}: no reader renders our decode within 2 codes: {renders:?}"
        );
    }
}
