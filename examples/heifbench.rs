//! Performance baseline: decode wall-clock + peak RSS and encode
//! wall-clock at default settings for a set of files.
//!
//! ```text
//! heifbench [--runs N] [--no-encode] file.heic [file.avif ...]
//! ```
//!
//! * decode: `HeifFile::parse` + `decode_primary` (direct factories),
//!   median of N runs (default 3) in this process, plus the peak
//!   resident set size of a child process that decodes the file once
//!   (`/usr/bin/time -l` on macOS, `-v` on Linux; `n/a` without it).
//! * encode: the decoded picture re-encoded with `EncodeOptions`
//!   defaults for HEVC (`intra` at QP 26) and for AV1 (quality 60,
//!   `fast`), median of N runs; the AV1 run is skipped for pictures
//!   above 4 MP unless `--all-encodes` is given (it is slow).
//!
//! Prints a Markdown table; the README's "Performance baseline"
//! section carries the last run on the reference machine.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use oxideav_heif::decode::{decode_primary, ItemDecoder};
use oxideav_heif::encode::{encode_still, EncodeOptions, StillCodec};
use oxideav_heif::HeifFile;

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: heifbench [--runs N] [--no-encode] [--all-encodes] <file>...");
        std::process::exit(2);
    }
    // Child mode: decode once (peak RSS measured by the parent).
    if args[0] == "--child" {
        let bytes = std::fs::read(&args[1]).expect("read");
        let f = HeifFile::parse(&bytes).expect("parse");
        let img = decode_primary(&f, ItemDecoder::direct()).expect("decode");
        println!("{}x{}", img.width(), img.height());
        return;
    }
    let mut runs = 3usize;
    let mut encode = true;
    let mut all_encodes = false;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--runs" => {
                runs = args[i + 1].parse().expect("--runs N");
                i += 1;
            }
            "--no-encode" => encode = false,
            "--all-encodes" => all_encodes = true,
            a => files.push(PathBuf::from(a)),
        }
        i += 1;
    }
    println!("| File | Size | Layout | Items | Decode (median of {runs}) | Peak RSS (decode) | Encode HEVC default | Encode AV1 default |");
    println!("|---|---|---|---|---|---|---|---|");
    for path in files {
        let bytes = std::fs::read(&path).expect("read");
        let mut times = Vec::with_capacity(runs);
        let mut img = None;
        let mut items = 0;
        for _ in 0..runs {
            let t = Instant::now();
            let f = HeifFile::parse(&bytes).expect("parse");
            let d = decode_primary(&f, ItemDecoder::direct()).expect("decode");
            times.push(t.elapsed());
            items = f.meta().map(|m| m.items.len()).unwrap_or(0);
            img = Some(d);
        }
        let img = img.unwrap();
        let decode = median(times);
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
        let exe = std::env::current_exe().expect("exe");
        let rss = {
            let flag = if cfg!(target_os = "macos") {
                "-l"
            } else {
                "-v"
            };
            match Command::new("/usr/bin/time")
                .arg(flag)
                .arg(&exe)
                .arg("--child")
                .arg(&path)
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
        let mut enc_hevc = "—".to_string();
        let mut enc_av1 = "—".to_string();
        if encode {
            let mut t = Vec::new();
            for _ in 0..runs {
                let s = Instant::now();
                let out = encode_still(&img.frame, &EncodeOptions::default()).expect("hevc encode");
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
                let opts = EncodeOptions {
                    codec: StillCodec::Av1,
                    av1_quality: Some(60),
                    ..EncodeOptions::default()
                };
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
            "| {} | {} KiB | {} | {} | {:.3} s | {} | {} | {} |",
            path.file_name().unwrap().to_string_lossy(),
            bytes.len() / 1024,
            layout,
            items,
            decode.as_secs_f64(),
            rss,
            enc_hevc,
            enc_av1
        );
    }
}
