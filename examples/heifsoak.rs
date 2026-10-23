//! Production soak (round 478): every file given decodes and re-encodes
//! under an `ExecutionContext` budget, with wall time and peak RSS
//! recorded per file and budget.
//!
//! ```text
//! heifsoak [--threads 1,4,8] [--runs N] [--no-encode] [--all-encodes] [--csv out.csv] <file|dir>...
//! ```
//!
//! * decode: `HeifFile::parse` + `decode_primary` under the budget
//!   (median of N runs, default 3); a file without a decodable primary
//!   (an image sequence, a conformance shape this crate refuses) falls
//!   back to the contract `decode_all` and reports its frame count or
//!   the typed refusal — a refusal is a row, never a panic.
//! * peak RSS: a child process decoding the file once under the same
//!   budget (`/usr/bin/time -l` on macOS, `-v` on Linux).
//! * encode: the decoded primary re-encoded with the production
//!   defaults for HEVC (`intra` QP 18, the automatic grid) and AV1
//!   (quality 60, `fast`; above 4 MP only with `--all-encodes`), and
//!   AVC (`cabac`, the defaults) when the source primary is an `avc1`
//!   item — one run each under the budget.
//!
//! Prints one Markdown table per budget, the ten slowest decodes and
//! encodes per budget, and totals; `--csv` writes every cell.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use oxideav_core::ExecutionContext;
use oxideav_heif::HeifFile;
use oxideav_heif::decode::{ItemDecoder, decode_primary};
use oxideav_heif::encode::{EncodeOptions, StillCodec, encode_still};

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[derive(Clone)]
struct Row {
    file: String,
    bytes: usize,
    layout: String,
    decode: Option<f64>,
    frames: usize,
    note: String,
    rss_mib: Option<f64>,
    hevc: Option<(f64, usize)>,
    av1: Option<(f64, usize)>,
    avc: Option<(f64, usize)>,
}

fn collect(args: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for a in args {
        let p = Path::new(a);
        if p.is_dir() {
            let mut v: Vec<PathBuf> = walk(p);
            v.sort();
            files.extend(v);
        } else {
            files.push(p.to_path_buf());
        }
    }
    files
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else if let Some(ext) = p.extension().and_then(|e| e.to_str())
                && matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "heic" | "heif" | "avif" | "hif" | "heics" | "avifs" | "avis"
                )
            {
                out.push(p);
            }
        }
    }
    out
}

fn peak_rss(exe: &Path, path: &Path, threads: usize) -> Option<f64> {
    let flag = if cfg!(target_os = "macos") {
        "-l"
    } else {
        "-v"
    };
    let o = Command::new("/usr/bin/time")
        .arg(flag)
        .arg(exe)
        .arg("--child")
        .arg(path)
        .arg(threads.to_string())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&o.stderr);
    let v = text
        .lines()
        .find(|l| l.to_ascii_lowercase().contains("maximum resident set size"))
        .and_then(|l| {
            l.split_whitespace()
                .filter_map(|w| w.parse::<u64>().ok())
                .next()
        })?;
    let bytes = if cfg!(target_os = "macos") {
        v
    } else {
        v * 1024
    };
    Some(bytes as f64 / 1048576.0)
}

fn fmt_s(v: Option<f64>) -> String {
    v.map(|s| format!("{s:.3} s")).unwrap_or_else(|| "—".into())
}

fn fmt_enc(v: Option<(f64, usize)>) -> String {
    v.map(|(s, n)| format!("{s:.2} s ({} KiB)", n / 1024))
        .unwrap_or_else(|| "—".into())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "usage: heifsoak [--threads 1,4,8] [--runs N] [--no-encode] [--all-encodes] [--csv out.csv] <file|dir>..."
        );
        std::process::exit(2);
    }
    if args[0] == "--child" {
        let bytes = std::fs::read(&args[1]).expect("read");
        let threads: usize = args.get(2).and_then(|t| t.parse().ok()).unwrap_or(1);
        let f = HeifFile::parse(&bytes).expect("parse");
        let dec =
            ItemDecoder::direct().with_execution_context(&ExecutionContext::with_threads(threads));
        match decode_primary(&f, dec) {
            Ok(img) => println!("{}x{}", img.width(), img.height()),
            Err(_) => {
                let n = oxideav_heif::decode_all(&bytes)
                    .map(|v| v.len())
                    .unwrap_or(0);
                println!("{n} frames");
            }
        }
        return;
    }
    let mut budgets = vec![1usize, 4, 8];
    let mut runs = 3usize;
    let mut encode = true;
    let mut all_encodes = false;
    let mut csv: Option<PathBuf> = None;
    let mut inputs = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--threads" => {
                budgets = args[i + 1]
                    .split(',')
                    .map(|t| t.parse().expect("--threads a,b,c"))
                    .collect();
                i += 1;
            }
            "--runs" => {
                runs = args[i + 1].parse().expect("--runs N");
                i += 1;
            }
            "--no-encode" => encode = false,
            "--all-encodes" => all_encodes = true,
            "--csv" => {
                csv = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            a => inputs.push(a.to_string()),
        }
        i += 1;
    }
    let files = collect(&inputs);
    let exe = std::env::current_exe().expect("exe");
    let mut csv_lines = vec![
        "threads,file,bytes,layout,decode_s,frames,rss_mib,hevc_s,hevc_bytes,av1_s,av1_bytes,avc_s,avc_bytes,note"
            .to_string(),
    ];
    for &threads in &budgets {
        let ctx = ExecutionContext::with_threads(threads);
        let mut rows: Vec<Row> = Vec::new();
        let started = Instant::now();
        for path in &files {
            let file = path.file_name().unwrap().to_string_lossy().to_string();
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => {
                    rows.push(Row {
                        file,
                        bytes: 0,
                        layout: String::new(),
                        decode: None,
                        frames: 0,
                        note: format!("read: {e}"),
                        rss_mib: None,
                        hevc: None,
                        av1: None,
                        avc: None,
                    });
                    continue;
                }
            };
            let mut row = Row {
                file: file.clone(),
                bytes: bytes.len(),
                layout: String::new(),
                decode: None,
                frames: 0,
                note: String::new(),
                rss_mib: None,
                hevc: None,
                av1: None,
                avc: None,
            };
            let mut img = None;
            let mut primary_type = None;
            let mut times = Vec::with_capacity(runs);
            for _ in 0..runs {
                let t = Instant::now();
                let f = match HeifFile::parse(&bytes) {
                    Ok(f) => f,
                    Err(e) => {
                        row.note = format!("parse: {e}");
                        break;
                    }
                };
                primary_type = f.primary_item().ok().map(|p| p.item_type);
                match decode_primary(&f, ItemDecoder::direct().with_execution_context(&ctx)) {
                    Ok(d) => {
                        times.push(t.elapsed());
                        img = Some(d);
                    }
                    Err(e) => {
                        // Sequences / shapes without a still: the contract path.
                        let t2 = Instant::now();
                        match oxideav_heif::decode_all(&bytes) {
                            Ok(frames) => {
                                times.push(t2.elapsed());
                                row.frames = frames.len();
                                row.note = format!("no decodable primary ({e}); decode_all");
                            }
                            Err(e2) => {
                                row.note = format!("refused: {e2}");
                                break;
                            }
                        }
                    }
                }
            }
            if !times.is_empty() {
                row.decode = Some(median(times).as_secs_f64());
            }
            if let Some(img) = &img {
                row.frames = 1;
                row.layout = format!(
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
            }
            row.rss_mib = peak_rss(&exe, path, threads);
            if encode && let Some(img) = &img {
                let base = EncodeOptions::default().with_threads(Some(threads));
                let s = Instant::now();
                if let Ok(out) = encode_still(&img.frame, &base) {
                    row.hevc = Some((s.elapsed().as_secs_f64(), out.len()));
                }
                let big = img.width() as u64 * img.height() as u64 > 4_000_000;
                if !big || all_encodes {
                    let o = base
                        .clone()
                        .with_codec(StillCodec::Av1)
                        .with_av1_quality(Some(60));
                    let s = Instant::now();
                    if let Ok(out) = encode_still(&img.frame, &o) {
                        row.av1 = Some((s.elapsed().as_secs_f64(), out.len()));
                    }
                }
                if primary_type == Some(*b"avc1") {
                    let o = base.clone().with_codec(StillCodec::Avc);
                    let s = Instant::now();
                    if let Ok(out) = encode_still(&img.frame, &o) {
                        row.avc = Some((s.elapsed().as_secs_f64(), out.len()));
                    }
                }
            }
            csv_lines.push(format!(
                "{threads},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                row.file,
                row.bytes,
                row.layout.replace(',', ";"),
                row.decode.map(|v| format!("{v:.4}")).unwrap_or_default(),
                row.frames,
                row.rss_mib.map(|v| format!("{v:.1}")).unwrap_or_default(),
                row.hevc.map(|(s, _)| format!("{s:.3}")).unwrap_or_default(),
                row.hevc.map(|(_, n)| n.to_string()).unwrap_or_default(),
                row.av1.map(|(s, _)| format!("{s:.3}")).unwrap_or_default(),
                row.av1.map(|(_, n)| n.to_string()).unwrap_or_default(),
                row.avc.map(|(s, _)| format!("{s:.3}")).unwrap_or_default(),
                row.avc.map(|(_, n)| n.to_string()).unwrap_or_default(),
                row.note.replace(',', ";"),
            ));
            rows.push(row);
        }
        println!(
            "\n### Budget: {threads} worker(s) — {} files in {:.1} s wall\n",
            rows.len(),
            started.elapsed().as_secs_f64()
        );
        println!(
            "| File | Size | Layout | Decode | Peak RSS | Encode HEVC | Encode AV1 | Encode AVC | Note |"
        );
        println!("|---|---|---|---|---|---|---|---|---|");
        for r in &rows {
            println!(
                "| {} | {} KiB | {} | {} | {} | {} | {} | {} | {} |",
                r.file,
                r.bytes / 1024,
                r.layout,
                fmt_s(r.decode),
                r.rss_mib
                    .map(|m| format!("{m:.0} MiB"))
                    .unwrap_or_else(|| "n/a".into()),
                fmt_enc(r.hevc),
                fmt_enc(r.av1),
                fmt_enc(r.avc),
                r.note
            );
        }
        let mut slowest: Vec<&Row> = rows.iter().filter(|r| r.decode.is_some()).collect();
        slowest.sort_by(|a, b| b.decode.partial_cmp(&a.decode).unwrap());
        println!("\nSlowest ten decodes at {threads} worker(s):\n");
        println!("| File | Layout | Decode | Peak RSS | Encode HEVC |");
        println!("|---|---|---|---|---|");
        for r in slowest.iter().take(10) {
            println!(
                "| {} | {} | {} | {} | {} |",
                r.file,
                r.layout,
                fmt_s(r.decode),
                r.rss_mib
                    .map(|m| format!("{m:.0} MiB"))
                    .unwrap_or_else(|| "n/a".into()),
                fmt_enc(r.hevc)
            );
        }
        let total_dec: f64 = rows.iter().filter_map(|r| r.decode).sum();
        let total_hevc: f64 = rows.iter().filter_map(|r| r.hevc.map(|h| h.0)).sum();
        let total_av1: f64 = rows.iter().filter_map(|r| r.av1.map(|h| h.0)).sum();
        let refused = rows.iter().filter(|r| r.decode.is_none()).count();
        let max_rss = rows.iter().filter_map(|r| r.rss_mib).fold(0.0f64, f64::max);
        println!(
            "\nTotals at {threads} worker(s): decode {total_dec:.2} s, HEVC encode {total_hevc:.2} s, AV1 encode {total_av1:.2} s, peak RSS max {max_rss:.0} MiB, {refused} refused of {}.",
            rows.len()
        );
    }
    if let Some(p) = csv {
        std::fs::write(&p, csv_lines.join("\n") + "\n").expect("csv");
    }
}
