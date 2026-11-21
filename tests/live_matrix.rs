//! Live black-box matrix, both directions. **Reader direction**: files
//! written on the spot by the foreign producers on this machine —
//! `heif-enc` (HEVC and AV1), the platform tool `sips`, `magick`,
//! `ffmpeg` — over every option they expose that changes the file.
//! **Writer direction**: files written by every path of this crate's
//! writer, and its re-muxes of foreign files. Each file is decoded by
//! this crate through every public surface and held against two
//! black-box decoders:
//!
//! * **coded planes, bit-exact** — every stream `ffmpeg` exposes from
//!   the file (image items, tiles, auxiliaries, thumbnails, track
//!   samples; the reconstructed picture before the container's clean
//!   aperture and display matrix, in its native sample format) equals
//!   one of this crate's reconstructed pictures sample for sample;
//! * **rendered picture** — the primary image (every top-level image of
//!   a collection, every frame of a sequence) converted to RGB(A)
//!   equals `heif-dec`'s PNG within [`RGB_TOLERANCE`] 8-bit codes: the
//!   two YCbCr → RGB conversions round independently (H.273 leaves the
//!   rounding of the floating-point equations to the implementation;
//!   8.3 / 8.5 of that document give the equations, not an integer
//!   kernel). Alpha is compared exactly. Transformed pictures
//!   (`irot` / `imir`) are held against the black-box render of the
//!   same coded picture written without the property, transformed in
//!   RGB by the definition of HEIF 6.5.10 / 6.5.12 — the black-box
//!   decoder resamples chroma differently once it turns a picture, so
//!   its own render of the transformed file is not a sample-exact
//!   reference;
//! * **metadata** — Exif, XMP and ICC bytes, the `nclx` triple and
//!   range, bit depth, alpha / pre-multiplication, thumbnails, `clli`,
//!   `pasp`, sequence timing and repetitions equal what `heif-dec` /
//!   `heif-info` / `ffprobe` report;
//! * **surfaces** — `decode_primary`, the contract `decode` / `info` /
//!   `decode_all`, and the registry demuxer + `"heif"` decoder agree.
//! * **readers** (written files) — `heif-info`, `heif-dec` and `ffmpeg`
//!   take the file with nothing on their error streams and the
//!   platform reader opens it; what a reader cannot do with any
//!   producer's file of the shape is noted on the cell.
//!
//! Nothing is committed: the files live in a scratch directory. The
//! test SKIPS when `heif-dec` or `ffmpeg` is absent (both references
//! are needed for a verdict); a producer that is absent contributes no
//! cells. `OXIDEAV_HEIF_LIVE_BIG=1` adds the 12 MP / 48 MP / 16 k-class
//! cells and the platform's own system pictures;
//! `OXIDEAV_HEIF_LIVE_KEEP=1` keeps the scratch directory.
#![cfg(feature = "registry")]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::png::read_png;
use common::pngw::write_png;
use common::{have_binary, scratch_dir};
use oxideav_heif::decode::{ItemDecoder, decode_primary};
use oxideav_heif::derived::build_graph;
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};
use oxideav_heif::props::Colr;
use oxideav_heif::rgb::to_rgb;
use oxideav_heif::sequence::{LoopBehaviour, parse_movie};
use oxideav_heif::{DecodedImage, HeifFile};

/// Largest difference, in 8-bit codes, between this crate's RGB
/// rendering and the black-box decoder's for a cell to pass.
const RGB_TOLERANCE: u32 = 2;

/// The same for an overlay (`iovl`): this crate composes the canvas in
/// the output's 8-bit YCbCr, the black-box decoder in RGB, so the sRGB
/// `canvas_fill_value` (HEIF 6.6.2.2) and the pixels blended over it
/// pass through one more quantisation here (measured: up to 4 codes on
/// lossy layers, 3 on lossless ones; the layers themselves match).
const OVERLAY_TOLERANCE: u32 = 4;

// ───────────────────────── sources ─────────────────────────

/// Deterministic picture with detail at every scale: two gradients, a
/// flat box, a disc and per-pixel noise in each channel (so a chroma
/// sample that lands one position off shows), `channels` 1 (grey),
/// 2 (grey + alpha), 3 or 4; alpha runs opaque → transparent with
/// noise. `depth` 8 or 16.
fn source(w: u32, h: u32, channels: usize, depth: u8, seed: u32) -> Vec<u16> {
    let max = ((1u32 << depth) - 1) as f64;
    let (cx, cy, r) = (w as f64 * 0.7, h as f64 * 0.65, w.min(h) as f64 * 0.18);
    let mut state = 0x9E37_79B9u32 ^ seed.wrapping_mul(0x85EB_CA6B);
    let mut noise = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        (state >> 8) as f64 / (1u32 << 24) as f64
    };
    let mut v = Vec::with_capacity(w as usize * h as usize * channels);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f64 / w.max(2) as f64, y as f64 / h.max(2) as f64);
            let mut rgb = [fx, fy, 1.0 - (fx + fy) * 0.5];
            if x >= w / 4 && x < w / 2 && y >= h / 4 && y < h / 2 {
                rgb = [0.94, 0.94, 0.94];
            }
            let (dx, dy) = (x as f64 - cx, y as f64 - cy);
            if dx * dx + dy * dy < r * r {
                rgb = [0.9, 0.12, 0.12];
            }
            for c in &mut rgb {
                *c = (*c * 0.7 + noise() * 0.3).clamp(0.0, 1.0);
            }
            let grey = 0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2];
            let a = if x < w / 3 {
                1.0
            } else if x > w - w / 6 {
                0.0
            } else {
                (fy * 0.8 + noise() * 0.2).clamp(0.0, 1.0)
            };
            let q = |c: f64| (c * max).round() as u16;
            match channels {
                1 => v.push(q(grey)),
                2 => v.extend([q(grey), q(a)]),
                3 => v.extend(rgb.map(q)),
                _ => {
                    v.extend(rgb.map(q));
                    v.push(q(a));
                }
            }
        }
    }
    v
}

/// The sources of the matrix, written once: `rgb96x80`, `rgba97x81`,
/// `g96x80`, `ga96x80` (suffix `d16` for 16-bit), `seq1` … `seq5`
/// (64×48 frames), and — made by the platform tool, when present —
/// `meta.jpg` / `meta_p3.jpg` / `meta.tiff` / `meta.png` (Exif, XMP
/// and an ICC profile).
struct Sources {
    dir: PathBuf,
    made: Mutex<BTreeMap<String, Option<PathBuf>>>,
}

impl Sources {
    fn get(&self, key: &str) -> Option<PathBuf> {
        let mut made = self.made.lock().unwrap();
        if let Some(p) = made.get(key) {
            return p.clone();
        }
        let p = self.make(key);
        made.insert(key.to_string(), p.clone());
        p
    }

    fn make(&self, key: &str) -> Option<PathBuf> {
        if let Some(n) = key.strip_prefix("seq") {
            let seed: u32 = n.parse().ok()?;
            let p = self.dir.join(format!("{key}.png"));
            std::fs::write(&p, write_png(64, 48, 3, 8, &source(64, 48, 3, 8, seed))).ok()?;
            return Some(p);
        }
        if key.starts_with("meta") || key.starts_with("photo") {
            return self.make_platform(key);
        }
        let (kind, rest) = key.split_at(key.find(|c: char| c.is_ascii_digit())?);
        let (dims, depth) = match rest.strip_suffix("d16") {
            Some(d) => (d, 16),
            None => (rest, 8),
        };
        let (w, h) = dims.split_once('x')?;
        let (w, h): (u32, u32) = (w.parse().ok()?, h.parse().ok()?);
        let channels = match kind {
            "g" => 1,
            "ga" => 2,
            "rgb" => 3,
            "rgba" => 4,
            _ => return None,
        };
        let p = self.dir.join(format!("{key}.png"));
        std::fs::write(
            &p,
            write_png(w, h, channels, depth, &source(w, h, channels, depth, 7)),
        )
        .ok()?;
        Some(p)
    }

    /// Photo-like sources with metadata, converted by the platform
    /// tool from a PNG: a JPEG with TIFF / Exif tags, the same with a
    /// wide-gamut ICC profile embedded, and TIFF / PNG copies of it;
    /// `photo<W>x<H>` is a plain JPEG of that size.
    fn make_platform(&self, key: &str) -> Option<PathBuf> {
        if !have_binary_cached("sips") {
            return None;
        }
        let sips = |args: &[&str], src: &Path, out: &Path| {
            Command::new("sips")
                .args(args)
                .arg(src)
                .arg("--out")
                .arg(out)
                .output()
                .ok()
                .filter(|o| o.status.success() && out.exists())
                .map(|_| out.to_path_buf())
        };
        if let Some(dims) = key.strip_prefix("photo") {
            let png = self.make(&format!("rgb{dims}"))?;
            return sips(
                &["-s", "format", "jpeg", "-s", "formatOptions", "90"],
                &png,
                &self.dir.join(format!("{key}.jpg")),
            );
        }
        let base = self.dir.join("meta.jpg");
        if !base.exists() {
            let png = self.make("rgb640x480")?;
            sips(
                &[
                    "-s",
                    "format",
                    "jpeg",
                    "-s",
                    "formatOptions",
                    "92",
                    "-s",
                    "make",
                    "OxideCam",
                    "-s",
                    "model",
                    "LiveMatrix",
                    "-s",
                    "copyright",
                    "(c) test",
                    "-s",
                    "description",
                    "matrix source",
                    "-s",
                    "artist",
                    "nobody",
                ],
                &png,
                &base,
            )?;
        }
        let p3 = Path::new("/System/Library/ColorSync/Profiles/Display P3.icc");
        let with_icc = self.dir.join("meta_p3.jpg");
        if key != "meta.jpg" && !with_icc.exists() {
            if !p3.exists() {
                return None;
            }
            sips(&["-e", p3.to_str()?], &base, &with_icc)?;
        }
        match key {
            "meta.jpg" => Some(base),
            "meta_p3.jpg" => Some(with_icc),
            "meta.tiff" => sips(&["-s", "format", "tiff"], &with_icc, &self.dir.join(key)),
            "meta.png" => sips(&["-s", "format", "png"], &with_icc, &self.dir.join(key)),
            _ => None,
        }
    }
}

fn have_binary_cached(bin: &'static str) -> bool {
    static CACHE: Mutex<BTreeMap<&'static str, bool>> = Mutex::new(BTreeMap::new());
    *CACHE
        .lock()
        .unwrap()
        .entry(bin)
        .or_insert_with(|| have_binary(bin))
}

// ───────────────────────── cells ─────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Producer {
    HeifEnc,
    Sips,
    Magick,
    Ffmpeg,
    /// A file already on the machine (the platform's own pictures).
    Existing,
    /// This crate's writer (the other direction of the matrix).
    Ours,
}

impl Producer {
    fn binary(self) -> Option<&'static str> {
        match self {
            Producer::HeifEnc => Some("heif-enc"),
            Producer::Sips => Some("sips"),
            Producer::Magick => Some("magick"),
            Producer::Ffmpeg => Some("ffmpeg"),
            Producer::Existing | Producer::Ours => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// One primary image (plus whatever hangs off it).
    Still,
    /// Several top-level images.
    Collection,
    /// An image sequence / video track (possibly beside an item).
    Sequence,
}

/// A transformation the producer signals as a property (not baked into
/// the pixels), in the order the file applies them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    /// Rotate clockwise by this many quarter turns.
    RotateCw(u8),
    /// Exchange left and right.
    MirrorH,
    /// Exchange top and bottom.
    MirrorV,
    /// Keep the `w × h` rectangle at `(x, y)`.
    Crop(u32, u32, u32, u32),
}

/// Writes a cell's file with this crate; the argument is the foreign
/// file a re-mux reads.
type Make = std::sync::Arc<dyn Fn(Option<&Path>) -> Result<Vec<u8>, String> + Send + Sync>;

#[derive(Clone)]
struct Cell {
    tag: String,
    producer: Producer,
    /// Arguments; `@0`, `@1`, … stand for the source paths.
    args: Vec<String>,
    sources: Vec<String>,
    ext: &'static str,
    kind: Kind,
    /// Tag of the cell coding the same picture without `ops`.
    twin: Option<String>,
    ops: Vec<Op>,
    /// How this crate writes the file ([`Producer::Ours`]).
    make: Option<Make>,
    /// Tag of the foreign cell whose file `make` reads.
    input: Option<String>,
}

fn cell(producer: Producer, tag: &str, ext: &'static str, args: &str, sources: &[&str]) -> Cell {
    Cell {
        tag: tag.to_string(),
        producer,
        args: args.split_whitespace().map(str::to_string).collect(),
        sources: sources.iter().map(|s| s.to_string()).collect(),
        ext,
        kind: Kind::Still,
        twin: None,
        ops: Vec::new(),
        make: None,
        input: None,
    }
}

impl Cell {
    fn kind(mut self, kind: Kind) -> Self {
        self.kind = kind;
        self
    }
    fn transformed(mut self, twin: &str, ops: &[Op]) -> Self {
        self.twin = Some(twin.to_string());
        self.ops = ops.to_vec();
        self
    }
}

const SEQ: [&str; 5] = ["seq1", "seq2", "seq3", "seq4", "seq5"];

/// Every option `heif-enc` exposes that changes the file, for both of
/// its codecs.
fn heif_enc_cells(out: &mut Vec<Cell>) {
    use Producer::HeifEnc as P;
    for (c, flag, ext, sext) in [
        ("hevc", "--hevc", "heic", "heics"),
        ("av1", "--avif", "avif", "avifs"),
    ] {
        let mut add = |tag: &str, args: &str, sources: &[&str]| {
            out.push(cell(
                P,
                &format!("he_{c}_{tag}"),
                ext,
                &format!("{flag} {args}"),
                sources,
            ));
        };
        // Quality extremes and lossless.
        for q in [0, 1, 25, 50, 75, 100] {
            add(&format!("q{q}"), &format!("-q {q}"), &["rgb96x80"]);
        }
        add("lossless", "-L", &["rgb96x80"]);
        add("lossless_odd", "-L", &["rgb97x81"]);
        add("lossless_alpha", "-L", &["rgba97x81"]);
        add("lossless_grey", "-L", &["g96x80"]);
        add("lossless_b10", "-L -b 10", &["rgb96x80d16"]);
        add("lossless_b12", "-L -b 12", &["rgb96x80d16"]);
        // Chroma format × bit depth × even / odd size, alpha, the
        // codec's own lossless mode per chroma format.
        for ch in [420, 422, 444] {
            for b in [8, 10, 12] {
                let (src, bits) = if b == 8 {
                    ("rgb96x80", String::new())
                } else {
                    ("rgb96x80d16", format!("-b {b}"))
                };
                add(
                    &format!("c{ch}_b{b}"),
                    &format!("-q 60 -p chroma={ch} {bits}"),
                    &[src],
                );
                let (src, bits) = if b == 8 {
                    ("rgb97x81", String::new())
                } else {
                    ("rgb97x81d16", format!("-b {b}"))
                };
                add(
                    &format!("c{ch}_b{b}_odd"),
                    &format!("-q 60 -p chroma={ch} {bits}"),
                    &[src],
                );
            }
            add(
                &format!("c{ch}_alpha"),
                &format!("-q 60 -p chroma={ch}"),
                &["rgba97x81"],
            );
            add(
                &format!("c{ch}_codec_lossless"),
                &format!("-p lossless=true -p chroma={ch}"),
                &["rgb97x81"],
            );
        }
        // Monochrome.
        add("grey_b8", "-q 60", &["g96x80"]);
        add("grey_b10", "-q 60 -b 10", &["g96x80d16"]);
        add("grey_b12", "-q 60 -b 12", &["g96x80d16"]);
        add("grey_alpha", "-q 60", &["ga96x80"]);
        // Alpha.
        add("alpha", "-q 60", &["rgba96x80"]);
        add("alpha_b10", "-q 60 -b 10", &["rgba96x80d16"]);
        add("alpha_b12", "-q 60 -b 12", &["rgba96x80d16"]);
        add(
            "alpha_premultiplied",
            "-q 60 --premultiplied-alpha",
            &["rgba96x80"],
        );
        add("alpha_dropped", "-q 60 --no-alpha", &["rgba96x80"]);
        // Thumbnails.
        add("thumb", "-q 60 -t 32", &["rgb513x385"]);
        add("thumb_alpha", "-q 60 -t 32", &["rgba97x81"]);
        add(
            "thumb_no_alpha",
            "-q 60 -t 32 --no-thumb-alpha",
            &["rgba97x81"],
        );
        // Sizes: 1×1, tiny, one-sample-wide, odd, mid.
        for s in [
            "1x1", "2x2", "7x5", "1x33", "65x1", "97x81", "513x385", "1280x720",
        ] {
            add(&format!("size_{s}"), "-q 60", &[&format!("rgb{s}")]);
        }
        for s in ["1x1", "2x2", "7x5", "1x33", "65x1"] {
            add(
                &format!("size_{s}_444"),
                "-q 60 -p chroma=444",
                &[&format!("rgb{s}")],
            );
        }
        // Orientation: every rotation, both mirrors, combinations,
        // on an odd 4:2:0 picture (the twin is `size_97x81`), on an
        // even one and on 4:4:4.
        let orientations: [(&str, &str, &[Op]); 8] = [
            ("rot90", "--rotate-cw 90", &[Op::RotateCw(1)]),
            ("rot180", "--rotate-cw 180", &[Op::RotateCw(2)]),
            ("rot270", "--rotate-cw 270", &[Op::RotateCw(3)]),
            ("mirror_h", "--flip-h", &[Op::MirrorH]),
            ("mirror_v", "--flip-v", &[Op::MirrorV]),
            (
                "rot90_mirror_h",
                "--rotate-cw 90 --flip-h",
                &[Op::RotateCw(1), Op::MirrorH],
            ),
            (
                "rot270_mirror_v",
                "--rotate-cw 270 --flip-v",
                &[Op::RotateCw(3), Op::MirrorV],
            ),
            (
                "rot180_mirror_h",
                "--rotate-cw 180 --flip-h",
                &[Op::RotateCw(2), Op::MirrorH],
            ),
        ];
        add("even", "-q 60", &["rgb96x80"]);
        add("odd_444", "-q 60 -p chroma=444", &["rgb97x81"]);
        add("odd_422", "-q 60 -p chroma=422", &["rgb97x81"]);
        add("odd_alpha", "-q 60", &["rgba97x81"]);
        add("grid64", "-q 60 --cut-tiles 64", &["rgb513x385"]);
        for (name, args, ops) in orientations {
            for (variant, twin, extra, src) in [
                ("odd", "size_97x81", "", "rgb97x81"),
                ("even", "even", "", "rgb96x80"),
                ("odd_444", "odd_444", "-p chroma=444", "rgb97x81"),
                ("odd_422", "odd_422", "-p chroma=422", "rgb97x81"),
            ] {
                out.push(
                    cell(
                        P,
                        &format!("he_{c}_{name}_{variant}"),
                        ext,
                        &format!("{flag} -q 60 {extra} {args}"),
                        &[src],
                    )
                    .transformed(&format!("he_{c}_{twin}"), ops),
                );
            }
        }
        out.push(
            cell(
                P,
                &format!("he_{c}_rot90_alpha"),
                ext,
                &format!("{flag} -q 60 --rotate-cw 90"),
                &["rgba97x81"],
            )
            .transformed(&format!("he_{c}_odd_alpha"), &[Op::RotateCw(1)]),
        );
        out.push(
            cell(
                P,
                &format!("he_{c}_grid64_rot90"),
                ext,
                &format!("{flag} -q 60 --cut-tiles 64 --rotate-cw 90"),
                &["rgb513x385"],
            )
            .transformed(&format!("he_{c}_grid64"), &[Op::RotateCw(1)]),
        );
        out.push(
            cell(
                P,
                &format!("he_{c}_grid64_mirror_h"),
                ext,
                &format!("{flag} -q 60 --cut-tiles 64 --flip-h"),
                &["rgb513x385"],
            )
            .transformed(&format!("he_{c}_grid64"), &[Op::MirrorH]),
        );
        let mut add = |tag: &str, args: &str, sources: &[&str]| {
            out.push(cell(
                P,
                &format!("he_{c}_{tag}"),
                ext,
                &format!("{flag} {args}"),
                sources,
            ));
        };
        // Descriptive properties.
        add("clli", "-q 60 --clli 1000,400", &["rgb96x80"]);
        add("pasp", "-q 60 --pasp 4,3", &["rgb96x80"]);
        // Colour: presets, matrices, range, both colr boxes.
        for cp in ["auto", "compatible", "601", "709", "2020"] {
            add(
                &format!("profile_{cp}"),
                &format!("-q 60 --color-profile {cp}"),
                &["rgb96x80"],
            );
        }
        add(
            "profile_2020_b10",
            "-q 60 --color-profile 2020 -b 10",
            &["rgb96x80d16"],
        );
        for mc in [1, 2, 5, 6, 9] {
            add(
                &format!("matrix{mc}"),
                &format!("-q 60 --matrix_coefficients {mc}"),
                &["rgb96x80"],
            );
        }
        add(
            "matrix0_444",
            "-q 60 --matrix_coefficients 0 -p chroma=444",
            &["rgb96x80"],
        );
        add(
            "matrix9_pq_b10",
            "-q 60 --matrix_coefficients 9 --colour_primaries 9 --transfer_characteristic 16 -b 10",
            &["rgb96x80d16"],
        );
        add("limited", "-q 60 --full_range_flag 0", &["rgb96x80"]);
        add(
            "limited_b10",
            "-q 60 --full_range_flag 0 -b 10",
            &["rgb96x80d16"],
        );
        add(
            "limited_444",
            "-q 60 --full_range_flag 0 -p chroma=444",
            &["rgb96x80"],
        );
        add("limited_grey", "-q 60 --full_range_flag 0", &["g96x80"]);
        // Grids.
        for t in [128, 256] {
            add(
                &format!("grid{t}"),
                &format!("-q 60 --cut-tiles {t}"),
                &["rgb513x385"],
            );
        }
        add("grid64_alpha", "-q 60 --cut-tiles 64", &["rgba97x81"]);
        add(
            "grid64_thumb",
            "-q 60 --cut-tiles 64 -t 32",
            &["rgb513x385"],
        );
        add("grid64_b10", "-q 60 --cut-tiles 64 -b 10", &["rgb96x80d16"]);
        add(
            "grid64_444",
            "-q 60 --cut-tiles 64 -p chroma=444",
            &["rgb97x81"],
        );
        add("grid64_grey", "-q 60 --cut-tiles 64", &["g96x80"]);
        add("grid256_720p", "-q 60 --cut-tiles 256", &["rgb1280x720"]);
        // Brands and structure.
        add("unified_ids", "-q 60 --unif", &["rgb96x80"]);
        add(
            "extra_brand",
            "-q 60 --add-compatible-brand abcd",
            &["rgb96x80"],
        );
        add(
            "description",
            "-q 60 --pitm-description hello",
            &["rgb96x80"],
        );
        add("low_overhead", "-q 60 --mini", &["rgb96x80"]);
        add("low_overhead_alpha", "-q 60 --mini", &["rgba97x81"]);
        // Sources carrying metadata: an ICC profile with Exif and XMP
        // (PNG), Exif and a profile (JPEG), TIFF tags; both colour
        // descriptions at once; the omnidirectional projection
        // property.
        add("from_png_icc", "-q 60", &["meta.png"]);
        add("from_jpeg_icc", "-q 60", &["meta_p3.jpg"]);
        add("from_tiff", "-q 60", &["meta.tiff"]);
        add("two_colr", "-q 60 --enable-two-colr-boxes", &["meta.png"]);
        add(
            "two_colr_p3",
            "-q 60 --enable-two-colr-boxes --matrix_coefficients 1 --colour_primaries 12 \
             --transfer_characteristic 13",
            &["meta.png"],
        );
        add(
            "projection_equirectangular",
            "-q 60 --omaf-image-projection equirectangular",
            &["rgb96x80"],
        );
        add(
            "projection_cube_map",
            "-q 60 --omaf-image-projection cube-map",
            &["rgb96x80"],
        );
        // Chroma downsampling of the producer (changes the samples).
        for ds in ["nn", "average", "sharp-yuv"] {
            add(
                &format!("downsample_{ds}"),
                &format!("-q 60 -C {ds}"),
                &["rgb97x81"],
            );
        }
        out.push(
            cell(
                P,
                &format!("he_{c}_collection"),
                ext,
                &format!("{flag} -q 60"),
                &["rgb96x80", "rgba97x81", "g96x80"],
            )
            .kind(Kind::Collection),
        );
        // Sequences.
        let mut seq = |tag: &str, args: &str, sources: &[&str], ext: &'static str| {
            out.push(
                cell(
                    P,
                    &format!("he_{c}_{tag}"),
                    ext,
                    &format!("{flag} {args}"),
                    sources,
                )
                .kind(Kind::Sequence),
            );
        };
        seq("seq", "-q 60 -S", &SEQ, sext);
        seq("seq_fps25", "-q 60 -S --fps 25", &SEQ, sext);
        seq(
            "seq_repeat3",
            "-q 60 -S --repetitions 3 --timebase 90000 --duration 3000",
            &SEQ,
            sext,
        );
        seq("seq_forever", "-q 60 -S --repetitions infinite", &SEQ, sext);
        for g in ["intra-only", "low-delay", "unrestricted"] {
            seq(
                &format!("seq_gop_{g}"),
                &format!("-q 60 -S --gop-structure {g}"),
                &SEQ,
                sext,
            );
        }
        seq("seq_max3", "-q 60 -S --max-frames 3", &SEQ, sext);
        seq(
            "seq_keyframes",
            "-q 60 -S --min-keyframe-distance 2 --max-keyframe-distance 2",
            &SEQ,
            sext,
        );
        seq("seq_alpha", "-q 60 -S", &["rgba96x80", "rgba96x80"], sext);
        seq(
            "seq_b10",
            "-q 60 -S -b 10",
            &["rgb96x80d16", "rgb96x80d16"],
            sext,
        );
        seq("seq_444", "-q 60 -S -p chroma=444", &SEQ, sext);
        seq("seq_lossless", "-L -S", &SEQ, sext);
        seq("video_track", "-q 60 -S -V", &SEQ, "mp4");
    }
    // Encoder parameters that change the bitstream's tools.
    let mut add = |tag: &str, args: &str, src: &str, ext: &'static str| {
        out.push(cell(P, &format!("he_{tag}"), ext, args, &[src]));
    };
    for ctu in [16, 32, 64] {
        add(
            &format!("hevc_ctu{ctu}"),
            &format!("--hevc -q 60 -p x265:ctu={ctu}"),
            "rgb513x385",
            "heic",
        );
    }
    for preset in ["ultrafast", "medium", "slower"] {
        add(
            &format!("hevc_preset_{preset}"),
            &format!("--hevc -q 60 -p preset={preset}"),
            "rgb513x385",
            "heic",
        );
    }
    for tune in ["psnr", "ssim", "grain", "fastdecode"] {
        add(
            &format!("hevc_tune_{tune}"),
            &format!("--hevc -q 60 -p tune={tune}"),
            "rgb513x385",
            "heic",
        );
    }
    for (tag, params) in [
        ("wpp_slices", "-p x265:wpp=1 -p x265:slices=4"),
        ("no_loop_filters", "-p x265:sao=0 -p x265:deblock=0"),
        (
            "amp_rect_tu4",
            "-p x265:amp=1 -p x265:rect=1 -p tu-intra-depth=4",
        ),
        ("tskip_cu_lossless", "-p x265:tskip=1 -p x265:cu-lossless=1"),
        ("scaling_list", "-p x265:scaling-list=default"),
        (
            "constrained_intra",
            "-p x265:constrained-intra=1 -p x265:strong-intra-smoothing=0",
        ),
    ] {
        add(
            &format!("hevc_{tag}"),
            &format!("--hevc -q 60 {params}"),
            "rgb513x385",
            "heic",
        );
    }
    for speed in [0, 4, 6, 9] {
        add(
            &format!("av1_speed{speed}"),
            &format!("--avif -q 60 -p speed={speed}"),
            "rgb513x385",
            "avif",
        );
    }
    for (tag, params, src) in [
        (
            "tiles",
            "-p aom:tile-columns=2 -p aom:tile-rows=1",
            "rgb1280x720",
        ),
        ("auto_tiles", "-p auto-tiles=true", "rgb1280x720"),
        (
            "no_cdef_restoration",
            "-p aom:enable-cdef=0 -p aom:enable-restoration=0",
            "rgb513x385",
        ),
        ("screen_content", "-p aom:tune-content=screen", "rgb513x385"),
        ("film_grain", "-p aom:film-grain-test=1", "rgb513x385"),
        ("sb128", "-p aom:sb-size=128", "rgb513x385"),
        ("no_intrabc", "-p enable-intrabc=false", "rgb513x385"),
        ("realtime", "-p realtime=true", "rgb513x385"),
        ("tune_psnr", "-p tune=psnr", "rgb513x385"),
        ("tune_ssim", "-p tune=ssim", "rgb513x385"),
        ("tune_iq", "-p tune=iq", "rgb513x385"),
        ("q_window", "-p min-q=10 -p max-q=20", "rgb513x385"),
        ("alpha_quality", "-p alpha-quality=10", "rgba97x81"),
        ("alpha_lossless", "-p lossless-alpha=true", "rgba97x81"),
    ] {
        add(
            &format!("av1_{tag}"),
            &format!("--avif -q 60 {params}"),
            src,
            "avif",
        );
    }
}

/// The platform tool: both formats it writes, its quality scale,
/// sizes, sample layouts it accepts, metadata-bearing JPEG / TIFF /
/// PNG sources, every rotation / mirror / crop / pad it offers.
fn sips_cells(out: &mut Vec<Cell>) {
    use Producer::Sips as P;
    for f in ["heic", "avif"] {
        let fmt = format!("-s format {f}");
        let mut add = |tag: &str, args: &str, src: &str| {
            out.push(cell(
                P,
                &format!("sips_{f}_{tag}"),
                if f == "heic" { "heic" } else { "avif" },
                &format!("{fmt} {args}"),
                &[src],
            ));
        };
        for q in [
            "0", "10", "50", "90", "100", "best", "low", "normal", "high",
        ] {
            add(
                &format!("q{q}"),
                &format!("-s formatOptions {q}"),
                "rgb96x80",
            );
        }
        for s in [
            "1x1", "2x2", "7x5", "1x33", "65x1", "97x81", "513x385", "1280x720",
        ] {
            add(&format!("size_{s}"), "", &format!("rgb{s}"));
        }
        add("grey", "", "g96x80");
        add("grey_16bit", "", "g96x80d16");
        add("alpha", "", "rgba97x81");
        add("grey_alpha", "", "ga96x80");
        add("16bit", "", "rgb96x80d16");
        add("alpha_16bit", "", "rgba96x80d16");
        add("from_jpeg", "", "meta.jpg");
        add("from_jpeg_icc", "", "meta_p3.jpg");
        add("from_tiff", "", "meta.tiff");
        add("from_png", "", "meta.png");
        for r in [90, 180, 270] {
            add(&format!("rot{r}"), &format!("-r {r}"), "meta.jpg");
        }
        add("mirror_h", "-f horizontal", "meta.jpg");
        add("mirror_v", "-f vertical", "meta.jpg");
        add("crop", "-c 101 203", "meta.jpg");
        add("pad", "-p 600 800", "meta.jpg");
        add("resample", "-Z 333", "meta.jpg");
    }
    out.push(cell(P, "sips_heics", "heics", "-s format heics", &["rgb96x80"]).kind(Kind::Sequence));
}

fn magick_cells(out: &mut Vec<Cell>) {
    use Producer::Magick as P;
    for f in ["heic", "avif"] {
        let ext = if f == "heic" { "heic" } else { "avif" };
        let mut add = |tag: &str, args: &str, sources: &[&str]| {
            out.push(cell(P, &format!("magick_{f}_{tag}"), ext, args, sources));
        };
        for q in [1, 50, 100] {
            add(&format!("q{q}"), &format!("-quality {q}"), &["rgb97x81"]);
        }
        add("lossless", "-define heic:lossless=true", &["rgb97x81"]);
        for ch in [420, 422, 444] {
            add(
                &format!("c{ch}"),
                &format!("-define heic:chroma={ch}"),
                &["rgb97x81"],
            );
        }
        add("depth10", "-depth 10", &["rgb96x80d16"]);
        add("depth12", "-depth 12", &["rgb96x80d16"]);
        add("alpha", "", &["rgba97x81"]);
        add("grey", "", &["g96x80"]);
        add("exif", "", &["meta.jpg"]);
        add("icc", "", &["meta_p3.jpg"]);
        add("exif_orientation", "-orient right-top", &["meta.jpg"]);
        add("size_1x1", "", &["rgb1x1"]);
        add("size_513x385", "", &["rgb513x385"]);
    }
    out.push(
        cell(
            P,
            "magick_heic_collection",
            "heic",
            "",
            &["rgb96x80", "rgb97x81"],
        )
        .kind(Kind::Collection),
    );
}

fn ffmpeg_cells(out: &mut Vec<Cell>) {
    use Producer::Ffmpeg as P;
    let mut add = |tag: &str, args: &str, src: &str, kind: Kind| {
        out.push(cell(P, &format!("ffmpeg_av1_{tag}"), "avif", args, &[src]).kind(kind));
    };
    let still = "-frames:v 1 -c:v libsvtav1";
    add(
        "420",
        &format!("-i @0 {still} -pix_fmt yuv420p"),
        "rgb96x80",
        Kind::Still,
    );
    add(
        "420_b10",
        &format!("-i @0 {still} -pix_fmt yuv420p10le"),
        "rgb96x80d16",
        Kind::Still,
    );
    add(
        "odd",
        &format!("-i @0 {still} -pix_fmt yuv420p"),
        "rgb97x81",
        Kind::Still,
    );
    add(
        "513x385",
        &format!("-i @0 {still} -pix_fmt yuv420p"),
        "rgb513x385",
        Kind::Still,
    );
    add(
        "tiles",
        &format!("-i @0 {still} -svtav1-params tile-columns=1:tile-rows=1 -pix_fmt yuv420p"),
        "rgb1280x720",
        Kind::Still,
    );
    for crf in [10, 60] {
        add(
            &format!("crf{crf}"),
            &format!("-i @0 {still} -crf {crf} -pix_fmt yuv420p"),
            "rgb513x385",
            Kind::Still,
        );
    }
    add(
        "preset2",
        &format!("-i @0 {still} -preset 2 -pix_fmt yuv420p"),
        "rgb513x385",
        Kind::Still,
    );
    add(
        "limited_709",
        &format!(
            "-i @0 {still} -pix_fmt yuv420p -color_range tv -colorspace bt709 \
             -color_primaries bt709 -color_trc bt709"
        ),
        "rgb96x80",
        Kind::Still,
    );
    let seq = "-framerate 10 -i @seq -c:v libsvtav1";
    add(
        "seq",
        &format!("{seq} -pix_fmt yuv420p"),
        "seq1",
        Kind::Sequence,
    );
    add(
        "seq_loop",
        &format!("{seq} -pix_fmt yuv420p -loop 0"),
        "seq1",
        Kind::Sequence,
    );
    add(
        "seq_b10",
        &format!("{seq} -pix_fmt yuv420p10le"),
        "seq1",
        Kind::Sequence,
    );
    // The track header's matrix: every quarter turn, both mirrors,
    // and a mirror with a turn (the input's display matrix is passed
    // through to the muxer instead of being applied to the pictures).
    for (tag, display) in [
        ("rot90", "-display_rotation 90"),
        ("rot180", "-display_rotation 180"),
        ("rot270", "-display_rotation 270"),
        ("mirror_h", "-display_hflip"),
        ("mirror_v", "-display_vflip"),
        ("mirror_h_rot90", "-display_hflip -display_rotation 90"),
        ("mirror_v_rot90", "-display_vflip -display_rotation 90"),
    ] {
        add(
            &format!("seq_matrix_{tag}"),
            &format!("-noautorotate {display} {seq} -pix_fmt yuv420p"),
            "seq1",
            Kind::Sequence,
        );
    }
}

/// 12 MP / 48 MP / 16 k-class pictures and the platform's own files.
fn big_cells(out: &mut Vec<Cell>) {
    for f in ["heic", "avif"] {
        out.push(cell(
            Producer::Sips,
            &format!("sips_{f}_12mp"),
            if f == "heic" { "heic" } else { "avif" },
            &format!("-s format {f}"),
            &["photo4032x3024"],
        ));
    }
    out.push(cell(
        Producer::Sips,
        "sips_heic_48mp",
        "heic",
        "-s format heic",
        &["photo8064x6048"],
    ));
    out.push(cell(
        Producer::Sips,
        "sips_heic_16k",
        "heic",
        "-s format heic",
        &["photo16384x2048"],
    ));
    out.push(cell(
        Producer::HeifEnc,
        "he_hevc_12mp",
        "heic",
        "--hevc -q 50 -p preset=ultrafast",
        &["photo4032x3024"],
    ));
    out.push(cell(
        Producer::HeifEnc,
        "he_av1_12mp",
        "avif",
        "--avif -q 50 -p speed=9",
        &["photo4032x3024"],
    ));
    for (tag, path) in [
        (
            "default_desktop",
            "/System/Library/CoreServices/DefaultDesktop.heic",
        ),
        ("mac_blue", "/System/Library/Desktop Pictures/Mac Blue.heic"),
        ("sonoma", "/System/Library/Desktop Pictures/Sonoma.heic"),
        (
            "thumb_tree",
            "/System/Library/Desktop Pictures/.thumbnails/Tree.heic",
        ),
        (
            "thumb_valley",
            "/System/Library/Desktop Pictures/.thumbnails/Valley Light.heic",
        ),
    ] {
        if Path::new(path).exists() {
            let mut c = cell(
                Producer::Existing,
                &format!("platform_{tag}"),
                "heic",
                "",
                &[],
            );
            c.args = vec![path.to_string()];
            out.push(c);
        }
    }
}

fn cells() -> Vec<Cell> {
    let mut v = Vec::new();
    heif_enc_cells(&mut v);
    sips_cells(&mut v);
    magick_cells(&mut v);
    ffmpeg_cells(&mut v);
    ours_cells(&mut v);
    if std::env::var_os("OXIDEAV_HEIF_LIVE_BIG").is_some() {
        big_cells(&mut v);
    }
    v
}

// ───────────────────────── this crate's writer ─────────────────────────

/// A picture of the matrix source in the given layout: BT.601 YCbCr
/// (or the R, G, B planes as G, B, R for `identity`), full range or the
/// nominal limited range, with the source's alpha plane when `alpha`.
#[allow(clippy::too_many_arguments)]
fn picture(
    w: u32,
    h: u32,
    chroma: Chroma,
    depth: u8,
    alpha: bool,
    limited: bool,
    identity: bool,
    seed: u32,
) -> HeifFrame {
    let grey = chroma == Chroma::Mono;
    let channels = if grey { 1 } else { 3 } + alpha as usize;
    let s = source(w, h, channels, 16, seed);
    let max = ((1u32 << depth) - 1) as f64;
    let scale = (1u32 << (depth - 8)) as f64;
    let mut full = HeifFrame::zeroed(
        w,
        h,
        HeifPixelFormat::new(
            if grey { Chroma::Mono } else { Chroma::Yuv444 },
            depth,
            false,
        )
        .unwrap(),
    )
    .unwrap();
    let mut alpha_plane = HeifFrame::zeroed(
        w,
        h,
        HeifPixelFormat::new(Chroma::Mono, depth, false).unwrap(),
    )
    .unwrap();
    let luma = |v: f64| {
        if limited {
            (16.0 + 219.0 * v) * scale
        } else {
            v * max
        }
    };
    let chroma_of = |v: f64| {
        if limited {
            (128.0 + 224.0 * v) * scale
        } else {
            v * max + (max + 1.0) / 2.0
        }
    };
    let q = |v: f64| v.round().clamp(0.0, max) as u16;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize * channels;
            let n = |k: usize| s[i + k] as f64 / 65535.0;
            if grey {
                full.set_sample(0, x, y, q(luma(n(0))));
            } else if identity {
                full.set_sample(0, x, y, q(luma(n(1))));
                full.set_sample(1, x, y, q(luma(n(2))));
                full.set_sample(2, x, y, q(luma(n(0))));
            } else {
                let (r, g, b) = (n(0), n(1), n(2));
                let yv = 0.299 * r + 0.587 * g + 0.114 * b;
                full.set_sample(0, x, y, q(luma(yv)));
                full.set_sample(1, x, y, q(chroma_of((b - yv) / 1.772)));
                full.set_sample(2, x, y, q(chroma_of((r - yv) / 1.402)));
            }
            if alpha {
                alpha_plane.set_sample(0, x, y, q(n(channels - 1) * max));
            }
        }
    }
    let colour = if grey || chroma == Chroma::Yuv444 {
        full
    } else {
        oxideav_heif::encode::to_layout(&full, chroma, depth).unwrap()
    };
    if alpha {
        oxideav_heif::compose::attach_alpha(&colour, &alpha_plane).unwrap()
    } else {
        colour
    }
}

fn plain(w: u32, h: u32, chroma: Chroma, depth: u8, alpha: bool) -> HeifFrame {
    picture(w, h, chroma, depth, alpha, false, false, 7)
}

/// A structurally valid display-class ICC profile header (no tags).
fn small_icc() -> Vec<u8> {
    let mut b = vec![0u8; 132];
    b[0..4].copy_from_slice(&132u32.to_be_bytes());
    b[8] = 4;
    b[12..16].copy_from_slice(b"mntr");
    b[16..20].copy_from_slice(b"RGB ");
    b[20..24].copy_from_slice(b"XYZ ");
    b[36..40].copy_from_slice(b"acsp");
    b
}

fn ours<F>(tag: &str, ext: &'static str, make: F) -> Cell
where
    F: Fn(Option<&Path>) -> Result<Vec<u8>, String> + Send + Sync + 'static,
{
    let mut c = cell(Producer::Ours, &format!("ours_{tag}"), ext, "", &[]);
    c.make = Some(std::sync::Arc::new(make));
    c
}

/// Every path of this crate's writer: the still encoder over its
/// codecs, layouts, sizes and options; derived items; auxiliaries;
/// metadata; the low-overhead form; the contract functions;
/// collections, sequences and re-muxes of foreign files.
fn ours_cells(out: &mut Vec<Cell>) {
    use oxideav_heif::encode::{
        EncodeOptions, OverlayLayer, OverlaySpec, StillCodec, TransformCarrier, encode_overlay,
        encode_still, encode_still_minimized,
    };
    use oxideav_heif::props::{Clap, CropRect, Imir, Irot, Property};
    let e = |r: oxideav_heif::error::Result<Vec<u8>>| r.map_err(|e| e.to_string());
    let codecs: [(&str, &'static str, EncodeOptions, bool); 4] = [
        (
            "hevc",
            "heic",
            EncodeOptions::default()
                .with_hevc_mode("intra".into())
                .with_qp(22),
            false,
        ),
        (
            "hevc_lossless",
            "heic",
            EncodeOptions::default().with_hevc_mode("pcm".into()),
            true,
        ),
        (
            "av1",
            "avif",
            EncodeOptions::default()
                .with_codec(StillCodec::Av1)
                .with_av1_quality(Some(60)),
            false,
        ),
        (
            "av1_lossless",
            "avif",
            EncodeOptions::default().with_codec(StillCodec::Av1),
            true,
        ),
    ];
    let chroma_tag = |c: Chroma| match c {
        Chroma::Mono => "c400",
        Chroma::Yuv420 => "c420",
        Chroma::Yuv422 => "c422",
        Chroma::Yuv444 => "c444",
    };
    for (name, ext, base, lossless) in codecs {
        let still = |tag: String, frame: HeifFrame, opts: EncodeOptions| {
            ours(&format!("{name}_{tag}"), ext, move |_| {
                e(encode_still(&frame, &opts))
            })
        };
        // Layout × depth, even and odd.
        for chroma in [Chroma::Yuv420, Chroma::Yuv422, Chroma::Yuv444, Chroma::Mono] {
            for depth in [8u8, 10, 12] {
                out.push(still(
                    format!("{}_b{depth}", chroma_tag(chroma)),
                    plain(96, 80, chroma, depth, false),
                    base.clone().with_chroma(Some(chroma)),
                ));
            }
            out.push(still(
                format!("{}_odd", chroma_tag(chroma)),
                plain(97, 81, chroma, 8, false),
                base.clone().with_chroma(Some(chroma)),
            ));
            out.push(still(
                format!("{}_alpha", chroma_tag(chroma)),
                plain(97, 81, chroma, 8, true),
                base.clone().with_chroma(Some(chroma)),
            ));
        }
        out.push(still(
            "alpha_b10".into(),
            plain(96, 80, Chroma::Yuv420, 10, true),
            base.clone(),
        ));
        out.push(still(
            "alpha_premultiplied".into(),
            plain(96, 80, Chroma::Yuv420, 8, true),
            base.clone().with_premultiplied_alpha(true),
        ));
        // Sizes.
        for (w, h) in [(1, 1), (2, 2), (7, 5), (1, 33), (65, 1), (513, 385)] {
            out.push(still(
                format!("size_{w}x{h}"),
                plain(w, h, Chroma::Yuv420, 8, false),
                base.clone(),
            ));
        }
        // Grids.
        out.push(still(
            "grid64".into(),
            plain(513, 385, Chroma::Yuv420, 8, false),
            base.clone().with_grid_tile(Some(64)),
        ));
        out.push(still(
            "grid64_alpha_thumb".into(),
            plain(200, 150, Chroma::Yuv420, 8, true),
            base.clone()
                .with_grid_tile(Some(64))
                .with_thumbnail_max_dim(Some(48)),
        ));
        out.push(still(
            "grid64_444_b10".into(),
            plain(200, 150, Chroma::Yuv444, 10, false),
            base.clone().with_grid_tile(Some(64)),
        ));
        // Thumbnail and metadata.
        out.push(still(
            "thumbnail".into(),
            plain(96, 80, Chroma::Yuv420, 8, false),
            base.clone().with_thumbnail_max_dim(Some(32)),
        ));
        out.push(still(
            "exif_xmp_icc".into(),
            plain(96, 80, Chroma::Yuv420, 8, false),
            base.clone()
                .with_exif(Some(b"II*\0\x08\0\0\0\0\0\0\0\0\0".to_vec()))
                .with_xmp(Some(
                    "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">live matrix</x:xmpmeta>".into(),
                ))
                .with_icc_profile(Some(small_icc())),
        ));
        // Colour descriptions.
        if !lossless {
            let nclx = |p: u16, t: u16, m: u16, full: bool| Colr::Nclx {
                primaries: p,
                transfer: t,
                matrix: m,
                full_range: full,
            };
            out.push(still(
                "limited".into(),
                picture(96, 80, Chroma::Yuv420, 8, false, true, false, 7),
                base.clone().with_colr(nclx(1, 13, 6, false)),
            ));
            out.push(still(
                "limited_grey".into(),
                picture(96, 80, Chroma::Mono, 8, false, true, false, 7),
                base.clone().with_colr(nclx(1, 13, 6, false)),
            ));
            out.push(still(
                "bt709".into(),
                plain(96, 80, Chroma::Yuv420, 8, false),
                base.clone().with_colr(nclx(1, 1, 1, true)),
            ));
            out.push(still(
                "bt2020_pq_b10".into(),
                plain(96, 80, Chroma::Yuv420, 10, false),
                base.clone().with_colr(nclx(9, 16, 9, false)),
            ));
        } else {
            out.push(still(
                "identity_444".into(),
                picture(96, 80, Chroma::Yuv444, 8, false, false, true, 7),
                base.clone().with_colr(Colr::Nclx {
                    primaries: 1,
                    transfer: 13,
                    matrix: 0,
                    full_range: true,
                }),
            ));
        }
        // Transformative properties, on 4:4:4 (odd), 4:2:0 even and
        // 4:2:0 odd; `irot` turns anti-clockwise.
        let transforms: [(&str, Vec<Property>, Vec<Op>); 7] = [
            (
                "irot1",
                vec![Property::Irot(Irot::new(1))],
                vec![Op::RotateCw(3)],
            ),
            (
                "irot2",
                vec![Property::Irot(Irot::new(2))],
                vec![Op::RotateCw(2)],
            ),
            (
                "irot3",
                vec![Property::Irot(Irot::new(3))],
                vec![Op::RotateCw(1)],
            ),
            (
                "imir0",
                vec![Property::Imir(Imir::new(0))],
                vec![Op::MirrorV],
            ),
            (
                "imir1",
                vec![Property::Imir(Imir::new(1))],
                vec![Op::MirrorH],
            ),
            (
                "irot1_imir1",
                vec![Property::Irot(Irot::new(1)), Property::Imir(Imir::new(1))],
                vec![Op::RotateCw(3), Op::MirrorH],
            ),
            (
                "irot3_imir0",
                vec![Property::Irot(Irot::new(3)), Property::Imir(Imir::new(0))],
                vec![Op::RotateCw(1), Op::MirrorV],
            ),
        ];
        for (tname, props, ops) in transforms {
            for (variant, twin, chroma, (w, h)) in [
                ("444_odd", "c444_odd", Chroma::Yuv444, (97, 81)),
                ("420_even", "c420_b8", Chroma::Yuv420, (96, 80)),
                ("420_odd", "c420_odd", Chroma::Yuv420, (97, 81)),
            ] {
                out.push(
                    still(
                        format!("{tname}_{variant}"),
                        plain(w, h, chroma, 8, false),
                        base.clone()
                            .with_chroma(Some(chroma))
                            .with_transforms(props.clone()),
                    )
                    .transformed(&format!("ours_{name}_{twin}"), &ops),
                );
            }
        }
        for (variant, twin, chroma, (w, h), rect) in [
            (
                "444",
                "c444_odd",
                Chroma::Yuv444,
                (97u32, 81u32),
                (5u32, 3u32, 61u32, 47u32),
            ),
            ("420", "c420_b8", Chroma::Yuv420, (96, 80), (8, 8, 64, 48)),
        ] {
            let crop = CropRect {
                x: rect.0,
                y: rect.1,
                width: rect.2,
                height: rect.3,
            };
            out.push(
                still(
                    format!("clap_{variant}"),
                    plain(w, h, chroma, 8, false),
                    base.clone()
                        .with_chroma(Some(chroma))
                        .with_transforms(vec![Property::Clap(Clap::for_rect(w, h, crop))]),
                )
                .transformed(
                    &format!("ours_{name}_{twin}"),
                    &[Op::Crop(rect.0, rect.1, rect.2, rect.3)],
                ),
            );
        }
        out.push(
            still(
                "iden_irot1_imir1".into(),
                plain(97, 81, Chroma::Yuv444, 8, false),
                base.clone()
                    .with_chroma(Some(Chroma::Yuv444))
                    .with_transform_carrier(TransformCarrier::Identity)
                    .with_transforms(vec![
                        Property::Irot(Irot::new(1)),
                        Property::Imir(Imir::new(1)),
                    ]),
            )
            .transformed(
                &format!("ours_{name}_c444_odd"),
                &[Op::RotateCw(3), Op::MirrorH],
            ),
        );
        // Depth auxiliary, overlay, the low-overhead form.
        out.push(still(
            "depth_map".into(),
            plain(96, 80, Chroma::Yuv420, 8, false),
            base.clone()
                .with_depth_map(Some(plain(48, 40, Chroma::Mono, 8, false))),
        ));
        {
            let opts = base.clone();
            out.push(ours(&format!("{name}_overlay"), ext, move |_| {
                let spec = OverlaySpec::new(
                    96,
                    80,
                    [12000, 30000, 50000, 65535],
                    vec![
                        OverlayLayer::new(plain(64, 48, Chroma::Yuv420, 8, false), 4, 4),
                        OverlayLayer::new(plain(40, 40, Chroma::Yuv420, 8, true), 48, 32),
                    ],
                );
                e(encode_overlay(&spec, &opts))
            }));
        }
        for (tag, alpha) in [("low_overhead", false), ("low_overhead_alpha", true)] {
            let opts = base.clone();
            out.push(ours(&format!("{name}_{tag}"), ext, move |_| {
                e(encode_still_minimized(
                    &plain(96, 80, Chroma::Yuv420, 8, alpha),
                    &opts,
                ))
            }));
        }
        // The contract functions: packed RGB(A) in, a collection, a
        // sequence, a sequence with alpha.
        {
            let opts = base.clone();
            out.push(ours(&format!("{name}_contract_rgb8"), ext, move |_| {
                let rgb: Vec<u8> = source(97, 81, 3, 8, 7).iter().map(|v| *v as u8).collect();
                e(oxideav_heif::encode_rgb8(97, 81, &rgb, &opts))
            }));
            let opts = base.clone();
            out.push(ours(&format!("{name}_contract_rgba8"), ext, move |_| {
                let rgba: Vec<u8> = source(96, 80, 4, 8, 7).iter().map(|v| *v as u8).collect();
                e(oxideav_heif::encode_rgba8(96, 80, &rgba, &opts))
            }));
            let image = |f: HeifFrame| {
                oxideav_heif::api::HeifImage::from_frame(
                    f,
                    oxideav_heif::api::ColorInfo::srgb(),
                    oxideav_heif::api::Metadata::default(),
                )
            };
            let opts = base.clone();
            out.push(
                ours(&format!("{name}_collection"), ext, move |_| {
                    let frames: Vec<oxideav_heif::api::Frame> = (1..4)
                        .map(|k| {
                            oxideav_heif::api::Frame::new(
                                image(picture(64, 48, Chroma::Yuv420, 8, false, false, false, k)),
                                None,
                                None,
                                None,
                            )
                        })
                        .collect();
                    e(oxideav_heif::encode_all(&frames, &opts))
                })
                .kind(Kind::Collection),
            );
            for (tag, alpha) in [("sequence", false), ("sequence_alpha", true)] {
                let opts = base.clone();
                out.push(
                    ours(&format!("{name}_{tag}"), ext, move |_| {
                        let frames: Vec<oxideav_heif::api::Frame> = (1..6)
                            .map(|k| {
                                oxideav_heif::api::Frame::new(
                                    image(picture(
                                        64,
                                        48,
                                        Chroma::Yuv420,
                                        8,
                                        alpha,
                                        false,
                                        false,
                                        k,
                                    )),
                                    Some(std::time::Duration::from_millis(40 * k as u64)),
                                    None,
                                    None,
                                )
                            })
                            .collect();
                        e(oxideav_heif::encode_all(&frames, &opts))
                    })
                    .kind(Kind::Sequence),
                );
            }
        }
    }
    // Rate extremes and the HEVC coder's structure options.
    let hevc = |qp: u8| {
        EncodeOptions::default()
            .with_hevc_mode("intra".into())
            .with_qp(qp)
    };
    let av1 = |q: u8| {
        EncodeOptions::default()
            .with_codec(StillCodec::Av1)
            .with_av1_quality(Some(q))
    };
    let mut still = |tag: &str, ext: &'static str, frame: HeifFrame, opts: EncodeOptions| {
        out.push(ours(tag, ext, move |_| e(encode_still(&frame, &opts))));
    };
    let mid = || plain(513, 385, Chroma::Yuv420, 8, false);
    for qp in [1, 51] {
        still(&format!("hevc_qp{qp}"), "heic", mid(), hevc(qp));
    }
    for q in [1, 100] {
        still(&format!("av1_q{q}"), "avif", mid(), av1(q));
    }
    for speed in ["fast", "slow"] {
        still(
            &format!("av1_speed_{speed}"),
            "avif",
            mid(),
            av1(60).with_av1_speed(speed.into()),
        );
    }
    for ctb in [16, 32, 64] {
        still(
            &format!("hevc_ctb{ctb}"),
            "heic",
            mid(),
            hevc(22).with_hevc_ctb(Some(ctb)),
        );
    }
    still(
        "hevc_tiles_2x2",
        "heic",
        mid(),
        hevc(22).with_hevc_tiles(Some("2x2".into())),
    );
    still("hevc_rd", "heic", mid(), hevc(22).with_hevc_rd(Some(1)));
    still(
        "hevc_no_loop_filters",
        "heic",
        mid(),
        hevc(22).with_hevc_filters(Some(false)),
    );
    still(
        "hevc_threads4",
        "heic",
        mid(),
        hevc(22).with_grid_tile(Some(128)).with_threads(Some(4)),
    );
    // Re-muxes of foreign files: every item of the source, rewritten.
    for (source_tag, ext, kind) in [
        ("he_hevc_thumb_alpha", "heic", Kind::Still),
        ("he_hevc_grid64", "heic", Kind::Still),
        ("he_av1_grid64_alpha", "avif", Kind::Still),
        ("he_av1_alpha_premultiplied", "avif", Kind::Still),
        ("he_hevc_clli", "heic", Kind::Still),
        ("he_av1_collection", "avif", Kind::Collection),
        ("sips_heic_from_jpeg_icc", "heic", Kind::Still),
        ("sips_avif_alpha", "avif", Kind::Still),
        ("magick_heic_exif", "heic", Kind::Still),
    ] {
        let mut c = ours(&format!("remux_{source_tag}"), ext, |input| {
            let bytes = std::fs::read(input.ok_or("no input")?).map_err(|e| e.to_string())?;
            oxideav_heif::HeifWriter::from_collection(&bytes)
                .and_then(|w| w.write_to_vec())
                .map_err(|e| e.to_string())
        })
        .kind(kind);
        c.input = Some(source_tag.to_string());
        out.push(c);
    }
}

// ───────────────────────── running tools ─────────────────────────

fn run(cmd: &mut Command) -> Result<std::process::Output, String> {
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if out.status.success() {
        Ok(out)
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        let line = msg
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty() && !l.starts_with("Try "))
            .unwrap_or("failed");
        Err(line.chars().take(110).collect())
    }
}

/// Write the cell's file; `Err` is the producer's refusal.
fn produce(
    dir: &Path,
    sources: &Sources,
    c: &Cell,
    input: Option<&Path>,
) -> Result<PathBuf, String> {
    if c.producer == Producer::Existing {
        return Ok(PathBuf::from(&c.args[0]));
    }
    let out = dir.join(format!("{}.{}", c.tag, c.ext));
    if let Some(make) = &c.make {
        if c.input.is_some() && input.is_none() {
            return Err("the foreign file to re-mux was not produced".into());
        }
        let bytes = make(input)?;
        std::fs::write(&out, bytes).map_err(|e| e.to_string())?;
        return Ok(out);
    }
    let mut paths = Vec::new();
    for s in &c.sources {
        paths.push(
            sources
                .get(s)
                .ok_or_else(|| format!("source {s} unavailable"))?,
        );
    }
    let resolve = |a: &String| -> std::ffi::OsString {
        if a == "@seq" {
            return sources.dir.join("seq%d.png").into();
        }
        match a.strip_prefix('@').and_then(|n| n.parse::<usize>().ok()) {
            Some(i) => paths[i].clone().into(),
            None => a.into(),
        }
    };
    let mut cmd;
    match c.producer {
        Producer::HeifEnc => {
            cmd = Command::new("heif-enc");
            cmd.args(c.args.iter().map(resolve))
                .args(&paths)
                .arg("-o")
                .arg(&out);
        }
        Producer::Sips => {
            cmd = Command::new("sips");
            cmd.args(c.args.iter().map(resolve))
                .args(&paths)
                .arg("--out")
                .arg(&out);
        }
        Producer::Magick => {
            cmd = Command::new("magick");
            cmd.args(&paths).args(c.args.iter().map(resolve)).arg(&out);
        }
        Producer::Ffmpeg => {
            if c.args.iter().any(|a| a == "@seq") {
                for s in SEQ {
                    sources.get(s);
                }
            }
            cmd = Command::new("ffmpeg");
            cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
                .args(c.args.iter().map(resolve))
                .arg(&out);
        }
        Producer::Existing | Producer::Ours => unreachable!(),
    }
    run(&mut cmd)?;
    match std::fs::metadata(&out) {
        Ok(m) if m.len() > 0 => Ok(out),
        _ => Err("no output file".into()),
    }
}

// ───────────────────────── black-box decodes ─────────────────────────

/// One stream `ffmpeg` exposes: its frames as pictures in the
/// stream's own sample format.
struct ForeignStream {
    index: usize,
    pix_fmt: String,
    frames: Vec<HeifFrame>,
    /// Display duration of each packet in seconds, when reported.
    durations: Vec<f64>,
}

fn layout_of_pix_fmt(pix_fmt: &str) -> Option<(HeifPixelFormat, bool)> {
    let (base, depth) = if let Some(b) = pix_fmt.strip_suffix("10le") {
        (b, 10)
    } else if let Some(b) = pix_fmt.strip_suffix("12le") {
        (b, 12)
    } else {
        (pix_fmt, 8)
    };
    let (chroma, rgb) = match base {
        "yuv420p" | "yuvj420p" => (Chroma::Yuv420, false),
        "yuv422p" | "yuvj422p" => (Chroma::Yuv422, false),
        "yuv444p" | "yuvj444p" => (Chroma::Yuv444, false),
        "gray" => (Chroma::Mono, false),
        "gbrp" => (Chroma::Yuv444, true),
        _ => return None,
    };
    Some((HeifPixelFormat::new(chroma, depth, false).ok()?, rgb))
}

fn frame_from_raw(w: u32, h: u32, fmt: HeifPixelFormat, raw: &[u8]) -> Option<HeifFrame> {
    let mut f = HeifFrame::zeroed(w, h, fmt).ok()?;
    let bps = fmt.bytes_per_sample();
    let mut at = 0;
    for p in 0..fmt.plane_count() {
        let (pw, ph) = f.plane_dims(p);
        let row = pw as usize * bps;
        let stride = f.planes[p].stride;
        for y in 0..ph as usize {
            f.planes[p].data[y * stride..y * stride + row].copy_from_slice(raw.get(at..at + row)?);
            at += row;
        }
    }
    Some(f)
}

/// Every video stream of `file` as `ffmpeg` decodes it, untouched:
/// no display matrix applied, the stream's own pixel format.
fn foreign_streams(dir: &Path, file: &Path, tag: &str) -> Vec<ForeignStream> {
    let Ok(probe) = run(Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "stream=index,width,height,pix_fmt",
            "-of",
            "csv=p=0",
        ])
        .arg(file))
    else {
        return Vec::new();
    };
    let mut streams = Vec::new();
    for line in String::from_utf8_lossy(&probe.stdout).lines() {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < 4 {
            continue;
        }
        let (Ok(index), Ok(w), Ok(h)) = (
            f[0].parse::<usize>(),
            f[1].parse::<u32>(),
            f[2].parse::<u32>(),
        ) else {
            continue;
        };
        let Some((fmt, _)) = layout_of_pix_fmt(f[3]) else {
            continue;
        };
        let raw_path = dir.join(format!("{tag}.s{index}.raw"));
        if run(Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "error",
                "-y",
                "-noautorotate",
                // Only the codec's own cropping (the conformance
                // window): the stream is the reconstructed picture,
                // before the container's clean aperture.
                "-apply_cropping",
                "codec",
                "-i",
            ])
            .arg(file)
            .args([
                "-map",
                &format!("0:{index}"),
                "-fps_mode",
                "passthrough",
                "-f",
                "rawvideo",
                "-pix_fmt",
                f[3],
            ])
            .arg(&raw_path))
        .is_err()
        {
            continue;
        }
        let Ok(raw) = std::fs::read(&raw_path) else {
            continue;
        };
        let _ = std::fs::remove_file(&raw_path);
        let per_frame: usize = (0..fmt.plane_count())
            .map(|p| {
                let (pw, ph) = fmt.plane_dims(p, w, h);
                pw as usize * ph as usize * fmt.bytes_per_sample()
            })
            .sum();
        if per_frame == 0 || raw.is_empty() || !raw.len().is_multiple_of(per_frame) {
            continue;
        }
        let frames: Vec<HeifFrame> = raw
            .chunks(per_frame)
            .filter_map(|c| frame_from_raw(w, h, fmt, c))
            .collect();
        let durations = run(Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                &index.to_string(),
                "-show_entries",
                "packet=duration_time",
                "-of",
                "csv=p=0",
            ])
            .arg(file))
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim_end_matches(',').parse().ok())
                .collect()
        })
        .unwrap_or_default();
        streams.push(ForeignStream {
            index,
            pix_fmt: f[3].to_string(),
            frames,
            durations,
        });
    }
    streams
}

/// The frames of stream `s` as the black-box video decoder displays
/// them (display matrix and clean aperture applied), read at the
/// geometry of `like`.
fn foreign_display_frames(
    dir: &Path,
    file: &Path,
    s: &ForeignStream,
    like: &HeifFrame,
    tag: &str,
) -> Option<Vec<HeifFrame>> {
    let raw_path = dir.join(format!("{tag}.d{}.raw", s.index));
    run(Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(file)
        .args([
            "-map",
            &format!("0:{}", s.index),
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            &s.pix_fmt,
        ])
        .arg(&raw_path))
    .ok()?;
    let raw = std::fs::read(&raw_path).ok()?;
    let _ = std::fs::remove_file(&raw_path);
    let fmt = like.format.without_alpha();
    let per_frame: usize = (0..fmt.plane_count())
        .map(|p| {
            let (pw, ph) = fmt.plane_dims(p, like.width, like.height);
            pw as usize * ph as usize * fmt.bytes_per_sample()
        })
        .sum();
    if raw.is_empty() || !raw.len().is_multiple_of(per_frame) {
        return None;
    }
    raw.chunks(per_frame)
        .map(|c| frame_from_raw(like.width, like.height, fmt, c))
        .collect()
}

/// An RGB(A) picture in 8-bit units.
#[derive(Clone, PartialEq)]
struct Rgba {
    w: u32,
    h: u32,
    alpha: bool,
    /// `w × h × 4` samples (alpha 255 without one).
    data: Vec<u8>,
}

impl Rgba {
    fn from_png(bytes: &[u8]) -> Self {
        let png = read_png(bytes);
        let shift = png.bit_depth.saturating_sub(8);
        let alpha = png.channels == 2 || png.channels == 4;
        let mut data = Vec::with_capacity(png.width as usize * png.height as usize * 4);
        for px in png.samples.chunks(png.channels) {
            let s = |i: usize| (px[i] >> shift) as u8;
            match png.channels {
                1 => data.extend([s(0), s(0), s(0), 255]),
                2 => data.extend([s(0), s(0), s(0), s(1)]),
                3 => data.extend([s(0), s(1), s(2), 255]),
                _ => data.extend([s(0), s(1), s(2), s(3)]),
            }
        }
        Self {
            w: png.width,
            h: png.height,
            alpha,
            data,
        }
    }

    fn from_frame(frame: &HeifFrame, colr: &Colr) -> Result<Self, String> {
        let rgb = to_rgb(frame, Some(colr)).map_err(|e| e.to_string())?;
        let shift = rgb.bit_depth.saturating_sub(8);
        let mut data = Vec::with_capacity(rgb.width as usize * rgb.height as usize * 4);
        for px in rgb.data.chunks(rgb.channels) {
            data.extend([
                (px[0] >> shift) as u8,
                (px[1] >> shift) as u8,
                (px[2] >> shift) as u8,
                if rgb.channels == 4 {
                    (px[3] >> shift) as u8
                } else {
                    255
                },
            ]);
        }
        Ok(Self {
            w: rgb.width,
            h: rgb.height,
            alpha: rgb.channels == 4,
            data,
        })
    }

    fn px(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.w as usize + x as usize) * 4;
        [
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
            self.data[i + 3],
        ]
    }

    /// The `w × h` region at the origin.
    fn top_left(&self, w: u32, h: u32) -> Self {
        let mut data = Vec::with_capacity(w as usize * h as usize * 4);
        for y in 0..h {
            let at = y as usize * self.w as usize * 4;
            data.extend_from_slice(&self.data[at..at + w as usize * 4]);
        }
        Self {
            w,
            h,
            alpha: self.alpha,
            data,
        }
    }

    /// The picture after `op`, by the definition of the property.
    fn apply(&self, op: Op) -> Self {
        let (w, h) = match op {
            Op::RotateCw(k) if k % 2 == 1 => (self.h, self.w),
            Op::Crop(_, _, w, h) => (w, h),
            _ => (self.w, self.h),
        };
        let mut data = Vec::with_capacity(self.data.len());
        for y in 0..h {
            for x in 0..w {
                let (sx, sy) = match op {
                    // A clockwise quarter turn: the left column of the
                    // source becomes the top row, bottom first.
                    Op::RotateCw(1) => (y, self.h - 1 - x),
                    Op::RotateCw(2) => (self.w - 1 - x, self.h - 1 - y),
                    Op::RotateCw(3) => (self.w - 1 - y, x),
                    Op::RotateCw(_) => (x, y),
                    Op::MirrorH => (self.w - 1 - x, y),
                    Op::MirrorV => (x, self.h - 1 - y),
                    Op::Crop(cx, cy, _, _) => (cx + x, cy + y),
                };
                data.extend(self.px(sx, sy));
            }
        }
        Self {
            w,
            h,
            alpha: self.alpha,
            data,
        }
    }

    /// `(max colour difference, mean, max alpha difference)`, or the
    /// reason the pictures cannot be compared.
    fn delta(&self, other: &Self) -> Result<(u32, f64, u32), String> {
        if (self.w, self.h) != (other.w, other.h) {
            return Err(format!(
                "{}x{} here, {}x{} there",
                self.w, self.h, other.w, other.h
            ));
        }
        let (mut max, mut sum, mut amax) = (0u32, 0u64, 0u32);
        for (a, b) in self
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .zip(other.data.as_chunks::<4>().0)
        {
            for c in 0..3 {
                let d = a[c].abs_diff(b[c]) as u32;
                max = max.max(d);
                sum += d as u64;
            }
            amax = amax.max(a[3].abs_diff(b[3]) as u32);
        }
        Ok((max, sum as f64 / (self.data.len() as f64 * 0.75), amax))
    }
}

/// What `heif-dec` makes of `file`: its PNG renders in output order
/// (one per top-level image, or per sequence frame with `sequence`),
/// the Exif payload from the TIFF header on, the XMP packet, and the
/// ICC profile it embedded in the first PNG.
struct ForeignRender {
    pngs: Vec<Vec<u8>>,
    exif: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
    icc: Option<Vec<u8>>,
}

fn foreign_render(
    dir: &Path,
    file: &Path,
    tag: &str,
    sequence: bool,
) -> Result<ForeignRender, String> {
    let sub = dir.join(format!("{tag}.render"));
    let _ = std::fs::remove_dir_all(&sub);
    std::fs::create_dir_all(&sub).map_err(|e| e.to_string())?;
    let out = sub.join("o.png");
    let mut cmd = Command::new("heif-dec");
    cmd.arg("--quiet");
    if sequence {
        cmd.args(["-S", "--ignore-editlist"]);
    } else {
        cmd.args(["--with-exif", "--with-xmp"]);
    }
    let result = run(cmd.arg(file).arg(&out));
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&sub);
        return Err(e);
    }
    let read = |name: &str| std::fs::read(sub.join(name)).ok();
    let mut pngs = Vec::new();
    if let Some(p) = read("o.png") {
        pngs.push(p);
    }
    for i in 1.. {
        match read(&format!("o-{i}.png")) {
            Some(p) => pngs.push(p),
            None => break,
        }
    }
    // The Exif file starts with the item's 4-byte offset to the TIFF
    // header (HEIF A.2.1).
    let exif = read("o.exif").or_else(|| read("o-1.exif")).and_then(|e| {
        let off = u32::from_be_bytes(e.get(..4)?.try_into().ok()?) as usize;
        e.get(4 + off..).map(<[u8]>::to_vec)
    });
    let xmp = read("o.xmp").or_else(|| read("o-1.xmp"));
    let icc = pngs.first().and_then(|p| png_icc(p));
    let _ = std::fs::remove_dir_all(&sub);
    Ok(ForeignRender {
        pngs,
        exif,
        xmp,
        icc,
    })
}

/// The profile of a PNG's `iCCP` chunk, inflated.
fn png_icc(png: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 8;
    while pos + 8 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().ok()?) as usize;
        let body = png.get(pos + 8..pos + 8 + len)?;
        if &png[pos + 4..pos + 8] == b"iCCP" {
            let name_end = body.iter().position(|b| *b == 0)?;
            return compcol::vec::decompress_to_vec::<compcol::zlib::Zlib>(
                body.get(name_end + 2..)?,
            )
            .ok();
        }
        pos += 12 + len;
    }
    None
}

/// The fields of `heif-info`'s report this matrix compares: those of
/// the primary image's block plus the sequence lines.
#[derive(Default, Debug)]
struct ForeignInfo {
    size: Option<(u32, u32)>,
    bit_depth: Option<u8>,
    thumbnails: usize,
    nclx: Option<(u16, u16, u16, bool)>,
    has_icc: bool,
    alpha: Option<bool>,
    premultiplied: bool,
    exif_len: Option<usize>,
    xmp_len: Option<usize>,
    clli: Option<(u16, u16)>,
    pasp: Option<(u32, u32)>,
    repetitions: Option<LoopBehaviour>,
}

fn foreign_info(file: &Path) -> Option<ForeignInfo> {
    let out = run(Command::new("heif-info").arg(file)).ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut info = ForeignInfo::default();
    let mut in_primary = false;
    let mut seen_primary = false;
    let (mut p, mut t, mut m) = (None, None, None);
    let first_number = |s: &str| -> Option<u32> {
        s.split(|c: char| !c.is_ascii_digit())
            .find(|x| !x.is_empty())?
            .parse()
            .ok()
    };
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("image: ") {
            in_primary = !seen_primary && (rest.contains("primary") || !text.contains("primary"));
            if in_primary {
                seen_primary = true;
                let dims = rest.split_whitespace().next()?;
                let (w, h) = dims.split_once('x')?;
                info.size = Some((w.parse().ok()?, h.parse().ok()?));
            }
            continue;
        }
        if let Some(v) = l.strip_prefix("repetitions: ") {
            info.repetitions = if v.starts_with("infinite") {
                Some(LoopBehaviour::Forever)
            } else {
                first_number(v).map(|n| LoopBehaviour::Times(n as u64))
            };
        }
        if !in_primary {
            continue;
        }
        if let Some(v) = l.strip_prefix("bit depth: ") {
            info.bit_depth = v.parse().ok();
        } else if l.starts_with("thumbnail: ") {
            info.thumbnails += 1;
        } else if let Some(v) = l.strip_prefix("color profile: ") {
            if v == "prof" || v == "rICC" {
                info.has_icc = true;
            }
        } else if let Some(v) = l.strip_prefix("colour primaries: ") {
            p = first_number(v);
        } else if let Some(v) = l.strip_prefix("transfer characteristics: ") {
            t = first_number(v);
        } else if let Some(v) = l.strip_prefix("matrix coefficients: ") {
            m = first_number(v);
        } else if let Some(v) = l.strip_prefix("full range flag: ") {
            if let (Some(p), Some(t), Some(m), Some(r)) = (p, t, m, first_number(v)) {
                info.nclx = Some((p as u16, t as u16, m as u16, r == 1));
            }
        } else if let Some(v) = l.strip_prefix("alpha channel: ") {
            info.alpha = Some(v.starts_with("yes"));
            info.premultiplied = v.contains("premultiplied");
        } else if let Some(v) = l.strip_prefix("Exif: ") {
            info.exif_len = first_number(v).map(|n| n as usize);
        } else if let Some(v) = l.strip_prefix("XMP: ") {
            info.xmp_len = first_number(v).map(|n| n as usize);
        } else if let Some(v) = l.strip_prefix("max content light level: ") {
            info.clli = Some((v.parse().ok()?, 0));
        } else if let Some(v) = l.strip_prefix("max pic average light level: ") {
            if let Some(c) = &mut info.clli {
                c.1 = v.parse().ok()?;
            }
        } else if let Some(v) = l.strip_prefix("pixel aspect ratio: ") {
            let (h, s) = v.split_once('/')?;
            info.pasp = Some((h.trim().parse().ok()?, s.trim().parse().ok()?));
        }
    }
    Some(info)
}

// ───────────────────────── comparisons ─────────────────────────

/// Whether the colour planes of `ours` and `theirs` hold the same
/// samples where both have them. A black-box stream is the coded
/// picture; ours may be a region of it at the origin, or its 4:4:4
/// promotion (an odd clean aperture), which is replicated the same way
/// here before comparing.
fn planes_match(ours: &HeifFrame, theirs: &HeifFrame) -> bool {
    if ours.format.bit_depth != theirs.format.bit_depth {
        return false;
    }
    let promoted;
    let theirs = if ours.format.chroma == theirs.format.chroma {
        theirs
    } else if ours.format.chroma == Chroma::Yuv444 && theirs.format.chroma != Chroma::Mono {
        promoted = match theirs.promote_to_444() {
            Ok(p) => p,
            Err(_) => return false,
        };
        &promoted
    } else if ours.format.chroma == Chroma::Mono {
        // An alpha plane against the picture that codes it: only the
        // luma plane is relevant (HEIF 6.9.1).
        theirs
    } else {
        return false;
    };
    // A coded picture is at most one coding block larger than the
    // aperture, and the black-box demuxer drops at most one odd
    // column / row.
    if ours.width.abs_diff(theirs.width) >= 64 || ours.height.abs_diff(theirs.height) >= 64 {
        return false;
    }
    let (w, h) = (ours.width.min(theirs.width), ours.height.min(theirs.height));
    let bps = ours.format.bytes_per_sample();
    for p in 0..ours.format.chroma.colour_planes() {
        let (pw, ph) = ours.format.plane_dims(p, w, h);
        for y in 0..ph {
            if ours.row(p, y)[..pw as usize * bps] != theirs.row(p, y)[..pw as usize * bps] {
                return false;
            }
        }
    }
    true
}

/// The alpha plane of `f` as a monochrome picture.
fn alpha_as_mono(f: &HeifFrame) -> Option<HeifFrame> {
    let a = f.format.alpha_plane()?;
    let fmt = HeifPixelFormat::new(Chroma::Mono, f.format.bit_depth, false).ok()?;
    let mut out = HeifFrame::zeroed(f.width, f.height, fmt).ok()?;
    out.planes[0] = f.planes[a].clone();
    Some(out)
}

/// Whether two of this crate's surfaces yielded the same picture. A
/// surface without a native layout for the picture carries its 4:4:4
/// promotion (grey + alpha, deep 4:2:0 + alpha).
fn same_picture(a: &HeifFrame, b: &HeifFrame) -> bool {
    if (a.width, a.height) != (b.width, b.height)
        || a.format.bit_depth != b.format.bit_depth
        || a.format.has_alpha != b.format.has_alpha
    {
        return false;
    }
    let (ta, tb) = (a.tight(), b.tight());
    if a.format == b.format {
        return ta.planes == tb.planes;
    }
    if let (Some(pa), Some(pb)) = (a.format.alpha_plane(), b.format.alpha_plane())
        && ta.planes[pa] != tb.planes[pb]
    {
        return false;
    }
    if a.format.chroma == Chroma::Mono {
        return ta.planes[0] == tb.planes[0];
    }
    match (a.promote_to_444(), b.promote_to_444()) {
        (Ok(x), Ok(y)) => {
            let (x, y) = (x.tight(), y.tight());
            (0..3).all(|p| x.planes[p] == y.planes[p])
        }
        _ => false,
    }
}

// ───────────────────────── this crate's surfaces ─────────────────────────

/// The registry path: probe, demux, the `"heif"` decoder.
fn decode_registry(bytes: &[u8]) -> Result<HeifFrame, String> {
    use oxideav_core::{Frame, RuntimeContext};
    use std::io::Cursor;
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let mut cur = Cursor::new(bytes.to_vec());
    let name = ctx
        .containers
        .probe_input(&mut cur, None)
        .map_err(|e| format!("probe: {e}"))?;
    let mut demuxer = ctx
        .containers
        .open_demuxer(&name, Box::new(Cursor::new(bytes.to_vec())), &ctx.codecs)
        .map_err(|e| format!("open: {e}"))?;
    let streams = demuxer.streams().to_vec();
    let still = streams
        .iter()
        .find(|s| s.params.codec_id.as_str() == "heif")
        .ok_or("no still stream")?;
    let pkt = loop {
        let p = demuxer.next_packet().map_err(|e| format!("packet: {e}"))?;
        if p.stream_index == still.index {
            break p;
        }
    };
    let mut dec = ctx
        .codecs
        .first_decoder(&still.params)
        .map_err(|e| format!("decoder: {e}"))?;
    dec.send_packet(&pkt).map_err(|e| format!("send: {e}"))?;
    dec.flush().map_err(|e| e.to_string())?;
    let v = loop {
        match dec.receive_frame().map_err(|e| format!("receive: {e}"))? {
            Frame::Video(v) => break v,
            _ => continue,
        }
    };
    let pf = still
        .params
        .pixel_format
        .ok_or("no pixel format announced")?;
    let (w, h) = (
        still.params.width.ok_or("no width")?,
        still.params.height.ok_or("no height")?,
    );
    HeifFrame::from_core(&v, w, h, pf)
        .or_else(|_| HeifFrame::from_core_gbr(&v, w, h, pf))
        .map_err(|e| format!("from_core: {e}"))
}

// ───────────────────────── verdicts ─────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Verdict {
    Pass(String),
    Fail(String),
    /// No reference for this check on this cell.
    NotApplicable(String),
}

#[derive(Clone, Debug)]
struct Row {
    tag: String,
    /// `None` when the producer refused the cell.
    produced: Result<String, String>,
    checks: Vec<(&'static str, Verdict)>,
}

impl Row {
    fn passed(&self) -> bool {
        self.produced.is_ok()
            && !self
                .checks
                .iter()
                .any(|(_, v)| matches!(v, Verdict::Fail(_)))
    }
}

fn describe(f: &HeifFrame) -> String {
    format!(
        "{}x{} {} {}b{}",
        f.width,
        f.height,
        match f.format.chroma {
            Chroma::Mono => "4:0:0",
            Chroma::Yuv420 => "4:2:0",
            Chroma::Yuv422 => "4:2:2",
            Chroma::Yuv444 => "4:4:4",
        },
        f.format.bit_depth,
        if f.format.has_alpha { " +a" } else { "" }
    )
}

/// A track frame with the track header's turn taken back out (a
/// mirror, then anti-clockwise quarter turns — undone in reverse):
/// the black-box streams are the coded pictures.
fn coded_view(f: HeifFrame, turn: Option<oxideav_heif::sequence::TrackOrientation>) -> HeifFrame {
    use oxideav_heif::compose::{apply_imir, apply_irot};
    use oxideav_heif::props::{Imir, Irot};
    let Some(o) = turn else { return f };
    let unturned = apply_irot(&f, &Irot::new((4 - o.rotation % 4) % 4)).unwrap();
    if o.mirror {
        apply_imir(&unturned, &Imir::new(1)).unwrap()
    } else {
        unturned
    }
}

/// Every picture this crate reconstructs from the file, for the
/// coded-plane comparison: each coded image item without its
/// transformative properties, the alpha planes on their own, and
/// every frame of every track.
fn our_pictures(file: &HeifFile, bytes: &[u8]) -> (Vec<(String, HeifFrame)>, Vec<String>) {
    let mut pictures = Vec::new();
    let mut errors = Vec::new();
    if let Ok(meta) = file.meta() {
        for it in &meta.items {
            if !matches!(
                &it.item_type,
                b"hvc1" | b"av01" | b"avc1" | b"vvc1" | b"lhv1"
            ) {
                continue;
            }
            match build_graph(file, it.id)
                .and_then(|node| ItemDecoder::direct().decode_coded(file, &node))
            {
                Ok(f) => pictures.push((format!("item {}", it.id), f)),
                Err(e) => errors.push(format!("item {}: {e}", it.id)),
            }
        }
    }
    if matches!(parse_movie(file), Ok(Some(_))) {
        match oxideav_heif::decode_all(bytes) {
            Ok(frames) => {
                let movie = parse_movie(file).ok().flatten();
                for (i, fr) in frames.iter().enumerate() {
                    let Some(track) = fr.track_id else { continue };
                    if let Ok(f) = fr.image.to_frame() {
                        let f = coded_view(
                            f,
                            movie
                                .as_ref()
                                .and_then(|m| m.track(track))
                                .and_then(|t| t.orientation()),
                        );
                        if let Some(a) = alpha_as_mono(&f) {
                            pictures.push((format!("track {track} frame {i} alpha"), a));
                        }
                        pictures.push((format!("track {track} frame {i}"), f));
                    }
                }
            }
            Err(e) => errors.push(format!("decode_all: {e}")),
        }
    }
    (pictures, errors)
}

/// The coded-plane check: every frame of every black-box stream is
/// one of our pictures.
fn check_planes(streams: &[ForeignStream], ours: &[(String, HeifFrame)]) -> Verdict {
    if streams.is_empty() {
        return Verdict::NotApplicable("the black-box decoder exposes no stream".into());
    }
    let mut frames = 0;
    for s in streams {
        for (k, f) in s.frames.iter().enumerate() {
            if !ours.iter().any(|(_, o)| planes_match(o, f)) {
                return Verdict::Fail(format!(
                    "stream {} frame {k} ({}) equals none of {} pictures",
                    s.index,
                    describe(f),
                    ours.len()
                ));
            }
            frames += 1;
        }
    }
    Verdict::Pass(format!(
        "{frames} pictures of {} streams exact",
        streams.len()
    ))
}

fn rgb_verdict(ours: &Rgba, theirs: &Rgba, what: &str, tolerance: u32) -> Verdict {
    match ours.delta(theirs) {
        Err(e) => Verdict::Fail(format!("{what}: {e}")),
        Ok((max, mean, amax)) => {
            let text = if max == 0 && amax == 0 {
                format!("{what} exact")
            } else {
                format!("{what} max {max} mean {mean:.3}")
            };
            if ours.alpha != theirs.alpha {
                Verdict::Fail(format!(
                    "{what}: alpha {} here, {} there",
                    ours.alpha, theirs.alpha
                ))
            } else if amax > 0 {
                Verdict::Fail(format!("{what}: alpha differs by {amax}"))
            } else if max > tolerance {
                Verdict::Fail(text)
            } else {
                Verdict::Pass(text)
            }
        }
    }
}

fn check_metadata(
    img: &DecodedImage,
    render: Option<&ForeignRender>,
    info: Option<&ForeignInfo>,
) -> Verdict {
    let mut compared = Vec::new();
    let mut wrong = Vec::new();
    if let Some(r) = render {
        if r.exif != img.exif {
            wrong.push(format!(
                "Exif {:?} bytes here, {:?} there",
                img.exif.as_ref().map(Vec::len),
                r.exif.as_ref().map(Vec::len)
            ));
        } else if r.exif.is_some() {
            compared.push("Exif");
        }
        let xmp = img.xmp.as_ref().map(|x| x.as_bytes().to_vec());
        if r.xmp != xmp {
            wrong.push(format!(
                "XMP {:?} bytes here, {:?} there",
                xmp.as_ref().map(Vec::len),
                r.xmp.as_ref().map(Vec::len)
            ));
        } else if r.xmp.is_some() {
            compared.push("XMP");
        }
        if let (Some(a), Some(b)) = (&img.icc_profile, &r.icc) {
            if a != b {
                wrong.push(format!("ICC {} bytes here, {} there", a.len(), b.len()));
            } else {
                compared.push("ICC");
            }
        }
    }
    if let Some(i) = info {
        if let Some(size) = i.size
            && size != (img.frame.width, img.frame.height)
            && size != (img.frame.height, img.frame.width)
        {
            wrong.push(format!(
                "size {}x{} here, {}x{} there",
                img.frame.width, img.frame.height, size.0, size.1
            ));
        }
        if let Some(d) = i.bit_depth {
            if d != img.frame.format.bit_depth {
                wrong.push(format!(
                    "depth {} here, {d} there",
                    img.frame.format.bit_depth
                ));
            } else {
                compared.push("depth");
            }
        }
        if let Some(a) = i.alpha {
            if a != img.frame.format.has_alpha {
                wrong.push(format!(
                    "alpha {} here, {a} there",
                    img.frame.format.has_alpha
                ));
            } else if i.premultiplied != img.premultiplied_alpha {
                wrong.push(format!(
                    "premultiplied {} here, {} there",
                    img.premultiplied_alpha, i.premultiplied
                ));
            } else if a {
                compared.push("alpha");
            }
        }
        if i.has_icc != img.icc_profile.is_some() {
            wrong.push(format!(
                "ICC present {} here, {} there",
                img.icc_profile.is_some(),
                i.has_icc
            ));
        }
        if let (Some(n), true) = (i.nclx, img.nclx_explicit) {
            let ours = match &img.nclx {
                Colr::Nclx {
                    primaries,
                    transfer,
                    matrix,
                    full_range,
                } => Some((*primaries, *transfer, *matrix, *full_range)),
                _ => None,
            };
            if ours != Some(n) {
                wrong.push(format!("nclx {ours:?} here, {n:?} there"));
            } else {
                compared.push("nclx");
            }
        }
        if i.thumbnails != img.thumbnail_ids.len() {
            wrong.push(format!(
                "{} thumbnails here, {} there",
                img.thumbnail_ids.len(),
                i.thumbnails
            ));
        } else if i.thumbnails > 0 {
            compared.push("thumbnails");
        }
        let clli = img
            .properties
            .clli()
            .map(|c| (c.max_content_light_level, c.max_pic_average_light_level));
        if i.clli.is_some() || clli.is_some() {
            if i.clli != clli {
                wrong.push(format!("clli {clli:?} here, {:?} there", i.clli));
            } else {
                compared.push("clli");
            }
        }
        let pasp = img.properties.pasp().map(|p| (p.h_spacing, p.v_spacing));
        if let Some(theirs) = i.pasp {
            if pasp != Some(theirs) {
                wrong.push(format!("pasp {pasp:?} here, {theirs:?} there"));
            } else {
                compared.push("pasp");
            }
        }
    }
    if !wrong.is_empty() {
        Verdict::Fail(wrong.join("; "))
    } else if render.is_none() && info.is_none() {
        Verdict::NotApplicable("no black-box report".into())
    } else {
        Verdict::Pass(if compared.is_empty() {
            "agrees (nothing attached)".into()
        } else {
            compared.join(" ")
        })
    }
}

/// The contract API and the registry against the direct decode.
fn check_surfaces(bytes: &[u8], direct: &DecodedImage) -> Verdict {
    let mut wrong = Vec::new();
    match oxideav_heif::decode(bytes) {
        Err(e) => wrong.push(format!("decode(): {e}")),
        Ok(img) => {
            match img.to_frame() {
                Ok(f) if same_picture(&direct.frame, &f) => {}
                Ok(f) => wrong.push(format!("decode() yields {}", describe(&f))),
                Err(e) => wrong.push(format!("decode().to_frame(): {e}")),
            }
            if img.metadata.exif != direct.exif
                || img.metadata.icc != direct.icc_profile
                || img.metadata.xmp != direct.xmp.as_ref().map(|x| x.as_bytes().to_vec())
            {
                wrong.push("decode() metadata differs".into());
            }
            match oxideav_heif::api::info(bytes) {
                Err(e) => wrong.push(format!("info(): {e}")),
                Ok(i) => {
                    if (i.width, i.height, i.format) != (img.width, img.height, img.format) {
                        wrong.push(format!(
                            "info() announces {}x{} {:?}, decode() yields {}x{} {:?}",
                            i.width, i.height, i.format, img.width, img.height, img.format
                        ));
                    }
                    if i.has_alpha != direct.frame.format.has_alpha
                        || i.has_icc != direct.icc_profile.is_some()
                        || i.has_exif != direct.exif.is_some()
                        || i.has_xmp != direct.xmp.is_some()
                        || i.primary_item_id != Some(direct.item_id)
                    {
                        wrong.push("info() flags differ from the decode".into());
                    }
                }
            }
        }
    }
    match decode_registry(bytes) {
        Ok(f) if same_picture(&direct.frame, &f) => {}
        Ok(f) => wrong.push(format!("registry yields {}", describe(&f))),
        Err(e) => wrong.push(format!("registry: {e}")),
    }
    match oxideav_heif::decode_all(bytes) {
        Err(e) => wrong.push(format!("decode_all(): {e}")),
        Ok(frames) => match frames.iter().find(|f| f.item_id == Some(direct.item_id)) {
            None => wrong.push("decode_all() has no frame for the primary".into()),
            Some(f) => match f.image.to_frame() {
                Ok(f) if same_picture(&direct.frame, &f) => {}
                _ => wrong.push("decode_all() primary differs".into()),
            },
        },
    }
    if wrong.is_empty() {
        Verdict::Pass("direct = contract = registry = burst".into())
    } else {
        Verdict::Fail(wrong.join("; "))
    }
}

/// Cells this crate is known to refuse, with the reason: a regression
/// elsewhere still fails the test, and these count as not passing.
fn known_refusal(tag: &str) -> Option<&'static str> {
    if !tag.starts_with("he_") {
        return None;
    }
    if tag.contains("low_overhead") {
        // Amd 2 names the item / configuration types a low-overhead
        // file infers only for the 'vvi3' brand (L.4.3); these files
        // carry 'heic' / 'avif' in minor_version with
        // explicit_codec_types_flag = 0, which no staged document
        // defines.
        return Some(
            "low-overhead file inferring its codec types from a brand no staged document defines",
        );
    }
    if tag.contains("projection") {
        // An essential `prfr` property on the item: not a HEIF / MIAF
        // property (no staged document defines it), and an item with
        // an essential property the reader does not recognise shall
        // not be processed (ISO/IEC 14496-12 8.11.14).
        return Some("essential property defined outside the staged documents");
    }
    None
}

fn verify(
    dir: &Path,
    c: &Cell,
    file_path: &Path,
    twin: Option<&Path>,
) -> Vec<(&'static str, Verdict)> {
    let mut checks = Vec::new();
    let bytes = match std::fs::read(file_path) {
        Ok(b) => b,
        Err(e) => return vec![("decode", Verdict::Fail(format!("read: {e}")))],
    };
    let render = foreign_render(dir, file_path, &c.tag, false);
    let file = match HeifFile::parse(&bytes) {
        Ok(f) => f,
        Err(e) => {
            let v = match &render {
                Err(_) => Verdict::NotApplicable(format!("refused here and by the black box: {e}")),
                Ok(_) => Verdict::Fail(format!("parse: {e}")),
            };
            return vec![("decode", v)];
        }
    };
    let streams = foreign_streams(dir, file_path, &c.tag);
    let info = foreign_info(file_path);
    let has_primary = file.primary_item().is_ok();

    // 1. The primary image, when there is one.
    let direct = if has_primary {
        match decode_primary(&file, ItemDecoder::direct()) {
            Ok(img) => {
                checks.push(("decode", Verdict::Pass(describe(&img.frame))));
                Some(img)
            }
            Err(e) => {
                checks.push(("decode", Verdict::Fail(format!("decode_primary: {e}"))));
                None
            }
        }
    } else {
        None
    };

    // 2. Coded planes.
    let (pictures, errors) = our_pictures(&file, &bytes);
    checks.push((
        "planes",
        if errors.is_empty() {
            check_planes(&streams, &pictures)
        } else {
            Verdict::Fail(errors.join("; "))
        },
    ));

    if let Some(img) = &direct {
        // 3. The rendered primary.
        let ours = Rgba::from_frame(&img.frame, &img.nclx);
        let rgb = match (&ours, &render, twin) {
            (Err(e), _, _) => Verdict::Fail(format!("to_rgb: {e}")),
            (Ok(ours), _, Some(twin)) => {
                match foreign_render(dir, twin, &format!("{}.twin", c.tag), false) {
                    Ok(t) if !t.pngs.is_empty() => {
                        let mut want = Rgba::from_png(&t.pngs[0]);
                        for op in &c.ops {
                            want = want.apply(*op);
                        }
                        rgb_verdict(ours, &want, "vs transformed twin render", RGB_TOLERANCE)
                    }
                    _ => Verdict::NotApplicable("the black box does not render the twin".into()),
                }
            }
            (Ok(_), Ok(_), None) if limited_monochrome(img) => Verdict::NotApplicable(
                "the black-box readers show limited-range monochrome unscaled".into(),
            ),
            (Ok(ours), Ok(r), None) if !r.pngs.is_empty() => rgb_verdict(
                ours,
                &Rgba::from_png(&r.pngs[0]),
                "vs black-box render",
                if c.tag.contains("overlay") {
                    OVERLAY_TOLERANCE
                } else {
                    RGB_TOLERANCE
                },
            ),
            (Ok(_), Ok(_), None) => Verdict::NotApplicable("the black box wrote no picture".into()),
            (Ok(_), Err(e), None) => {
                Verdict::NotApplicable(format!("the black box refuses the file: {e}"))
            }
        };
        checks.push(("rgb", rgb));

        // 4. A transformed cell codes the picture its twin codes.
        if let Some(twin) = twin {
            let same_payload = std::fs::read(twin)
                .ok()
                .and_then(|t| {
                    let tf = HeifFile::parse(&t).ok()?;
                    let a = tf.item_data(tf.primary_item().ok()?.id).ok()?.to_vec();
                    let b = file.item_data(file.primary_item().ok()?.id).ok()?.to_vec();
                    Some(a == b)
                })
                .unwrap_or(false);
            if !same_payload && !c.tag.contains("grid") {
                checks.push((
                    "twin",
                    Verdict::NotApplicable("the twin's coded payload differs".into()),
                ));
            }
        }

        // 5. Metadata.
        checks.push((
            "metadata",
            check_metadata(img, render.as_ref().ok(), info.as_ref()),
        ));

        // 6. Surfaces.
        checks.push(("surfaces", check_surfaces(&bytes, img)));
    }

    // The writer direction: the readers open the file without a
    // complaint, the platform reader among them.
    if c.producer == Producer::Ours {
        checks.push(("readers", check_readers(dir, c, file_path, direct.as_ref())));
    }

    // 7. Collections: every top-level image the black box renders.
    if c.kind == Kind::Collection {
        checks.push(("collection", check_collection(&bytes, render.as_ref().ok())));
    }

    // 8. Sequences: frames, timing, repetitions.
    if c.kind == Kind::Sequence {
        checks.push((
            "sequence",
            check_sequence(dir, c, file_path, &file, &bytes, &streams, info.as_ref()),
        ));
    }
    checks
}

/// A monochrome picture whose `nclx` declares limited range. This
/// crate scales its samples by the flag (the range of H.273 applies to
/// luma whatever the chroma format; MIAF 8.8: "readers shall support
/// both values"); every black-box reader on this machine
/// shows such a picture unscaled — a correctly coded one (this crate's
/// `limited_grey` cells, samples within 16..235) and the black-box
/// encoder's, which flags limited range on full-range samples. There
/// is no reference rendering; the coded planes are still compared.
fn limited_monochrome(img: &DecodedImage) -> bool {
    img.frame.format.chroma == Chroma::Mono
        && matches!(
            img.nclx,
            Colr::Nclx {
                full_range: false,
                ..
            }
        )
}

/// What the readers say of a file this crate wrote: `heif-info`,
/// `heif-dec` and `ffmpeg` must take it without a word on their error
/// streams; the platform reader, when present, must open it (its
/// render is compared when the item is not transformed — it reports
/// `irot` / `imir` as an orientation instead of applying them — and
/// the difference is recorded: its colour pipeline is its own).
fn check_readers(dir: &Path, c: &Cell, file: &Path, direct: Option<&DecodedImage>) -> Verdict {
    let mut notes = Vec::new();
    let quiet = |name: &str, cmd: &mut Command| -> Result<(), String> {
        let out = cmd
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("{name}: {e}"))?;
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        if !out.status.success() {
            return Err(format!("{name} refuses the file: {line}"));
        }
        if !line.is_empty() {
            return Err(format!("{name} warns: {line}"));
        }
        Ok(())
    };
    let scratch = dir.join(format!("{}.readers.png", c.tag));
    let mut dec = Command::new("heif-dec");
    if direct.is_none() {
        dec.arg("-S");
    }
    let results = [
        quiet("heif-info", Command::new("heif-info").arg(file)),
        quiet("heif-dec", dec.arg(file).arg(&scratch)),
        quiet(
            "ffmpeg",
            Command::new("ffmpeg")
                .args(["-nostdin", "-v", "warning", "-i"])
                .arg(file)
                .args(["-f", "null", "-"]),
        ),
    ];
    let prefix = format!("{}.readers", c.tag);
    for f in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if f.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(f.path());
        }
    }
    let mut silent = Vec::new();
    for (name, r) in ["heif-info", "heif-dec", "ffmpeg"].iter().zip(results) {
        match r {
            Ok(()) => silent.push(*name),
            Err(e) => match reader_limit(&e) {
                Some(limit) => notes.push(format!("{name}: {limit}")),
                None => return Verdict::Fail(e.chars().take(160).collect()),
            },
        }
    }
    notes.insert(0, format!("{} silent", silent.join(", ")));
    if have_binary_cached("sips")
        && let Some(img) = direct
    {
        let png = dir.join(format!("{}.sips.png", c.tag));
        let opened = run(Command::new("sips")
            .args(["-s", "format", "png"])
            .arg(file)
            .arg("--out")
            .arg(&png));
        let bytes = std::fs::read(&png).ok();
        let _ = std::fs::remove_file(&png);
        match (opened, bytes) {
            (Ok(_), Some(b)) => {
                let theirs = Rgba::from_png(&b);
                match Rgba::from_frame(&img.frame, &img.nclx)
                    .ok()
                    .filter(|_| c.ops.is_empty())
                    .and_then(|ours| ours.delta(&theirs).ok())
                {
                    Some((max, mean, _)) => {
                        notes.push(format!("platform reader max {max} mean {mean:.2}"));
                    }
                    None => {
                        notes.push(format!("platform reader opens ({}x{})", theirs.w, theirs.h))
                    }
                }
            }
            // Measured on the black-box encoder's own files too (the
            // reader cells `he_hevc_c422_b12` / `he_hevc_c444_b12`).
            (Err(_), _)
                if img.frame.format.bit_depth == 12
                    && matches!(img.frame.format.chroma, Chroma::Yuv422 | Chroma::Yuv444)
                    && c.ext == "heic" =>
            {
                notes
                    .push("platform reader: no 12-bit 4:2:2 / 4:4:4 HEVC from any producer".into());
            }
            // The black-box encoder's low-overhead files are refused
            // the same way.
            (Err(_), _) if c.tag.contains("low_overhead") => {
                notes.push(
                    "platform reader: no low-overhead (`mini`) file from any producer".into(),
                );
            }
            (Err(e), _) => {
                return Verdict::Fail(format!("the platform reader refuses the file: {e}"));
            }
            _ => return Verdict::Fail("the platform reader wrote no picture".into()),
        }
    }
    Verdict::Pass(notes.join("; "))
}

/// What a black-box reader cannot do with any producer's file of the
/// shape, by the complaint it makes — the cell's other references
/// still hold. Each is measured on a foreign file as well: the
/// black-box encoder's 1-sample-wide pictures and low-overhead files
/// fail the same way, and no foreign producer here writes an `iden`.
fn reader_limit(complaint: &str) -> Option<&'static str> {
    [
        (
            "Invalid too big or non positive size",
            "cuts a clean aperture to even sizes and refuses a 1-sample one",
        ),
        (
            "moov atom not found",
            "does not read low-overhead (`mini`) files",
        ),
        (
            "Derived Image item of type iden",
            "does not support `iden` derivations",
        ),
    ]
    .into_iter()
    .find(|(needle, _)| complaint.contains(needle))
    .map(|(_, limit)| limit)
}

fn check_collection(bytes: &[u8], render: Option<&ForeignRender>) -> Verdict {
    let Some(render) = render else {
        return Verdict::NotApplicable("the black box refuses the file".into());
    };
    let frames = match oxideav_heif::decode_all(bytes) {
        Ok(f) => f,
        Err(e) => return Verdict::Fail(format!("decode_all: {e}")),
    };
    let ours: Vec<Rgba> = frames
        .iter()
        .filter(|f| f.item_id.is_some())
        .filter_map(|f| {
            let frame = f.image.to_frame().ok()?;
            Rgba::from_frame(&frame, &f.image.color.to_colr()).ok()
        })
        .collect();
    if ours.len() != render.pngs.len() {
        return Verdict::Fail(format!(
            "{} images here, {} there",
            ours.len(),
            render.pngs.len()
        ));
    }
    let mut worst = 0;
    for (i, png) in render.pngs.iter().enumerate() {
        let theirs = Rgba::from_png(png);
        let best = ours
            .iter()
            .filter_map(|o| o.delta(&theirs).ok())
            .filter(|d| d.2 == 0)
            .map(|d| d.0)
            .min();
        match best {
            Some(d) if d <= RGB_TOLERANCE => worst = worst.max(d),
            other => {
                return Verdict::Fail(format!("image {i} of the black box: best match {other:?}"));
            }
        }
    }
    Verdict::Pass(format!("{} images, max {worst}", ours.len()))
}

fn check_sequence(
    dir: &Path,
    c: &Cell,
    file_path: &Path,
    file: &HeifFile,
    bytes: &[u8],
    streams: &[ForeignStream],
    info: Option<&ForeignInfo>,
) -> Verdict {
    let movie = match parse_movie(file) {
        Ok(Some(m)) => m,
        Ok(None) => return Verdict::Fail("no movie".into()),
        Err(e) => return Verdict::Fail(format!("moov: {e}")),
    };
    let frames = match oxideav_heif::decode_all(bytes) {
        Ok(f) => f,
        Err(e) => return Verdict::Fail(format!("decode_all: {e}")),
    };
    let track_frames: Vec<_> = frames.iter().filter(|f| f.track_id.is_some()).collect();
    if track_frames.is_empty() {
        return Verdict::Fail("no track frames".into());
    }
    let mut notes = vec![format!("{} frames", track_frames.len())];
    // Timing: the stream with as many frames as the track.
    let delays: Vec<f64> = track_frames
        .iter()
        .filter_map(|f| f.delay.map(|d| d.as_secs_f64()))
        .collect();
    let track_pictures: Vec<HeifFrame> = track_frames
        .iter()
        .filter_map(|f| f.image.to_frame().ok())
        .collect();
    let turn = movie
        .visual_tracks()
        .find(|t| t.aux_kind().is_none())
        .and_then(|t| t.orientation())
        .filter(|o| o.mirror || o.rotation % 4 != 0);
    let coded_pictures: Vec<HeifFrame> = track_pictures
        .iter()
        .map(|f| coded_view(f.clone(), turn))
        .collect();
    // The streams showing the track's pictures (a one-frame track and
    // the item it was made from can be the same picture): one of them
    // carries the track's timing.
    let candidates: Vec<&ForeignStream> = streams
        .iter()
        .filter(|s| {
            s.durations.len() == delays.len()
                && s.frames.len() == coded_pictures.len()
                && s.frames
                    .iter()
                    .zip(&coded_pictures)
                    .all(|(t, o)| planes_match(o, t))
        })
        .collect();
    if !candidates.is_empty() {
        let agrees = |s: &&ForeignStream| {
            delays
                .iter()
                .zip(&s.durations)
                .all(|(a, b)| (a - b).abs() <= 2e-6)
        };
        if !candidates.iter().any(agrees) {
            return Verdict::Fail(format!(
                "frames last {delays:?} s here, {:?} s there",
                candidates[0].durations
            ));
        }
        notes.push("durations".into());
    }
    // Repetitions.
    if let (Some(theirs), Some(track)) = (
        info.and_then(|i| i.repetitions),
        movie.visual_tracks().find(|t| t.aux_kind().is_none()),
    ) {
        let ours = track.loop_behaviour();
        if track.edits.is_empty() {
            // MIAF Amd 1:2025 A.2: a track without an edit list plays
            // indefinitely. The black box reports such a track as
            // played once; the specification's table is what this
            // crate answers.
            assert_eq!(ours, LoopBehaviour::Forever);
            notes.push("no edit list: indefinitely (MIAF Amd 1 A.2)".into());
        } else if ours != theirs {
            return Verdict::Fail(format!("plays {ours:?} here, {theirs:?} there"));
        } else {
            notes.push(format!("{ours:?}"));
        }
    }
    // A turned track against the black-box video decoder's own
    // application of the display matrix: the same samples.
    if turn.is_some() {
        let shown = candidates.iter().any(|s| {
            foreign_display_frames(dir, file_path, s, &track_pictures[0], &c.tag).is_some_and(
                |theirs| {
                    theirs.len() == track_pictures.len()
                        && theirs
                            .iter()
                            .zip(&track_pictures)
                            .all(|(t, o)| same_picture(&o.without_alpha(), t))
                },
            )
        });
        if !shown {
            return Verdict::Fail(
                "the turned frames differ from the black-box decoder's display".into(),
            );
        }
        notes.push("track matrix".into());
    }
    // Every frame against the black-box render of the sequence.
    if let Ok(r) = foreign_render(dir, file_path, &format!("{}.seq", c.tag), true) {
        if r.pngs.len() == track_frames.len() {
            let mut worst = 0;
            let mut cropped = false;
            for (k, (f, png)) in track_frames.iter().zip(&r.pngs).enumerate() {
                let frame = match f.image.to_frame() {
                    Ok(f) => f,
                    Err(e) => return Verdict::Fail(format!("frame {k}: {e}")),
                };
                let ours = match Rgba::from_frame(&frame, &f.image.color.to_colr()) {
                    Ok(o) => o,
                    Err(e) => return Verdict::Fail(format!("frame {k}: {e}")),
                };
                // The black box renders a track's coded pictures: a
                // sample entry's clean aperture (at the origin in
                // every file here) is not applied by it.
                // Nor is the track header's matrix: the black-box
                // frame is turned here by the definition (a mirror,
                // then anti-clockwise quarter turns).
                let mut theirs = Rgba::from_png(png);
                let (cw, ch) = match turn {
                    Some(o) if o.rotation % 2 == 1 => (ours.h, ours.w),
                    _ => (ours.w, ours.h),
                };
                if theirs.w >= cw && theirs.h >= ch && (theirs.w, theirs.h) != (cw, ch) {
                    theirs = theirs.top_left(cw, ch);
                    cropped = true;
                }
                if let Some(o) = turn {
                    if o.mirror {
                        theirs = theirs.apply(Op::MirrorH);
                    }
                    theirs = theirs.apply(Op::RotateCw((4 - o.rotation % 4) % 4));
                }
                match ours.delta(&theirs) {
                    Ok((max, _, amax)) if max <= RGB_TOLERANCE && amax == 0 => {
                        worst = worst.max(max)
                    }
                    other => {
                        return Verdict::Fail(format!("frame {k} vs black-box render: {other:?}"));
                    }
                }
            }
            notes.push(format!(
                "rgb max {worst}{}",
                if cropped {
                    " (black-box frames cut to the clean aperture)"
                } else {
                    ""
                }
            ));
        } else if !r.pngs.is_empty() {
            return Verdict::Fail(format!(
                "{} frames here, {} there",
                track_frames.len(),
                r.pngs.len()
            ));
        }
    }
    Verdict::Pass(notes.join(", "))
}

// ───────────────────────── the test ─────────────────────────

#[test]
fn live_matrix_both_directions() {
    for needed in ["heif-dec", "ffmpeg", "ffprobe"] {
        if !have_binary_cached(needed) {
            eprintln!("SKIP live matrix: `{needed}` is not on this machine");
            return;
        }
    }
    let dir = scratch_dir("live");
    let sources = Sources {
        dir: dir.clone(),
        made: Mutex::new(BTreeMap::new()),
    };
    let all = cells();
    let cells: Vec<&Cell> = all
        .iter()
        .filter(|c| c.producer.binary().is_none_or(have_binary_cached))
        .collect();
    let absent: Vec<&str> = [Producer::HeifEnc, Producer::Sips, Producer::Magick]
        .iter()
        .filter_map(|p| p.binary())
        .filter(|b| !have_binary_cached(b))
        .collect();
    if cells.is_empty() {
        eprintln!("SKIP live matrix: no producer on this machine");
        return;
    }
    // Produce (twins are ordinary cells), then verify, on a few
    // workers: the time goes to the tools' processes.
    let produced: Vec<Mutex<Option<Result<PathBuf, String>>>> =
        cells.iter().map(|_| Mutex::new(None)).collect();
    let rows: Vec<Mutex<Option<Row>>> = cells.iter().map(|_| Mutex::new(None)).collect();
    let workers = 4;
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = cells.get(i) else { break };
                    if c.input.is_none() {
                        *produced[i].lock().unwrap() = Some(produce(&dir, &sources, c, None));
                    }
                }
            });
        }
    });
    let path_of = |tag: &str| -> Option<PathBuf> {
        let i = cells.iter().position(|c| c.tag == tag)?;
        produced[i].lock().unwrap().clone()?.ok()
    };
    // Re-muxes read a foreign cell's file.
    for (i, c) in cells.iter().enumerate() {
        if let Some(input) = &c.input {
            let source = path_of(input);
            *produced[i].lock().unwrap() = Some(produce(&dir, &sources, c, source.as_deref()));
        }
    }
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = cells.get(i) else { break };
                    let p = produced[i].lock().unwrap().clone().unwrap();
                    let row = match p {
                        // What this crate's writer refuses is a
                        // failure of the writer, not a producer's
                        // choice — unless the refusal is a re-mux
                        // whose foreign source was never produced.
                        Err(e)
                            if c.producer == Producer::Ours && !e.contains("was not produced") =>
                        {
                            Row {
                                tag: c.tag.clone(),
                                produced: Ok("not written".into()),
                                checks: vec![("write", Verdict::Fail(e))],
                            }
                        }
                        Err(e) => Row {
                            tag: c.tag.clone(),
                            produced: Err(e),
                            checks: Vec::new(),
                        },
                        Ok(path) => {
                            let twin = c.twin.as_deref().and_then(path_of);
                            let checks = if c.twin.is_some() && twin.is_none() {
                                vec![("rgb", Verdict::NotApplicable("no twin was produced".into()))]
                            } else {
                                verify(&dir, c, &path, twin.as_deref())
                            };
                            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                            Row {
                                tag: c.tag.clone(),
                                produced: Ok(format!("{size} bytes")),
                                checks,
                            }
                        }
                    };
                    *rows[i].lock().unwrap() = Some(row);
                }
            });
        }
    });
    let rows: Vec<Row> = rows
        .into_iter()
        .map(|r| r.into_inner().unwrap().unwrap())
        .collect();

    // Report.
    let produced_rows: Vec<&Row> = rows.iter().filter(|r| r.produced.is_ok()).collect();
    let refused = rows.len() - produced_rows.len();
    let passed = produced_rows.iter().filter(|r| r.passed()).count();
    let mut unexpected = Vec::new();
    let mut known = 0;
    println!("\n### Live matrix\n");
    for r in &rows {
        match &r.produced {
            Err(e) => println!("{:34} producer refused: {e}", r.tag),
            Ok(size) => {
                let cells: Vec<String> = r
                    .checks
                    .iter()
                    .map(|(n, v)| match v {
                        Verdict::Pass(t) => format!("{n}: {t}"),
                        Verdict::Fail(t) => format!("{n}: FAIL {t}"),
                        Verdict::NotApplicable(t) => format!("{n}: n/a ({t})"),
                    })
                    .collect();
                println!("{:34} {size:>14} | {}", r.tag, cells.join(" | "));
                if !r.passed() {
                    if known_refusal(&r.tag).is_some() {
                        known += 1;
                    } else {
                        unexpected.push(r.tag.clone());
                    }
                }
            }
        }
    }
    for (direction, ours) in [
        ("foreign files read here", false),
        ("files written here", true),
    ] {
        let of = |r: &&Row| r.tag.starts_with("ours_") == ours;
        println!(
            "{direction}: {} cells, {} files, {} pass",
            rows.iter().filter(of).count(),
            rows.iter()
                .filter(of)
                .filter(|r| r.produced.is_ok())
                .count(),
            rows.iter().filter(of).filter(|r| r.passed()).count()
        );
    }
    println!(
        "\nlive matrix: {} cells, {refused} refused by their producer, {} files, {passed} pass, \
         {known} known refusals, {} unexpected failures{}",
        rows.len(),
        produced_rows.len(),
        unexpected.len(),
        if absent.is_empty() {
            String::new()
        } else {
            format!(" (producers absent: {})", absent.join(", "))
        }
    );
    if std::env::var_os("OXIDEAV_HEIF_LIVE_KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("scratch kept at {}", dir.display());
    }
    assert!(
        unexpected.is_empty(),
        "cells that fail against the black-box decoders: {unexpected:?}"
    );
}

/// The orientation model of this file against the crate's own
/// transforms on a 4:4:4 picture, where no chroma position is in
/// play: the test's reference is the same mapping the crate applies.
#[test]
fn orientation_reference_model() {
    use oxideav_heif::compose::{apply_imir, apply_irot};
    use oxideav_heif::props::{Imir, Irot};
    let fmt = HeifPixelFormat::new(Chroma::Yuv444, 8, false).unwrap();
    let mut f = HeifFrame::zeroed(5, 3, fmt).unwrap();
    for p in 0..3 {
        for y in 0..3 {
            for x in 0..5 {
                f.set_sample(p, x, y, (p as u32 * 60 + y * 16 + x) as u16);
            }
        }
    }
    // Planes as "RGB": matrix 0 (identity), full range.
    let colr = Colr::Nclx {
        primaries: 1,
        transfer: 13,
        matrix: 0,
        full_range: true,
    };
    let base = Rgba::from_frame(&f, &colr).unwrap();
    for k in 1..4u8 {
        // `irot` turns anti-clockwise: k clockwise turns are 4 − k.
        let turned = apply_irot(&f, &Irot::new(4 - k)).unwrap();
        assert!(
            Rgba::from_frame(&turned, &colr).unwrap() == base.apply(Op::RotateCw(k)),
            "clockwise turn {k}"
        );
    }
    let h = apply_imir(&f, &Imir::new(1)).unwrap();
    assert!(Rgba::from_frame(&h, &colr).unwrap() == base.apply(Op::MirrorH));
    let v = apply_imir(&f, &Imir::new(0)).unwrap();
    assert!(Rgba::from_frame(&v, &colr).unwrap() == base.apply(Op::MirrorV));
}
