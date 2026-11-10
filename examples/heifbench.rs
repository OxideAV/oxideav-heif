//! Performance baseline: decode wall-clock + peak RSS and encode
//! wall-clock at default settings for a set of files.
//!
//! ```text
//! heifbench [--runs N] [--threads N|auto] [--no-encode] [--framework] file.heic [file.avif ...]
//! ```
//!
//! * decode: `HeifFile::parse` + `decode_primary` (direct factories,
//!   under an `ExecutionContext` of `--threads` workers — default 1,
//!   the serial contract), median of N runs (default 3) in this
//!   process, plus the peak
//!   resident set size of a child process that decodes the file once
//!   (`/usr/bin/time -l` on macOS, `-v` on Linux; `n/a` without it).
//! * encode: the decoded picture re-encoded with `EncodeOptions`
//!   defaults for HEVC (`intra` at QP 18, filters, the automatic grid) and for AV1 (quality 60,
//!   `fast`) under the same thread budget, median of N runs; the AV1 run is skipped for pictures
//!   above 4 MP unless `--all-encodes` is given (it is slow).
//!
//! * hash: FNV-1a 64 over the decoded output planes (the byte-identity
//!   gate for the optimisation rounds: the value must not move).
//!
//! Prints a Markdown table; the README's "Performance baseline"
//! section carries the last run on the reference machine.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use oxideav_heif::HeifFile;
use oxideav_heif::decode::{ItemDecoder, decode_primary};
use oxideav_heif::encode::{EncodeOptions, StillCodec, encode_still};

fn fnv1a64(planes: &[oxideav_heif::Plane]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for p in planes {
        for b in &p.data {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// The registry path: the `"heif"` demuxer's first still packet
/// through the registry's `"heif"` decoder under `threads` workers.
/// Returns the frame and the stream parameters describing it.
fn framework_decode(
    ctx: &oxideav_core::RuntimeContext,
    bytes: &[u8],
    threads: usize,
) -> (oxideav_core::VideoFrame, oxideav_core::CodecParameters) {
    use oxideav_core::Frame;
    let mut demuxer = ctx
        .containers
        .open_demuxer(
            "heif",
            Box::new(std::io::Cursor::new(bytes.to_vec())),
            &ctx.codecs,
        )
        .expect("demuxer");
    let params = demuxer.streams()[0].params.clone();
    let pkt = demuxer.next_packet().expect("packet");
    drop(demuxer);
    let mut dec = ctx.codecs.first_decoder(&params).expect("decoder");
    dec.set_execution_context(&oxideav_core::ExecutionContext::with_threads(threads));
    dec.send_packet(&pkt).expect("send_packet");
    drop(pkt);
    loop {
        match dec.receive_frame().expect("frame") {
            Frame::Video(v) => return (v, params),
            _ => continue,
        }
    }
}

fn framework_context() -> oxideav_core::RuntimeContext {
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_h264::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    ctx
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "usage: heifbench [--runs N] [--threads N|auto] [--no-encode] [--all-encodes] \
             [--grid none|<tile>] [--write <out.avif|out.heic>] <file>..."
        );
        std::process::exit(2);
    }
    // Child mode: decode once (peak RSS measured by the parent).
    if args[0] == "--child-framework" {
        let bytes = std::fs::read(&args[1]).expect("read");
        let threads: usize = args.get(2).and_then(|t| t.parse().ok()).unwrap_or(1);
        let ctx = framework_context();
        let (frame, _) = framework_decode(&ctx, &bytes, threads);
        println!("{} planes", frame.planes.len());
        return;
    }
    if args[0] == "--child" {
        let bytes = std::fs::read(&args[1]).expect("read");
        let threads: usize = args.get(2).and_then(|t| t.parse().ok()).unwrap_or(1);
        let f = HeifFile::parse(&bytes).expect("parse");
        let dec = ItemDecoder::direct()
            .with_execution_context(&oxideav_core::ExecutionContext::with_threads(threads));
        let img = decode_primary(&f, dec).expect("decode");
        println!("{}x{}", img.width(), img.height());
        return;
    }
    let mut runs = 3usize;
    let mut threads = 1usize;
    let mut encode = true;
    let mut all_encodes = false;
    // `--framework`: time (and measure the peak RSS of) the registry
    // path — demuxer packet → registry `"heif"` decoder — next to the
    // direct one; its output digest must equal the direct decode's.
    let mut framework = false;
    // `--grid none` codes one item whatever the size (the automatic
    // 512-px grid above 4 MP otherwise); `--grid <tile>` an explicit grid.
    let mut grid: Option<u32> = None;
    // `--write <path>`: the first file's decoded primary re-encoded —
    // AV1 (quality 60) for an `.avif` path, HEVC (the defaults) for any
    // other — and written out: the 12 MP decode-budget sources.
    let mut write: Option<PathBuf> = None;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--runs" => {
                runs = args[i + 1].parse().expect("--runs N");
                i += 1;
            }
            "--threads" => {
                threads = match args[i + 1].as_str() {
                    "auto" => oxideav_core::ExecutionContext::auto().threads,
                    n => n.parse().expect("--threads N|auto"),
                };
                i += 1;
            }
            "--no-encode" => encode = false,
            "--framework" => framework = true,
            "--all-encodes" => all_encodes = true,
            "--grid" => {
                grid = Some(match args[i + 1].as_str() {
                    "none" => 0,
                    n => n.parse().expect("--grid none|<tile>"),
                });
                i += 1;
            }
            "--write" => {
                write = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            a => files.push(PathBuf::from(a)),
        }
        i += 1;
    }
    println!("threads: {threads}");
    println!(
        "| File | Size | Layout | Items | Decode (median of {runs}) | Peak RSS (decode) | Encode HEVC default | Encode AV1 default | Output FNV-1a |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for path in files {
        let bytes = std::fs::read(&path).expect("read");
        let mut times = Vec::with_capacity(runs);
        let mut img = None;
        let mut items = 0;
        for _ in 0..runs {
            let t = Instant::now();
            let f = HeifFile::parse(&bytes).expect("parse");
            let dec = ItemDecoder::direct()
                .with_execution_context(&oxideav_core::ExecutionContext::with_threads(threads));
            let d = decode_primary(&f, dec).expect("decode");
            times.push(t.elapsed());
            items = f.meta().map(|m| m.items.len()).unwrap_or(0);
            img = Some(d);
        }
        let img = img.unwrap();
        let decode = median(times);
        let child_rss = |mode: &str| -> String {
            let flag = if cfg!(target_os = "macos") {
                "-l"
            } else {
                "-v"
            };
            let exe = std::env::current_exe().expect("exe");
            match Command::new("/usr/bin/time")
                .arg(flag)
                .arg(&exe)
                .arg(mode)
                .arg(&path)
                .arg(threads.to_string())
                .output()
            {
                Ok(o) => {
                    let text = String::from_utf8_lossy(&o.stderr);
                    text.lines()
                        .find(|l| l.to_ascii_lowercase().contains("maximum resident set size"))
                        .and_then(|l| {
                            l.split_whitespace()
                                .filter_map(|w| w.parse::<u64>().ok())
                                .next()
                        })
                        .map(|v| {
                            // macOS reports bytes, GNU time kilobytes.
                            let bytes = if cfg!(target_os = "macos") {
                                v
                            } else {
                                v * 1024
                            };
                            format!("{:.0} MiB", bytes as f64 / 1048576.0)
                        })
                        .unwrap_or_else(|| "n/a".into())
                }
                Err(_) => "n/a".into(),
            }
        };
        if framework {
            let ctx = framework_context();
            let mut times = Vec::with_capacity(runs);
            let mut hash = 0;
            for _ in 0..runs {
                let t = Instant::now();
                let (frame, params) = framework_decode(&ctx, &bytes, threads);
                times.push(t.elapsed());
                // The digest is over the visible samples, as the direct
                // path's (outside the timed span).
                let f = oxideav_heif::HeifFrame::from_core(
                    &frame,
                    params.width.expect("width"),
                    params.height.expect("height"),
                    params.pixel_format.expect("pixel format"),
                )
                .expect("frame");
                hash = fnv1a64(&f.tight().planes);
            }
            eprintln!(
                "framework path: {} decode {:.3} s, peak RSS {}, output FNV-1a {hash:016x}",
                path.file_name().unwrap().to_string_lossy(),
                median(times).as_secs_f64(),
                child_rss("--child-framework"),
            );
        }
        let layout = format!(
            "{}×{} {:?} {}-bit{}",
            img.width(),
            img.height(),
            img.frame.format.chroma,
            img.frame.format.bit_depth,
            if img.frame.format.has_alpha {
                " +α"
            } else {
                ""
            }
        );
        // Peak RSS of a fresh process decoding once.
        let rss = child_rss("--child");
        let mut enc_hevc = "—".to_string();
        let mut enc_av1 = "—".to_string();
        if let Some(out) = write.take() {
            let avif = out
                .extension()
                .map(|e| e.eq_ignore_ascii_case("avif"))
                .unwrap_or(false);
            let mut opts = EncodeOptions::default()
                .with_grid_tile(grid)
                .with_threads(Some(threads));
            if avif {
                opts = opts.with_codec(StillCodec::Av1).with_av1_quality(Some(60));
            }
            let s = Instant::now();
            let bytes = encode_still(&img.frame, &opts).expect("encode");
            eprintln!(
                "wrote {} ({} bytes, {:.2} s)",
                out.display(),
                bytes.len(),
                s.elapsed().as_secs_f64()
            );
            std::fs::write(&out, bytes).expect("write");
        }
        if encode {
            let mut t = Vec::new();
            for _ in 0..runs {
                let s = Instant::now();
                let opts = EncodeOptions::default()
                    .with_grid_tile(grid)
                    .with_threads(Some(threads));
                let out = encode_still(&img.frame, &opts).expect("hevc encode");
                t.push(s.elapsed());
                if t.len() == 1 {
                    enc_hevc = format!("{:.2} s ({} KiB)", 0.0, out.len() / 1024);
                }
            }
            let m = median(t);
            enc_hevc = format!(
                "{:.2} s{}",
                m.as_secs_f64(),
                &enc_hevc[enc_hevc.find(" (").unwrap_or(enc_hevc.len())..]
            );
            let big = img.width() as u64 * img.height() as u64 > 4_000_000;
            if !big || all_encodes {
                let mut t = Vec::new();
                let opts = EncodeOptions::default()
                    .with_codec(StillCodec::Av1)
                    .with_av1_quality(Some(60))
                    .with_grid_tile(grid)
                    .with_threads(Some(threads));
                let mut size = 0;
                for _ in 0..runs {
                    let s = Instant::now();
                    let out = encode_still(&img.frame, &opts).expect("av1 encode");
                    t.push(s.elapsed());
                    size = out.len();
                }
                enc_av1 = format!("{:.2} s ({} KiB)", median(t).as_secs_f64(), size / 1024);
            } else {
                enc_av1 = "skipped (>4 MP; --all-encodes)".into();
            }
        }
        println!(
            "| {} | {} KiB | {} | {} | {:.3} s | {} | {} | {} | {:016x} |",
            path.file_name().unwrap().to_string_lossy(),
            bytes.len() / 1024,
            layout,
            items,
            decode.as_secs_f64(),
            rss,
            enc_hevc,
            enc_av1,
            fnv1a64(&img.frame.tight().planes)
        );
    }
}
