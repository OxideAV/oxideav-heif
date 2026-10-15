//! The framework surface of the `"heif"` encoder, table-driven: every
//! declared encoder option × every input `VideoFrame` layout the
//! gateway can hand it × the sizes that matter (1×1, odd, block-aligned,
//! ≥ 4 MP for the automatic grid). One cell = one assertion set:
//!
//! 1. the registry encoder either refuses the cell with a typed error
//!    (a documented codec limit) or yields one packet holding a file;
//! 2. the file opens in the framework demuxer with the expected stream
//!    description (geometry, pixel-format label, `1/1` untimed stream,
//!    `stream:0:item_ids`) and the `"heif"` decoder emits one frame;
//! 3. the contract `decode` of the same bytes yields byte-identical
//!    planes and the same label;
//! 4. the bytes equal the contract-path encode of the same picture under
//!    the same options (where the contract has the layout);
//! 5. the decoded picture is the source: exact under a lossless mode,
//!    ≥ 30 dB (RGBA8) under a lossy one;
//! 6. a black-box reader (`heif-info`, and Apple ImageIO `sips` for the
//!    HEVC / AV1 / AVC items it decodes) opens the file with the
//!    expected geometry — when the binary exists on the machine.
//!
//! The expected label per cell is the crate's documented coding rule
//! (README *Supported layouts* / *Framework surface*): the test is the
//! executable form of that table.
#![cfg(feature = "registry")]

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;

use oxideav_core::{
    CodecId, CodecOptions, CodecParameters, Frame, PixelFormat as Pf, RuntimeContext, TimeBase,
    VideoFrame, VideoPlane,
};
use oxideav_heif::encode::{coded_depth, EncodeOptions, HeifEncoderOptions};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat, Plane};
use oxideav_heif::{ColorInfo, HeifFile, HeifImage, PixelFormat as Hp};

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_h264::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    ctx
}

fn scratch() -> PathBuf {
    let d = std::env::var("OXIDEAV_HEIF_SCRATCH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("oxideav-heif-framework-matrix"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn have_binary(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------
// Input layouts
// ---------------------------------------------------------------------

/// What a framework layout is, for building a source and predicting
/// the coded output.
#[derive(Clone, Copy, Debug)]
struct Layout {
    pf: Pf,
    /// Planar chroma (`Yuv444` for planar RGB), `Mono` for grey.
    chroma: Chroma,
    depth: u8,
    alpha: bool,
    /// Planar RGB (`Gbrp*` / `Gbrap*`).
    planar_rgb: bool,
    /// Packed: `(bytes per pixel, channel order of R, G, B, A | Y, A)`.
    packed: Option<(usize, [usize; 4])>,
    /// Packed grey + alpha.
    grey_packed: bool,
}

const fn planar(pf: Pf, chroma: Chroma, depth: u8, alpha: bool) -> Layout {
    Layout {
        pf,
        chroma,
        depth,
        alpha,
        planar_rgb: false,
        packed: None,
        grey_packed: false,
    }
}

/// The framework's full-range 8-bit labels: the same planes.
const fn jplanar(pf: Pf, chroma: Chroma) -> Layout {
    planar(pf, chroma, 8, false)
}

const fn gbr(pf: Pf, depth: u8, alpha: bool) -> Layout {
    Layout {
        planar_rgb: true,
        ..planar(pf, Chroma::Yuv444, depth, alpha)
    }
}

const fn packed(pf: Pf, bpp: usize, order: [usize; 4], depth: u8, alpha: bool) -> Layout {
    Layout {
        packed: Some((bpp, order)),
        ..planar(pf, Chroma::Yuv444, depth, alpha)
    }
}

const fn ya(pf: Pf, depth: u8) -> Layout {
    Layout {
        packed: Some((if depth == 8 { 2 } else { 4 }, [0, 0, 0, 1])),
        grey_packed: true,
        ..planar(pf, Chroma::Mono, depth, true)
    }
}

/// Every layout the gateway can hand the encoder.
const LAYOUTS: &[Layout] = &[
    planar(Pf::Gray8, Chroma::Mono, 8, false),
    planar(Pf::Gray10Le, Chroma::Mono, 10, false),
    planar(Pf::Gray12Le, Chroma::Mono, 12, false),
    planar(Pf::Gray16Le, Chroma::Mono, 16, false),
    planar(Pf::Yuv420P, Chroma::Yuv420, 8, false),
    jplanar(Pf::YuvJ420P, Chroma::Yuv420),
    planar(Pf::Yuv420P10Le, Chroma::Yuv420, 10, false),
    planar(Pf::Yuv420P12Le, Chroma::Yuv420, 12, false),
    planar(Pf::Yuv420P16Le, Chroma::Yuv420, 16, false),
    planar(Pf::Yuva420P, Chroma::Yuv420, 8, true),
    planar(Pf::Yuva420P10Le, Chroma::Yuv420, 10, true),
    planar(Pf::Yuv422P, Chroma::Yuv422, 8, false),
    jplanar(Pf::YuvJ422P, Chroma::Yuv422),
    planar(Pf::Yuv422P10Le, Chroma::Yuv422, 10, false),
    planar(Pf::Yuv422P12Le, Chroma::Yuv422, 12, false),
    planar(Pf::Yuv422P16Le, Chroma::Yuv422, 16, false),
    planar(Pf::Yuva422P, Chroma::Yuv422, 8, true),
    planar(Pf::Yuva422P10Le, Chroma::Yuv422, 10, true),
    planar(Pf::Yuva422P12Le, Chroma::Yuv422, 12, true),
    planar(Pf::Yuva422P16Le, Chroma::Yuv422, 16, true),
    planar(Pf::Yuv444P, Chroma::Yuv444, 8, false),
    jplanar(Pf::YuvJ444P, Chroma::Yuv444),
    planar(Pf::Yuv444P10Le, Chroma::Yuv444, 10, false),
    planar(Pf::Yuv444P12Le, Chroma::Yuv444, 12, false),
    planar(Pf::Yuv444P16Le, Chroma::Yuv444, 16, false),
    planar(Pf::Yuva444P, Chroma::Yuv444, 8, true),
    planar(Pf::Yuva444P10Le, Chroma::Yuv444, 10, true),
    planar(Pf::Yuva444P12Le, Chroma::Yuv444, 12, true),
    planar(Pf::Yuva444P16Le, Chroma::Yuv444, 16, true),
    gbr(Pf::Gbrp8, 8, false),
    gbr(Pf::Gbrap8, 8, true),
    gbr(Pf::Gbrp10Le, 10, false),
    gbr(Pf::Gbrap10Le, 10, true),
    gbr(Pf::Gbrp12Le, 12, false),
    gbr(Pf::Gbrap12Le, 12, true),
    gbr(Pf::Gbrp14Le, 14, false),
    gbr(Pf::Gbrap14Le, 14, true),
    gbr(Pf::Gbrp16Le, 16, false),
    gbr(Pf::Gbrap16Le, 16, true),
    packed(Pf::Rgb24, 3, [0, 1, 2, 0], 8, false),
    packed(Pf::Rgba, 4, [0, 1, 2, 3], 8, true),
    packed(Pf::Bgr24, 3, [2, 1, 0, 0], 8, false),
    packed(Pf::Bgra, 4, [2, 1, 0, 3], 8, true),
    packed(Pf::Rgb48Le, 6, [0, 1, 2, 0], 16, false),
    packed(Pf::Rgba64Le, 8, [0, 1, 2, 3], 16, true),
    ya(Pf::Ya8, 8),
    ya(Pf::Ya16Le, 16),
];

fn layout_of(pf: Pf) -> Layout {
    *LAYOUTS.iter().find(|l| l.pf == pf).unwrap()
}

// ---------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------

/// The picture every cell codes, as `[r, g, b, a]` in 16-bit scale at
/// `(x, y)`: smooth gradients, constant over 2×2 blocks (so chroma
/// subsampling and the 4:2:0 codings lose nothing of the pattern) and
/// a diagonal hash so the coders have texture.
fn rgba16(x: u32, y: u32, w: u32, h: u32) -> [u16; 4] {
    let (bx, by) = (x / 2, y / 2);
    let fx = bx as f64 / (w.div_ceil(2)).max(1) as f64;
    let fy = by as f64 / (h.div_ceil(2)).max(1) as f64;
    let hash =
        (((bx.wrapping_mul(2654435761) ^ by.wrapping_mul(40503)) >> 9) & 0xff) as f64 / 255.0;
    let r = (fx * 0.8 + hash * 0.2) * 65535.0;
    let g = (fy * 0.8 + 0.1) * 65535.0;
    let b = (((fx + fy) * 0.5) * 0.7 + 0.15) * 65535.0;
    let a = (0.25 + 0.75 * fy) * 65535.0;
    [r as u16, g as u16, b as u16, a as u16]
}

/// `[y, cb, cr, a]` in 16-bit scale: the same gradients as YCbCr planes
/// (not a conversion of `rgba16`; a YCbCr source is its own truth).
fn ycbcra16(x: u32, y: u32, w: u32, h: u32) -> [u16; 4] {
    let [r, g, b, a] = rgba16(x, y, w, h);
    let y_ = (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000;
    let cb = 32768 + (b as i32 - y_ as i32) / 4;
    let cr = 32768 + (r as i32 - y_ as i32) / 4;
    [
        y_ as u16,
        cb.clamp(0, 65535) as u16,
        cr.clamp(0, 65535) as u16,
        a,
    ]
}

/// The source frame of a layout plus the contract image of the same
/// picture (`None` when the contract has no such layout) and a
/// *truth* image whose `to_rgba8` is the expected decode (built
/// planar 16-bit RGB for the packed-16 / grey+alpha layouts).
fn source(l: Layout, w: u32, h: u32) -> (VideoFrame, Option<HeifImage>, HeifImage) {
    let to_depth = |v: u16, d: u8| -> u16 {
        if d == 16 {
            v
        } else {
            v >> (16 - d)
        }
    };
    if let Some((bpp, order)) = l.packed {
        let wide = l.depth == 16;
        let stride = w as usize * bpp + 7; // padded rows
        let mut data = vec![0u8; stride * h as usize];
        // Planar 16-bit RGB(A) truth (G, B, R, A planes).
        let tfmt = HeifPixelFormat::new(Chroma::Yuv444, 16, l.alpha).unwrap();
        let mut truth = HeifFrame::zeroed(w, h, tfmt).unwrap();
        // The contract-equivalent packed image for the 8-bit RGB layouts.
        let mut rgb8 = Vec::with_capacity(w as usize * h as usize * 4);
        for y in 0..h {
            for x in 0..w {
                let [r, g, b, a] = rgba16(x, y, w, h);
                let grey = ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u16;
                let at = y as usize * stride + x as usize * bpp;
                let chans: Vec<(usize, u16)> = if l.grey_packed {
                    vec![(order[0], grey), (order[3], a)]
                } else if l.alpha {
                    vec![(order[0], r), (order[1], g), (order[2], b), (order[3], a)]
                } else {
                    vec![(order[0], r), (order[1], g), (order[2], b)]
                };
                for (i, v) in &chans {
                    if wide {
                        data[at + 2 * i..at + 2 * i + 2].copy_from_slice(&v.to_le_bytes());
                    } else {
                        data[at + i] = (*v >> 8) as u8;
                    }
                }
                let (tr, tg, tb) = if l.grey_packed {
                    (grey, grey, grey)
                } else {
                    (r, g, b)
                };
                // 8-bit packed sources are exactly their top byte.
                let q = |v: u16| if wide { v } else { (v >> 8) * 257 };
                truth.set_sample(0, x, y, q(tg));
                truth.set_sample(1, x, y, q(tb));
                truth.set_sample(2, x, y, q(tr));
                if l.alpha {
                    truth.set_sample(3, x, y, q(a));
                }
                if !wide && !l.grey_packed {
                    rgb8.extend_from_slice(&[(r >> 8) as u8, (g >> 8) as u8, (b >> 8) as u8]);
                    if l.alpha {
                        rgb8.push((a >> 8) as u8);
                    }
                }
            }
        }
        let contract = if !wide && !l.grey_packed {
            Some(if l.alpha {
                HeifImage::from_rgba8(w, h, rgb8).unwrap()
            } else {
                HeifImage::from_rgb8(w, h, rgb8).unwrap()
            })
        } else {
            None
        };
        let truth_img = HeifImage::from_frame(
            truth,
            ColorInfo::new(oxideav_heif::ColorRange::Full, 1, 13, 0),
            Default::default(),
        );
        let vf = VideoFrame {
            pts: Some(0),
            planes: vec![VideoPlane { stride, data }],
        };
        return (vf, contract, truth_img);
    }
    let fmt = HeifPixelFormat::new(l.chroma, l.depth, l.alpha).unwrap();
    let mut f = HeifFrame::zeroed(w, h, fmt).unwrap();
    for p in 0..fmt.plane_count() {
        let (pw, ph) = f.plane_dims(p);
        let (sx, sy) = if p == 0 || Some(p) == fmt.alpha_plane() {
            (0, 0)
        } else {
            l.chroma.shift()
        };
        for py in 0..ph {
            for px in 0..pw {
                let (x, y) = ((px << sx).min(w - 1), (py << sy).min(h - 1));
                let v = if l.planar_rgb {
                    let [r, g, b, a] = rgba16(x, y, w, h);
                    [g, b, r, a][p]
                } else if l.chroma == Chroma::Mono {
                    let [y_, _, _, a] = ycbcra16(x, y, w, h);
                    if p == 0 {
                        y_
                    } else {
                        a
                    }
                } else {
                    ycbcra16(x, y, w, h)[p]
                };
                f.set_sample(p, px, py, to_depth(v, l.depth));
            }
        }
    }
    let planes = f
        .planes
        .iter()
        .map(|p| VideoPlane {
            stride: p.stride,
            data: p.data.clone(),
        })
        .collect();
    let vf = VideoFrame {
        pts: Some(0),
        planes,
    };
    let hp = Hp::try_from(l.pf).unwrap();
    let img = HeifImage::new(
        w,
        h,
        hp,
        f.planes
            .iter()
            .map(|p| Plane::new(p.stride, p.data.clone()))
            .collect(),
    )
    .unwrap();
    let truth = if l.planar_rgb {
        img.clone()
            .with_color(ColorInfo::new(oxideav_heif::ColorRange::Full, 1, 13, 0))
    } else {
        img.clone()
    };
    (vf, Some(img), truth)
}

// ---------------------------------------------------------------------
// Cells and expectations
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Cell {
    tag: String,
    pf: Pf,
    w: u32,
    h: u32,
    opts: Vec<(&'static str, String)>,
}

impl Cell {
    fn new(tag: &str, pf: Pf, w: u32, h: u32, opts: &[(&'static str, &str)]) -> Self {
        Self {
            tag: tag.to_string(),
            pf,
            w,
            h,
            opts: opts.iter().map(|(k, v)| (*k, v.to_string())).collect(),
        }
    }
    fn opt(&self, k: &str) -> Option<&str> {
        self.opts
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.as_str())
    }
    fn codec(&self) -> &str {
        self.opt("codec").unwrap_or("hevc")
    }
    fn mode(&self) -> &str {
        self.opt("mode").unwrap_or("intra")
    }
    fn codec_options(&self) -> CodecOptions {
        let mut o = CodecOptions::new();
        for (k, v) in &self.opts {
            // `exec_threads` is the test's own: the execution context
            // handed to the encoder, not an option.
            if *k != "exec_threads" {
                o = o.set(*k, v.as_str());
            }
        }
        o
    }
    /// The PSNR floor of a lossy cell: 30 dB at the defaults, 20 dB
    /// when the cell asks for a coarse quantiser, 24 dB for the VVC
    /// encoder (an IDR encoder without rate-distortion search).
    fn psnr_floor(&self) -> f64 {
        let coarse = self
            .opt("qp")
            .map(|q| q.parse::<u32>().unwrap() >= 35)
            .unwrap_or(false)
            || self
                .opt("quality")
                .map(|q| q.parse::<u32>().unwrap() <= 10)
                .unwrap_or(false);
        if coarse {
            20.0
        } else if matches!(self.codec(), "vvc" | "h266") {
            24.0
        } else {
            30.0
        }
    }
    fn params(&self) -> CodecParameters {
        let mut p = CodecParameters::video(CodecId::new("heif"));
        p.width = Some(self.w);
        p.height = Some(self.h);
        p.pixel_format = Some(self.pf);
        p.options = self.codec_options();
        p
    }
    fn encode_options(&self) -> EncodeOptions {
        oxideav_core::parse_options::<HeifEncoderOptions>(&self.codec_options())
            .unwrap()
            .to_encode_options()
            .unwrap()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Expect {
    /// A typed refusal whose message contains the text.
    Refuse(&'static str),
    /// The label the demuxer / decoder emit, and whether the coding is
    /// lossless (the decode must then equal the source exactly).
    Ok { label: Pf, lossless: bool },
}

fn j_variant(pf: Pf) -> Pf {
    match pf {
        Pf::Yuv420P => Pf::YuvJ420P,
        Pf::Yuv422P => Pf::YuvJ422P,
        Pf::Yuv444P => Pf::YuvJ444P,
        other => other,
    }
}

/// The framework label of a coded layout plus alpha: the crate's own
/// mapping, with its 4:4:4 promotion of layouts the framework lacks.
fn label(coded: HeifPixelFormat, identity: bool, full: bool) -> Pf {
    if identity {
        return coded.to_core_gbr().unwrap();
    }
    let pf = coded.to_core().unwrap_or_else(|| {
        HeifPixelFormat {
            chroma: Chroma::Yuv444,
            ..coded
        }
        .to_core()
        .unwrap()
    });
    if full && !coded.has_alpha {
        j_variant(pf)
    } else {
        pf
    }
}

/// The coding rule of the crate — the README's layout tables as code.
fn expect(c: &Cell) -> Expect {
    let l = layout_of(c.pf);
    let codec = c.codec();
    let mode = c.mode();
    let depth_opt = c.opt("depth").map(|d| d.parse::<u8>().unwrap());
    let coded_d = |src: u8| coded_depth(src, depth_opt).unwrap();
    let full = c.opt("range") != Some("limited");
    let quality = c.opt("quality").map(|q| q.parse::<u8>().unwrap());
    let chroma_opt = if c.opt("chroma") == Some("444") {
        Chroma::Yuv444
    } else {
        Chroma::Yuv420
    };
    let lossless_mode = match codec {
        "hevc" | "h265" | "avc" | "h264" | "vvc" | "h266" => mode == "pcm",
        "av1" => mode == "pcm" || quality == Some(100),
        _ => false,
    };
    let rgb8 = l.packed.is_some() && !l.grey_packed && l.depth == 8;
    let (chroma, depth, identity, lossless): (Chroma, u8, bool, bool) = match codec {
        "hevc" | "h265" | "av1" | "avc" | "h264" | "vvc" | "h266" => {
            let av1 = codec == "av1";
            let avc = matches!(codec, "avc" | "h264");
            let vvc = matches!(codec, "vvc" | "h266");
            // VVC codes 8 – 10 bits: deeper sources at 10, an explicit
            // deeper `depth` refused.
            if vvc && depth_opt.map(|d| d > 10).unwrap_or(false) {
                return Expect::Refuse("8 – 10-bit");
            }
            let d = if vvc {
                coded_d(l.depth).min(10)
            } else {
                coded_d(l.depth)
            };
            if avc && (l.chroma == Chroma::Mono && l.packed.is_none() || l.grey_packed) {
                // The AVC registry encoder codes grey input as 4:2:0 with
                // neutral chroma: the item is a 4:2:0 item (never exact
                // against a grey source, whose layout it does not keep).
                (Chroma::Yuv420, d, false, false)
            } else if lossless_mode && full && (rgb8 || (l.planar_rgb && l.depth <= 12)) {
                // Lossless RGB: the G, B, R planes as an identity item.
                (Chroma::Yuv444, d, true, d == l.depth)
            } else if av1 && l.planar_rgb && l.depth <= 12 {
                // AV1 keeps planar RGB as an identity item, lossy too.
                (Chroma::Yuv444, d, true, false)
            } else if l.grey_packed {
                (Chroma::Mono, d, false, lossless_mode && d == l.depth)
            } else if l.packed.is_some() {
                // Packed RGB converts into `chroma` (4:2:0 by default).
                (chroma_opt, d, false, false)
            } else if l.planar_rgb {
                // Converted to YCbCr: AV1 at 4:4:4, HEVC into `chroma`.
                (
                    if av1 { Chroma::Yuv444 } else { chroma_opt },
                    d,
                    false,
                    false,
                )
            } else {
                // Planar YCbCr / grey keeps its sampling; lossless when
                // the depth is kept too.
                (l.chroma, d, false, lossless_mode && d == l.depth)
            }
        }
        _ => return Expect::Refuse("unknown codec"),
    };
    let mut coded = HeifPixelFormat::new(chroma, depth, l.alpha).unwrap();
    // Odd geometry: a 4:2:0 / 4:2:2 item cropped by `clap` to an odd
    // size is composed as 4:4:4.
    if oxideav_heif::compose::needs_444(coded.chroma, c.w % 2 == 1, c.h % 2 == 1) {
        coded.chroma = Chroma::Yuv444;
    }
    Expect::Ok {
        label: label(coded, identity, full),
        lossless,
    }
}

// ---------------------------------------------------------------------
// One cell
// ---------------------------------------------------------------------

struct Outcome {
    bytes: Vec<u8>,
    label: Pf,
    psnr: f64,
    identical_to_contract: Option<bool>,
    heif_info: Option<bool>,
    sips: Option<bool>,
    /// AVC cells: every `avc1` item of the file decodes byte-identically
    /// in this crate's AVC decoder and in the black-box reference
    /// decoder (`None` without the binary / on other codecs).
    avc_black_box: Option<bool>,
}

/// Every `avc1` item of `bytes` through oxideav-h264's registry decoder
/// (the item's `avcC` as extradata, its length-prefixed payload as the
/// packet) and through the black-box reference decoder over the Annex B
/// reconstruction (parameter sets + NAL units with start codes), at the
/// coded (macroblock-aligned) size: the planes must be byte-identical.
fn avc_items_match_black_box(bytes: &[u8], dir: &std::path::Path, tag: &str) -> bool {
    use oxideav_heif::hvcc::split_length_prefixed;
    let f = HeifFile::parse(bytes).unwrap();
    let meta = f.meta().unwrap();
    let mut n = 0usize;
    for it in &meta.items {
        if &it.item_type != b"avc1" {
            continue;
        }
        let props = oxideav_heif::props::ItemProperties::resolve(meta, it.id).unwrap();
        let cfg = props.avcc().unwrap();
        let data = f.item_data_owned(it.id).unwrap();
        let mut annex_b = Vec::new();
        for nal in cfg
            .sps
            .iter()
            .chain(cfg.pps.iter())
            .map(|v| v.as_slice())
            .chain(split_length_prefixed(&data, cfg.length_size).unwrap())
        {
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(nal);
        }
        // Ours.
        let layout = cfg.layout().unwrap();
        let mut params = CodecParameters::video(CodecId::new("h264"));
        params.extradata = cfg.raw.clone();
        params.pixel_format = layout.to_core();
        let mut dec = oxideav_h264::h264_decoder::make_decoder(&params).unwrap();
        let pkt = oxideav_core::Packet::new(0, TimeBase::new(1, 1), data)
            .with_pts(0)
            .with_keyframe(true);
        dec.send_packet(&pkt)
            .unwrap_or_else(|e| panic!("{tag}: item {}: our decoder: {e}", it.id));
        dec.flush().unwrap();
        let Ok(Frame::Video(ours)) = dec.receive_frame() else {
            panic!("{tag}: item {}: our decoder yielded no picture", it.id)
        };
        // Theirs.
        let (pix_fmt, bps) = match (layout.chroma, layout.bit_depth) {
            (Chroma::Yuv420, 8) => ("yuv420p", 1),
            (Chroma::Yuv422, 8) => ("yuv422p", 1),
            (Chroma::Yuv444, 8) => ("yuv444p", 1),
            (Chroma::Yuv420, 10) => ("yuv420p10le", 2),
            (Chroma::Yuv422, 10) => ("yuv422p10le", 2),
            (Chroma::Yuv444, 10) => ("yuv444p10le", 2),
            (Chroma::Yuv420, 12) => ("yuv420p12le", 2),
            (Chroma::Yuv422, 12) => ("yuv422p12le", 2),
            (Chroma::Yuv444, 12) => ("yuv444p12le", 2),
            other => panic!("{tag}: item {}: layout {other:?}", it.id),
        };
        let h264 = dir.join(format!(
            "{}_{}.h264",
            tag.replace(['/', ' ', ':'], "_"),
            it.id
        ));
        let raw = dir.join(format!(
            "{}_{}.raw",
            tag.replace(['/', ' ', ':'], "_"),
            it.id
        ));
        std::fs::write(&h264, &annex_b).unwrap();
        let st = Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
            .arg(&h264)
            .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", pix_fmt])
            .arg(&raw)
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "{tag}: item {}: the black-box decoder refused the stream: {}",
            it.id,
            String::from_utf8_lossy(&st.stderr)
        );
        let theirs = std::fs::read(&raw).unwrap();
        // The coded picture size: the SPS carries no cropping (this
        // crate pads to 16 and crops with `clap`), so both decoders
        // emit the macroblock-aligned picture — the `ispe` of the item.
        let (cw, ch) = props.ispe().map(|i| (i.width, i.height)).unwrap();
        let mut mine = Vec::with_capacity(theirs.len());
        let (sx, sy) = layout.chroma.shift();
        for (p, plane) in ours.image_planes().iter().enumerate() {
            let (pw, ph) = if p == 0 {
                (cw, ch)
            } else {
                (cw.div_ceil(1 << sx), ch.div_ceil(1 << sy))
            };
            for y in 0..ph as usize {
                let row = &plane.data[y * plane.stride..y * plane.stride + pw as usize * bps];
                mine.extend_from_slice(row);
            }
        }
        if mine != theirs {
            eprintln!(
                "{tag}: item {}: {} bytes ours vs {} the black-box decoder's differ",
                it.id,
                mine.len(),
                theirs.len()
            );
            return false;
        }
        n += 1;
    }
    assert!(n > 0, "{tag}: no avc1 item");
    true
}

fn encode_framework(ctx: &RuntimeContext, c: &Cell, vf: &VideoFrame) -> Result<Vec<u8>, String> {
    let mut enc = ctx
        .codecs
        .first_encoder(&c.params())
        .map_err(|e| e.to_string())?;
    if let Some(t) = c.opt("exec_threads") {
        enc.set_execution_context(&oxideav_core::ExecutionContext::with_threads(
            t.parse().unwrap(),
        ));
    }
    enc.send_frame(&Frame::Video(vf.clone()))
        .map_err(|e| e.to_string())?;
    enc.flush().map_err(|e| e.to_string())?;
    let pkt = enc.receive_packet().map_err(|e| e.to_string())?;
    assert_eq!(pkt.pts, Some(0), "{}: packet pts", c.tag);
    assert!(pkt.is_keyframe(), "{}: keyframe", c.tag);
    assert_eq!(
        pkt.time_base,
        TimeBase::new(1, 1),
        "{}: packet time base",
        c.tag
    );
    assert_eq!(pkt.duration, Some(1), "{}: packet duration", c.tag);
    assert!(
        matches!(enc.receive_packet(), Err(oxideav_core::Error::Eof)),
        "{}: one packet per frame",
        c.tag
    );
    Ok(pkt.data)
}

fn psnr_rgba8(a: &[u8], b: &[u8], alpha: bool) -> f64 {
    assert_eq!(a.len(), b.len());
    let mut se = 0f64;
    let mut n = 0usize;
    let ch = if alpha { 4 } else { 3 };
    for (pa, pb) in a.chunks(4).zip(b.chunks(4)) {
        for (x, y) in pa.iter().zip(pb).take(ch) {
            let d = *x as f64 - *y as f64;
            se += d * d;
            n += 1;
        }
    }
    if se == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 * n as f64 / se).log10()
}

fn run_cell(ctx: &RuntimeContext, c: &Cell, dir: &std::path::Path) -> Result<Outcome, String> {
    let l = layout_of(c.pf);
    let (vf, contract_img, truth) = source(l, c.w, c.h);
    let want = expect(c);
    let bytes = match (encode_framework(ctx, c, &vf), &want) {
        (Err(e), Expect::Refuse(text)) => {
            assert!(
                e.contains(text),
                "{}: refused for another reason: {e}",
                c.tag
            );
            return Err(e);
        }
        (Err(e), Expect::Ok { .. }) => panic!("{}: refused: {e}", c.tag),
        (Ok(_), Expect::Refuse(text)) => {
            panic!(
                "{}: accepted, expected a refusal mentioning '{text}'",
                c.tag
            )
        }
        (Ok(b), Expect::Ok { .. }) => b,
    };
    let Expect::Ok {
        label: want_label,
        lossless,
    } = want
    else {
        unreachable!()
    };
    // 2. Framework demuxer + decoder.
    let mut demuxer = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes.clone())), &ctx.codecs)
        .unwrap_or_else(|e| panic!("{}: open: {e}", c.tag));
    assert_eq!(demuxer.streams().len(), 1, "{}: one still stream", c.tag);
    let info = demuxer.streams()[0].clone();
    assert_eq!(info.time_base, TimeBase::new(1, 1), "{}", c.tag);
    assert_eq!(info.duration, Some(1), "{}", c.tag);
    assert_eq!(
        (info.params.width, info.params.height),
        (Some(c.w), Some(c.h)),
        "{}",
        c.tag
    );
    assert_eq!(
        info.params.pixel_format,
        Some(want_label),
        "{}: stream label",
        c.tag
    );
    let primary = HeifFile::parse(&bytes).unwrap().primary_item().unwrap().id;
    let ids = demuxer
        .metadata()
        .iter()
        .find(|(k, _)| k == "stream:0:item_ids")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(ids, primary.to_string(), "{}: item ids", c.tag);
    let pkt = demuxer.next_packet().unwrap();
    assert_eq!(
        (pkt.pts, pkt.dts, pkt.duration),
        (Some(0), Some(0), Some(1)),
        "{}",
        c.tag
    );
    assert!(matches!(
        demuxer.next_packet(),
        Err(oxideav_core::Error::Eof)
    ));
    let mut dec = ctx.codecs.first_decoder(&info.params).unwrap();
    dec.send_packet(&pkt)
        .unwrap_or_else(|e| panic!("{}: decode: {e}", c.tag));
    let Frame::Video(frame) = dec.receive_frame().unwrap() else {
        panic!("{}: video frame", c.tag)
    };
    assert_eq!(frame.pts, Some(0), "{}", c.tag);
    assert!(matches!(
        dec.receive_frame(),
        Err(oxideav_core::Error::NeedMore)
    ));
    // 3. Contract decode: same label, identical planes.
    let img = oxideav_heif::decode(&bytes).unwrap_or_else(|e| panic!("{}: decode(): {e}", c.tag));
    assert_eq!((img.width, img.height), (c.w, c.h), "{}", c.tag);
    let (cvf, cpf) = img.clone().into_video_frame();
    assert_eq!(cpf, want_label, "{}: contract label", c.tag);
    assert_eq!(
        frame.image_planes().len(),
        cvf.image_planes().len(),
        "{}: plane count",
        c.tag
    );
    for (i, (a, b)) in frame
        .image_planes()
        .iter()
        .zip(cvf.image_planes())
        .enumerate()
    {
        assert_eq!(a.stride, b.stride, "{}: plane {i} stride", c.tag);
        assert!(a.data == b.data, "{}: plane {i} bytes differ", c.tag);
    }
    assert_eq!(
        frame.color_signal(),
        cvf.color_signal(),
        "{}: colour signal",
        c.tag
    );
    // 4. Byte identity with the contract-path encode.
    let identical_to_contract = contract_img.as_ref().map(|ci| {
        let o = c.encode_options();
        // The framework describes a frame without a colour signal as
        // unspecified, with `range` pinned when given explicitly.
        let mut color = ColorInfo::unspecified();
        if let Some(r) = c.opt("range") {
            color.range = if r == "limited" {
                oxideav_heif::ColorRange::Limited
            } else {
                oxideav_heif::ColorRange::Full
            };
        }
        let cbytes = oxideav_heif::encode(&ci.clone().with_color(color), &o)
            .unwrap_or_else(|e| panic!("{}: contract encode: {e}", c.tag));
        cbytes == bytes
    });
    // 5. Fidelity.
    let mut t = truth.clone();
    if !l.planar_rgb && l.packed.is_none() {
        // A YCbCr source is read back through the file's colour
        // description.
        t.color = img.color;
    }
    let psnr = psnr_rgba8(&img.to_rgba8(), &t.to_rgba8(), l.alpha);
    if lossless {
        // Exact: the decoded planes are the source planes.
        let src = t.to_frame().unwrap();
        let got = img.to_frame().unwrap();
        if l.packed.is_some() {
            assert_eq!(
                img.to_rgba8(),
                t.to_rgba8(),
                "{}: lossless RGB exact",
                c.tag
            );
        } else {
            // The same layout, or the 4:4:4 promotion of an odd-sized
            // picture (chroma replicated from its co-sited sample).
            assert_eq!(
                got.format.bit_depth, src.format.bit_depth,
                "{}: depth",
                c.tag
            );
            assert_eq!(
                got.format.has_alpha, src.format.has_alpha,
                "{}: alpha",
                c.tag
            );
            assert!(
                got.format.chroma == src.format.chroma || got.format.chroma == Chroma::Yuv444,
                "{}: lossless layout {:?} vs {:?}",
                c.tag,
                got.format,
                src.format
            );
            for p in 0..src.format.plane_count() {
                let (pw, ph) = got.plane_dims(p);
                let chroma_plane = p != 0 && Some(p) != src.format.alpha_plane();
                let (gsx, gsy) = if chroma_plane {
                    got.format.chroma.shift()
                } else {
                    (0, 0)
                };
                let (ssx, ssy) = if chroma_plane {
                    src.format.chroma.shift()
                } else {
                    (0, 0)
                };
                for y in 0..ph {
                    for x in 0..pw {
                        let (sx, sy) = ((x << gsx) >> ssx, (y << gsy) >> ssy);
                        assert_eq!(
                            got.sample(p, x, y),
                            src.sample(p, sx, sy),
                            "{}: plane {p} ({x},{y}) lossless",
                            c.tag
                        );
                    }
                }
            }
        }
    } else {
        let floor = c.psnr_floor();
        assert!(psnr >= floor, "{}: {psnr:.2} dB (floor {floor})", c.tag);
    }
    // 6. Black-box readers.
    let ext = match c.codec() {
        "av1" => "avif",
        _ => "heic",
    };
    let path = dir.join(format!("{}.{ext}", c.tag.replace(['/', ' ', ':'], "_")));
    std::fs::write(&path, &bytes).unwrap();
    let heif_info = have_binary("heif-info").then(|| {
        let out = Command::new("heif-info").arg(&path).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        out.status.success() && text.contains(&format!("{}x{}", c.w, c.h))
    });
    let sips = (have_binary("sips") && c.codec() != "vvc").then(|| {
        let out = Command::new("sips")
            .args(["-g", "pixelWidth", "-g", "pixelHeight"])
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        out.status.success()
            && text.contains(&format!("pixelWidth: {}", c.w))
            && text.contains(&format!("pixelHeight: {}", c.h))
    });
    let avc_black_box = (matches!(c.codec(), "avc" | "h264") && have_binary("ffmpeg"))
        .then(|| avc_items_match_black_box(&bytes, dir, &c.tag));
    Ok(Outcome {
        bytes,
        label: want_label,
        psnr,
        identical_to_contract,
        heif_info,
        sips,
        avc_black_box,
    })
}

// ---------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------

fn cells() -> Vec<Cell> {
    let mut v = Vec::new();
    let codec_modes: &[(&str, &str)] = &[
        ("hevc", "intra"),
        ("hevc", "pcm"),
        ("av1", "intra"),
        ("av1", "pcm"),
        ("avc", "intra"),
        ("avc", "pcm"),
    ];
    // Every layout × every codec / mode at a block-aligned size.
    for l in LAYOUTS {
        for &(codec, mode) in codec_modes {
            v.push(Cell::new(
                &format!("{codec}/{mode} {:?} 48x32", l.pf),
                l.pf,
                48,
                32,
                &[("codec", codec), ("mode", mode)],
            ));
        }
    }
    // VVC: one cell per layout family (the encoder is slow).
    for pf in [
        Pf::Gray8,
        Pf::Yuv420P,
        Pf::Yuv420P10Le,
        Pf::Yuva420P,
        Pf::Yuv444P,
        Pf::Gbrp8,
        Pf::Rgb24,
        Pf::Rgba,
        Pf::Rgb48Le,
        Pf::Ya8,
    ] {
        v.push(Cell::new(
            &format!("vvc/intra {pf:?} 32x32"),
            pf,
            32,
            32,
            &[("codec", "vvc")],
        ));
    }
    for pf in [
        Pf::Yuv420P,
        Pf::Gray8,
        Pf::Yuv444P,
        Pf::Yuv420P10Le,
        Pf::Gbrp8,
        Pf::Rgb24,
        Pf::Rgba,
        Pf::Ya8,
    ] {
        v.push(Cell::new(
            &format!("vvc/pcm {pf:?} 32x32"),
            pf,
            32,
            32,
            &[("codec", "vvc"), ("mode", "pcm")],
        ));
    }
    for (name, o) in [
        ("depth10", &[("depth", "10")][..]),
        ("depth12", &[("depth", "12")][..]),
        ("chroma444", &[("chroma", "444")][..]),
        ("pcm+chroma444", &[("mode", "pcm"), ("chroma", "444")][..]),
        ("limited", &[("range", "limited")][..]),
        ("pcm+limited", &[("mode", "pcm"), ("range", "limited")][..]),
    ] {
        let mut opts: Vec<(&str, &str)> = vec![("codec", "vvc")];
        opts.extend_from_slice(o);
        v.push(Cell::new(
            &format!("vvc {name} Rgb24 32x32"),
            Pf::Rgb24,
            32,
            32,
            &opts,
        ));
    }
    // Sizes: 1×1, odd, unaligned, through every codec.
    for (w, h) in [(1u32, 1u32), (7, 5), (65, 33), (2, 2)] {
        for &(codec, mode) in codec_modes
            .iter()
            .chain([("vvc", "intra"), ("vvc", "pcm")].iter())
        {
            for pf in [Pf::Rgb24, Pf::Rgba, Pf::Gray8, Pf::Yuv420P, Pf::Yuv444P10Le] {
                if codec == "vvc" && pf != Pf::Rgb24 && pf != Pf::Yuv420P {
                    continue;
                }
                v.push(Cell::new(
                    &format!("{codec}/{mode} {pf:?} {w}x{h}"),
                    pf,
                    w,
                    h,
                    &[("codec", codec), ("mode", mode)],
                ));
            }
        }
    }
    // Options.
    let hevc_opts: &[(&str, &[(&str, &str)])] = &[
        ("qp4", &[("qp", "4")]),
        ("qp40", &[("qp", "40")]),
        ("thumbnail16", &[("thumbnail", "16")]),
        ("limited", &[("range", "limited")]),
        ("pcm+limited", &[("mode", "pcm"), ("range", "limited")]),
        ("rd0", &[("rd", "0")]),
        ("rd1", &[("rd", "1")]),
        ("rd2", &[("rd", "2")]),
        ("tiles2x1", &[("tiles", "2x1")]),
        ("ctb16", &[("ctb", "16")]),
        ("ctb32", &[("ctb", "32")]),
        ("ctb64", &[("ctb", "64")]),
        ("threads2", &[("threads", "2")]),
        ("threads-auto", &[("threads", "auto")]),
        ("exec2", &[("exec_threads", "2")]),
        ("depth8", &[("depth", "8")]),
        ("depth10", &[("depth", "10")]),
        ("depth12", &[("depth", "12")]),
        ("filters-off", &[("filters", "off")]),
        ("chroma444", &[("chroma", "444")]),
        ("pcm+chroma444", &[("mode", "pcm"), ("chroma", "444")]),
    ];
    for &(name, o) in hevc_opts {
        for pf in [Pf::Rgb24, Pf::Rgba, Pf::Yuv420P10Le, Pf::Gray8, Pf::Yuv444P] {
            let mut opts: Vec<(&str, &str)> = vec![("codec", "hevc")];
            opts.extend_from_slice(o);
            v.push(Cell::new(
                &format!("hevc {name} {pf:?} 48x32"),
                pf,
                48,
                32,
                &opts,
            ));
        }
    }
    let av1_opts: &[(&str, &[(&str, &str)])] = &[
        ("quality100", &[("quality", "100")]),
        ("quality0", &[("quality", "0")]),
        ("balanced", &[("speed", "balanced")]),
        ("thorough", &[("speed", "thorough")]),
        ("chroma444", &[("chroma", "444")]),
        ("depth10", &[("depth", "10")]),
        ("depth12", &[("depth", "12")]),
        ("thumbnail16", &[("thumbnail", "16")]),
        ("limited", &[("range", "limited")]),
        ("pcm+limited", &[("mode", "pcm"), ("range", "limited")]),
        ("threads2", &[("threads", "2")]),
    ];
    for &(name, o) in av1_opts {
        for pf in [Pf::Rgb24, Pf::Rgba, Pf::Yuv420P10Le, Pf::Gray8, Pf::Gbrp8] {
            let mut opts: Vec<(&str, &str)> = vec![("codec", "av1")];
            opts.extend_from_slice(o);
            v.push(Cell::new(
                &format!("av1 {name} {pf:?} 48x32"),
                pf,
                48,
                32,
                &opts,
            ));
        }
    }
    // AVC (round 476, oxideav-h264 0.1.9): every High-family layout,
    // lossless, CABAC, the depth / chroma / range knobs.
    let avc_opts: &[(&str, &[(&str, &str)])] = &[
        ("qp4", &[("qp", "4")]),
        ("qp30", &[("qp", "30")]),
        ("qp40", &[("qp", "40")]),
        ("thumbnail16", &[("thumbnail", "16")]),
        ("limited", &[("range", "limited")]),
        ("pcm+limited", &[("mode", "pcm"), ("range", "limited")]),
        ("cabac", &[("cabac", "1")]),
        ("pcm+cabac", &[("mode", "pcm"), ("cabac", "1")]),
        ("depth8", &[("depth", "8")]),
        ("depth10", &[("depth", "10")]),
        ("depth12", &[("depth", "12")]),
        ("chroma444", &[("chroma", "444")]),
        ("pcm+chroma444", &[("mode", "pcm"), ("chroma", "444")]),
        ("threads2", &[("threads", "2")]),
    ];
    for &(name, o) in avc_opts {
        for pf in [
            Pf::Rgb24,
            Pf::Rgba,
            Pf::Yuv420P10Le,
            Pf::Gray8,
            Pf::Yuv444P,
            Pf::Yuv422P12Le,
        ] {
            let mut opts: Vec<(&str, &str)> = vec![("codec", "avc")];
            opts.extend_from_slice(o);
            v.push(Cell::new(
                &format!("avc {name} {pf:?} 48x32"),
                pf,
                48,
                32,
                &opts,
            ));
        }
    }
    v.push(Cell::new(
        "vvc thumbnail16 Rgb24 32x32",
        Pf::Rgb24,
        32,
        32,
        &[("codec", "vvc"), ("thumbnail", "16")],
    ));
    v.push(Cell::new(
        "vvc qp40 Rgb24 32x32",
        Pf::Rgb24,
        32,
        32,
        &[("codec", "vvc"), ("qp", "40")],
    ));
    // Explicit grids (a 160×96 picture in 64-px tiles: 3×2), with and
    // without a thumbnail, with a thread budget, through every codec.
    for (codec, mode) in [
        ("hevc", "intra"),
        ("hevc", "pcm"),
        ("av1", "intra"),
        ("av1", "pcm"),
        ("avc", "intra"),
        ("avc", "pcm"),
    ] {
        for pf in [Pf::Rgb24, Pf::Rgba, Pf::Yuv420P] {
            v.push(Cell::new(
                &format!("{codec}/{mode} grid64 {pf:?} 160x96"),
                pf,
                160,
                96,
                &[("codec", codec), ("mode", mode), ("grid", "64")],
            ));
        }
        v.push(Cell::new(
            &format!("{codec}/{mode} grid64+thumb Rgba 160x96"),
            Pf::Rgba,
            160,
            96,
            &[
                ("codec", codec),
                ("mode", mode),
                ("grid", "64"),
                ("thumbnail", "40"),
            ],
        ));
        v.push(Cell::new(
            &format!("{codec}/{mode} grid64 threads4 Rgb24 161x97"),
            Pf::Rgb24,
            161,
            97,
            &[
                ("codec", codec),
                ("mode", mode),
                ("grid", "64"),
                ("threads", "4"),
            ],
        ));
        v.push(Cell::new(
            &format!("{codec}/{mode} grid none Rgb24 160x96"),
            Pf::Rgb24,
            160,
            96,
            &[("codec", codec), ("mode", mode), ("grid", "none")],
        ));
    }
    v
}

/// Cells whose bytes must equal another cell's (the thread budget
/// never changes a byte).
fn byte_twins() -> Vec<(String, String)> {
    let mut v = Vec::new();
    for pf in [Pf::Rgb24, Pf::Rgba, Pf::Yuv420P10Le, Pf::Gray8, Pf::Yuv444P] {
        for name in ["threads2", "threads-auto", "exec2"] {
            v.push((
                format!("hevc/intra {pf:?} 48x32"),
                format!("hevc {name} {pf:?} 48x32"),
            ));
        }
    }
    for pf in [Pf::Rgb24, Pf::Rgba, Pf::Yuv420P10Le, Pf::Gray8, Pf::Gbrp8] {
        v.push((
            format!("av1/intra {pf:?} 48x32"),
            format!("av1 threads2 {pf:?} 48x32"),
        ));
    }
    // `pcm` on AV1 is `quality=100`; `depth=8` on an 8-bit source is
    // the default; `rd 0` is the quadtree coder's default on the
    // layouts that need it.
    for pf in [Pf::Rgb24, Pf::Rgba, Pf::Gray8, Pf::Gbrp8] {
        v.push((
            format!("av1/pcm {pf:?} 48x32"),
            format!("av1 quality100 {pf:?} 48x32"),
        ));
    }
    for pf in [Pf::Rgb24, Pf::Rgba, Pf::Gray8] {
        v.push((
            format!("hevc/intra {pf:?} 48x32"),
            format!("hevc depth8 {pf:?} 48x32"),
        ));
    }
    v.push((
        "hevc/intra Yuv420P10Le 48x32".into(),
        "hevc rd0 Yuv420P10Le 48x32".into(),
    ));
    for pf in [Pf::Rgb24, Pf::Rgba, Pf::Gray8, Pf::Yuv444P] {
        v.push((
            format!("avc/intra {pf:?} 48x32"),
            format!("avc depth8 {pf:?} 48x32"),
        ));
    }
    for pf in [
        Pf::Rgb24,
        Pf::Rgba,
        Pf::Yuv420P10Le,
        Pf::Gray8,
        Pf::Yuv444P,
        Pf::Yuv422P12Le,
    ] {
        v.push((
            format!("avc/intra {pf:?} 48x32"),
            format!("avc threads2 {pf:?} 48x32"),
        ));
    }
    v
}

#[test]
fn framework_encoder_matrix() {
    let ctx = context();
    let dir = scratch();
    let cells = cells();
    let mut outcomes: BTreeMap<String, Outcome> = BTreeMap::new();
    let mut refused: Vec<(String, String)> = Vec::new();
    let mut lines = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    // Every cell is measured; the panics are collected and reported
    // together at the end (the hook is silenced meanwhile).
    std::panic::set_hook(Box::new(|_| {}));
    for c in &cells {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_cell(&ctx, c, &dir)));
        let r = match r {
            Ok(r) => r,
            Err(p) => {
                let msg = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "panic".into());
                lines.push(format!("{:<48} FAIL {msg}", c.tag));
                failed.push((c.tag.clone(), msg));
                continue;
            }
        };
        match r {
            Ok(o) => {
                lines.push(format!(
                    "{:<48} {:<14} {:>7} B {:>6} dB contract={} heif-info={} sips={}{}",
                    c.tag,
                    format!("{:?}", o.label),
                    o.bytes.len(),
                    if o.psnr.is_finite() {
                        format!("{:.2}", o.psnr)
                    } else {
                        "exact".into()
                    },
                    o.identical_to_contract
                        .map(|b| if b { "same" } else { "DIFF" })
                        .unwrap_or("n/a"),
                    o.heif_info.map(|b| b.to_string()).unwrap_or("-".into()),
                    o.sips.map(|b| b.to_string()).unwrap_or("-".into()),
                    o.avc_black_box
                        .map(|b| format!(" avc-black-box={b}"))
                        .unwrap_or_default(),
                ));
                outcomes.insert(c.tag.clone(), o);
            }
            Err(e) => {
                lines.push(format!("{:<48} REFUSED {e}", c.tag));
                refused.push((c.tag.clone(), e));
            }
        }
    }
    let _ = std::panic::take_hook();
    let report = lines.join("\n");
    eprintln!("{report}");
    std::fs::write(dir.join("framework_matrix.txt"), &report).unwrap();
    eprintln!(
        "framework matrix: {} cells, {} coded, {} typed refusals, {} failed",
        cells.len(),
        outcomes.len(),
        refused.len(),
        failed.len()
    );
    assert!(
        failed.is_empty(),
        "{} cells failed:\n{}",
        failed.len(),
        failed
            .iter()
            .map(|(t, m)| format!("  {t}: {m}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // Byte identity with the contract wherever the contract has the layout.
    let diff: Vec<&String> = outcomes
        .iter()
        .filter(|(_, o)| o.identical_to_contract == Some(false))
        .map(|(t, _)| t)
        .collect();
    assert!(
        diff.is_empty(),
        "framework bytes differ from the contract: {diff:?}"
    );
    // Black-box readers: every file opens with its geometry.
    let bad_info: Vec<&String> = outcomes
        .iter()
        .filter(|(_, o)| o.heif_info == Some(false))
        .map(|(t, _)| t)
        .collect();
    assert!(bad_info.is_empty(), "heif-info refused: {bad_info:?}");
    let bad_sips: Vec<&String> = outcomes
        .iter()
        .filter(|(_, o)| o.sips == Some(false))
        .map(|(t, _)| t)
        .collect();
    eprintln!("sips refused {} files: {bad_sips:?}", bad_sips.len());
    let bad_avc: Vec<&String> = outcomes
        .iter()
        .filter(|(_, o)| o.avc_black_box == Some(false))
        .map(|(t, _)| t)
        .collect();
    assert!(
        bad_avc.is_empty(),
        "avc1 items differ from the black-box decoder: {bad_avc:?}"
    );
    let avc_checked = outcomes
        .values()
        .filter(|o| o.avc_black_box == Some(true))
        .count();
    eprintln!("avc1 files byte-identical in the black-box decoder: {avc_checked}");
    // The thread budget and the aliases never change a byte.
    for (a, b) in byte_twins() {
        let (oa, ob) = (&outcomes[&a], &outcomes[&b]);
        assert!(oa.bytes == ob.bytes, "{a} and {b} differ");
    }
}

/// Invalid parameters are typed refusals at construction / first frame.
#[test]
fn framework_encoder_refuses_bad_parameters() {
    let ctx = context();
    let src = source(layout_of(Pf::Rgb24), 8, 8).0;
    let bad: &[(&str, &str)] = &[
        ("codec", "jpeg"),
        ("mode", "lossless"),
        ("depth", "9"),
        ("ctb", "48"),
        ("grid", "abc"),
        ("threads", "many"),
        ("range", "wide"),
        ("filters", "maybe"),
        ("chroma", "422"),
        ("speed", "ludicrous"),
        ("quality", "x"),
        ("nonsense", "1"),
    ];
    for (k, v) in bad {
        let mut p = CodecParameters::video(CodecId::new("heif"));
        p.width = Some(8);
        p.height = Some(8);
        p.pixel_format = Some(Pf::Rgb24);
        p.options = CodecOptions::new().set(*k, *v);
        assert!(
            ctx.codecs.first_encoder(&p).is_err(),
            "{k}={v} must be refused at construction"
        );
    }
    // Geometry / layout.
    let mut p = CodecParameters::video(CodecId::new("heif"));
    assert!(ctx
        .codecs
        .first_encoder(&p)
        .unwrap()
        .send_frame(&Frame::Video(src.clone()))
        .is_err());
    p.width = Some(8);
    p.height = Some(8);
    assert!(ctx
        .codecs
        .first_encoder(&p)
        .unwrap()
        .send_frame(&Frame::Video(src.clone()))
        .is_err());
    for pf in [
        Pf::Nv12,
        Pf::Pal8,
        Pf::Yuyv422,
        Pf::Cmyk,
        Pf::MonoBlack,
        Pf::Argb,
        Pf::Yuv411P,
    ] {
        p.pixel_format = Some(pf);
        let mut enc = ctx.codecs.first_encoder(&p).unwrap();
        let e = enc.send_frame(&Frame::Video(src.clone())).unwrap_err();
        assert!(
            matches!(e, oxideav_core::Error::Unsupported(_)),
            "{pf:?}: {e}"
        );
    }
    // A frame short of planes / bytes.
    p.pixel_format = Some(Pf::Yuv420P);
    let mut enc = ctx.codecs.first_encoder(&p).unwrap();
    assert!(
        enc.send_frame(&Frame::Video(src.clone())).is_err(),
        "one plane for 4:2:0"
    );
    p.pixel_format = Some(Pf::Rgb24);
    let mut enc = ctx.codecs.first_encoder(&p).unwrap();
    let short = VideoFrame {
        pts: None,
        planes: vec![VideoPlane {
            stride: 24,
            data: vec![0; 100],
        }],
    };
    assert!(enc.send_frame(&Frame::Video(short)).is_err(), "short rows");
    // Audio frames.
    let mut enc = ctx.codecs.first_encoder(&p).unwrap();
    assert!(enc
        .send_frame(&Frame::Audio(oxideav_core::AudioFrame {
            samples: 0,
            pts: None,
            data: vec![]
        }))
        .is_err());
}

/// The automatic grid: a picture above 4 MP is tiled (512-px tiles)
/// through the framework encoder exactly as through the contract, and
/// opens everywhere with the full geometry.
#[test]
fn framework_encoder_auto_grid_above_4_mp() {
    let ctx = context();
    let dir = scratch();
    let (w, h) = (2056u32, 2048u32);
    let c = Cell::new(
        "hevc/intra auto-grid Rgb24 2056x2048",
        Pf::Rgb24,
        w,
        h,
        &[("threads", "auto")],
    );
    let o = run_cell(&ctx, &c, &dir).unwrap();
    assert_eq!(o.identical_to_contract, Some(true));
    let file = HeifFile::parse(&o.bytes).unwrap();
    let meta = file.meta().unwrap();
    let primary = file.primary_item().unwrap();
    assert_eq!(&primary.item_type, b"grid", "the primary is a grid item");
    let tiles = meta.derivation_inputs(primary.id);
    assert_eq!(tiles.len(), 5 * 4, "512-px tiles over 2056x2048");
    assert_eq!(o.label, Pf::YuvJ420P);
    assert!(o.psnr >= 30.0);
}
