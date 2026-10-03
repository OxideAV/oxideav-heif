//! Encode attribution bench: wall-clock, CPU time and peak RSS of the
//! still-image encode path, stage by stage, for the budget table in
//! the README.
//!
//! ```text
//! heifencbench [--raw FILE --size WxH --fmt rgb24|rgb48le|gray8]
//!              [--synth WxH] [--codec hevc|av1] [--mode intra|pcm]
//!              [--qp N] [--quality N] [--grid T] [--threads N|auto]
//!              [--rd N] [--framework] [--out FILE] [--label TEXT]
//!              [--option KEY=VALUE ...]
//! ```
//!
//! * `--raw`: a packed picture dumped by a black-box tool (for example
//!   `ffmpeg -i in.png -f rawvideo -pix_fmt rgb24 in.rgb`); `--synth`
//!   makes the conformance-matrix gradient picture instead.
//! * `--framework`: the packed frame goes through the registry
//!   `"heif"` encoder (`packed_to_planar` + `encode_still`, the CLI
//!   path); otherwise the picture is converted first and
//!   `encode_still` is timed alone (the library path).
//! * Stages: `convert` (packed RGB → the codec's planar input),
//!   `encode` (the codec items + grid tiling), `write` (the file).
//!   The framework path reports the whole `send_frame` as one stage.
//!
//! Peak RSS is `ru_maxrss` of this process (`getrusage`), read after
//! each stage; CPU is `ru_utime + ru_stime`. One configuration per
//! process keeps the peaks attributable.

use std::time::Instant;

use oxideav_heif::encode::{encode_still, EncodeOptions, StillCodec};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};
use oxideav_heif::props::Colr;

#[cfg(unix)]
mod usage {
    #[repr(C)]
    struct Timeval {
        sec: i64,
        usec: i64,
    }
    #[repr(C)]
    struct Rusage {
        utime: Timeval,
        stime: Timeval,
        maxrss: i64,
        rest: [i64; 13],
    }
    extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }
    /// `(cpu seconds, peak RSS in bytes)` of this process.
    pub fn read() -> (f64, u64) {
        let mut r = Rusage {
            utime: Timeval { sec: 0, usec: 0 },
            stime: Timeval { sec: 0, usec: 0 },
            maxrss: 0,
            rest: [0; 13],
        };
        // SAFETY: `Rusage` mirrors the C struct's leading fields on
        // 64-bit Linux and macOS (two timevals then `ru_maxrss`), and
        // the trailing longs cover the rest of `struct rusage`.
        let rc = unsafe { getrusage(0, &mut r) };
        if rc != 0 {
            return (0.0, 0);
        }
        let cpu = r.utime.sec as f64
            + r.utime.usec as f64 * 1e-6
            + r.stime.sec as f64
            + r.stime.usec as f64 * 1e-6;
        // macOS reports bytes, Linux kilobytes.
        let rss = if cfg!(target_os = "macos") {
            r.maxrss as u64
        } else {
            r.maxrss as u64 * 1024
        };
        (cpu, rss)
    }
}
#[cfg(not(unix))]
mod usage {
    pub fn read() -> (f64, u64) {
        (0.0, 0)
    }
}

fn synth(w: u32, h: u32, channels: usize) -> Vec<u8> {
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
            if channels == 1 {
                let g = 0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2];
                v.push((g * 255.0).round() as u8);
            } else {
                v.extend(rgb.iter().map(|c| (c * 255.0).round() as u8));
            }
        }
    }
    v
}

struct Stage {
    name: &'static str,
    wall: f64,
    cpu: f64,
    rss: u64,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut raw: Option<String> = None;
    let mut size = (0u32, 0u32);
    let mut fmt = "rgb24".to_string();
    let mut synth_size: Option<(u32, u32)> = None;
    let mut opts = EncodeOptions::default();
    let mut threads = 1usize;
    let mut framework = false;
    let mut out: Option<String> = None;
    let mut label = String::new();
    let mut extra: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    let next = |i: &mut usize| -> String {
        *i += 1;
        args.get(*i).cloned().unwrap_or_default()
    };
    let wxh = |s: &str| -> (u32, u32) {
        let (a, b) = s.split_once('x').expect("WxH");
        (a.parse().expect("W"), b.parse().expect("H"))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--raw" => raw = Some(next(&mut i)),
            "--size" => size = wxh(&next(&mut i)),
            "--fmt" => fmt = next(&mut i),
            "--synth" => synth_size = Some(wxh(&next(&mut i))),
            "--codec" => {
                opts.codec = match next(&mut i).as_str() {
                    "av1" => StillCodec::Av1,
                    _ => StillCodec::Hevc,
                }
            }
            "--mode" => opts.hevc_mode = next(&mut i),
            "--qp" => opts.qp = next(&mut i).parse().expect("qp"),
            "--quality" => opts.av1_quality = Some(next(&mut i).parse().expect("quality")),
            "--grid" => opts.grid_tile = Some(next(&mut i).parse().expect("grid")),
            "--rd" => opts.hevc_rd = Some(next(&mut i).parse().expect("rd")),
            "--threads" => {
                threads = match next(&mut i).as_str() {
                    "auto" => oxideav_core::ExecutionContext::auto().threads,
                    n => n.parse().expect("threads"),
                }
            }
            "--framework" => framework = true,
            "--out" => out = Some(next(&mut i)),
            "--label" => label = next(&mut i),
            "--option" => {
                let kv = next(&mut i);
                let (k, v) = kv.split_once('=').expect("KEY=VALUE");
                extra.push((k.to_string(), v.to_string()));
            }
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    opts.threads = Some(threads);
    opts.hevc_options = extra.clone();
    let (w, h, packed, channels, wide): (u32, u32, Vec<u8>, usize, bool) = match (&raw, synth_size)
    {
        (Some(p), _) => {
            let bytes = std::fs::read(p).expect("read raw");
            let (ch, wide) = match fmt.as_str() {
                "rgb24" => (3, false),
                "rgb48le" => (3, true),
                "gray8" => (1, false),
                other => panic!("fmt {other}"),
            };
            let need = size.0 as usize * size.1 as usize * ch * if wide { 2 } else { 1 };
            assert_eq!(bytes.len(), need, "raw size does not match --size / --fmt");
            (size.0, size.1, bytes, ch, wide)
        }
        (None, Some((w, h))) => (w, h, synth(w, h, 3), 3, false),
        _ => {
            eprintln!("usage: heifencbench --raw FILE --size WxH [--fmt F] | --synth WxH ...");
            std::process::exit(2);
        }
    };
    let (cpu0, rss0) = usage::read();
    let t0 = Instant::now();
    let mut stages: Vec<Stage> = Vec::new();
    let mark = |name: &'static str, stages: &mut Vec<Stage>, last: &mut (Instant, f64)| {
        let (cpu, rss) = usage::read();
        stages.push(Stage {
            name,
            wall: last.0.elapsed().as_secs_f64(),
            cpu: cpu - last.1,
            rss,
        });
        *last = (Instant::now(), cpu);
    };
    let mut last = (Instant::now(), cpu0);
    let bytes: Vec<u8> = if framework {
        use oxideav_core::{CodecId, CodecOptions, CodecParameters, Frame};
        let pf = match (channels, wide) {
            (3, false) => oxideav_core::PixelFormat::Rgb24,
            (3, true) => oxideav_core::PixelFormat::Rgb48Le,
            _ => oxideav_core::PixelFormat::Gray8,
        };
        let mut params = CodecParameters::video(CodecId::new("heif"));
        params.width = Some(w);
        params.height = Some(h);
        params.pixel_format = Some(pf);
        let mut o = CodecOptions::new()
            .set(
                "codec",
                match opts.codec {
                    StillCodec::Av1 => "av1",
                    _ => "hevc",
                },
            )
            .set("mode", opts.hevc_mode.as_str())
            .set("qp", opts.qp.to_string())
            .set("threads", threads.to_string());
        if let Some(q) = opts.av1_quality {
            o = o.set("quality", q.to_string());
        }
        if let Some(g) = opts.grid_tile {
            o = o.set("grid", g.to_string());
        }
        if let Some(rd) = opts.hevc_rd {
            o = o.set("rd", rd.to_string());
        }
        for (k, v) in &extra {
            o = o.set(k.as_str(), v.as_str());
        }
        params.options = o;
        let mut enc = oxideav_heif::encode::make_encoder(&params).expect("encoder");
        let stride = w as usize * channels * if wide { 2 } else { 1 };
        let frame = oxideav_core::VideoFrame {
            pts: Some(0),
            planes: vec![oxideav_core::VideoPlane {
                stride,
                data: packed,
            }],
        };
        mark("input", &mut stages, &mut last);
        enc.send_frame(&Frame::Video(frame)).expect("send_frame");
        enc.flush().expect("flush");
        let pkt = enc.receive_packet().expect("packet");
        mark("send_frame", &mut stages, &mut last);
        pkt.data.to_vec()
    } else {
        // Library path: packed → 4:2:0 (or grey) planar at the source
        // depth, the layout `encode_still` codes directly.
        let depth: u8 = if wide { 16 } else { 8 };
        let chroma = if channels == 1 {
            Chroma::Mono
        } else {
            Chroma::Yuv420
        };
        let pf = HeifPixelFormat::new(chroma, depth, false).expect("format");
        let frame = if channels == 1 {
            HeifFrame {
                width: w,
                height: h,
                format: pf,
                planes: vec![oxideav_heif::Plane {
                    stride: w as usize * if wide { 2 } else { 1 },
                    data: packed,
                }],
            }
        } else {
            let pfmt = if wide {
                oxideav_core::PixelFormat::Rgb48Le
            } else {
                oxideav_core::PixelFormat::Rgb24
            };
            let vf = oxideav_core::VideoFrame {
                pts: None,
                planes: vec![oxideav_core::VideoPlane {
                    stride: w as usize * 3 * if wide { 2 } else { 1 },
                    data: packed,
                }],
            };
            oxideav_heif::encode::packed_to_planar_for(&vf, w, h, pfmt, &opts.colr, pf)
                .expect("convert")
        };
        mark("convert", &mut stages, &mut last);
        let bytes = encode_still(&frame, &opts).expect("encode");
        mark("encode+write", &mut stages, &mut last);
        bytes
    };
    let total = t0.elapsed().as_secs_f64();
    let (cpu1, rss1) = usage::read();
    if let Some(p) = &out {
        std::fs::write(p, &bytes).expect("write out");
    }
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in &bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    println!(
        "{label} {w}x{h} {} {} qp{} grid={:?} threads={threads} {}: {} bytes fnv={hash:016x}",
        match opts.codec {
            StillCodec::Av1 => "av1",
            _ => "hevc",
        },
        opts.hevc_mode,
        opts.qp,
        opts.grid_tile,
        if framework { "framework" } else { "library" },
        bytes.len()
    );
    println!(
        "  start rss {:.0} MiB; total wall {total:.3} s cpu {:.3} s peak rss {:.0} MiB",
        mib(rss0),
        cpu1 - cpu0,
        mib(rss1)
    );
    for s in &stages {
        println!(
            "  {:<14} wall {:.3} s  cpu {:.3} s  peak rss after {:.0} MiB",
            s.name,
            s.wall,
            s.cpu,
            mib(s.rss)
        );
    }
    let _ = Colr::MIAF_DEFAULT;
}
