//! The framework path (`"heif"` demuxer + the registry decoders)
//! against the contract (`decode_all` / `info`) over every HEIF-family
//! file this checkout can see: the crate's vendored fixtures (always),
//! and — when the umbrella's `docs/` is checked out — the staged
//! corpus bundles, the real device samples and the MPEG / Nokia
//! conformance set (`docs/image/heif/{fixtures,device-samples,
//! conformance}`), plus the AVIF corpus under `docs/image/avif`.
//!
//! One row per file: the outcome class must agree (both decode, or
//! both refuse), every contract image item must be a packet of a
//! still stream with byte-identical pixels, the same label and colour
//! signal, untimed (`pts` = index, duration 1, 1/1), and every contract
//! track frame must come back from a track stream with the same pixels
//! and a delay of `duration × time_base`. `info()` must describe the
//! primary stream.
#![cfg(feature = "registry")]

mod common;

use std::collections::{BTreeMap, VecDeque};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use oxideav_core::{Decoder, Error, Frame as CoreFrame, RuntimeContext, StreamInfo, TimeBase};
use oxideav_heif::{decode_all_with, info, ColorInfo, DecodeOptions, Frame, HeifImage};

fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_h264::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    ctx
}

fn is_heif_name(p: &Path) -> bool {
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "heic" | "heif" | "heics" | "heifs" | "hif" | "avif" | "avifs" | "hmg"
    )
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else if is_heif_name(&p) {
            out.push(p);
        }
    }
}

/// Every input file: the vendored set, then the docs sets when present.
/// A debug build skips the files whose header announces more than 4 M
/// samples of decode work (`frames × width × height`: the 12 MP device
/// samples, the 720p conformance sequences — minutes unoptimised,
/// twice over) unless `OXIDEAV_HEIF_MATRIX_FULL=1`; a release build
/// takes everything.
fn inputs() -> Vec<PathBuf> {
    let mut all = Vec::new();
    collect(&mut all);
    let full = !cfg!(debug_assertions)
        || std::env::var("OXIDEAV_HEIF_MATRIX_FULL")
            .map(|v| v == "1")
            .unwrap_or(false);
    if full {
        return all;
    }
    let (small, large): (Vec<PathBuf>, Vec<PathBuf>) = all.into_iter().partition(|p| {
        let bytes = std::fs::read(p).unwrap_or_default();
        match info(&bytes) {
            Ok(i) => (i.frames as u64) * (i.width as u64) * (i.height as u64) <= 4 << 20,
            Err(_) => true,
        }
    });
    eprintln!(
        "debug build: {} large files skipped (OXIDEAV_HEIF_MATRIX_FULL=1 or --release takes them)",
        large.len()
    );
    small
}

fn collect(v: &mut Vec<PathBuf>) {
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        v,
    );
    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/image");
    for sub in [
        "heif/fixtures",
        "heif/device-samples",
        "heif/conformance/heic",
        "heif/conformance/cenc",
        "avif",
    ] {
        let d = docs.join(sub);
        if d.is_dir() {
            walk(&d, v);
        }
    }
}

/// Pixels (layout, geometry, every row) of two images are identical.
fn same_pixels(a: &HeifImage, b: &HeifImage) -> Result<(), String> {
    let fa = a.to_frame().map_err(|e| e.to_string())?;
    let fb = b.to_frame().map_err(|e| e.to_string())?;
    if (fa.width, fa.height, fa.format) != (fb.width, fb.height, fb.format) {
        return Err(format!(
            "{}x{} {:?} vs {}x{} {:?}",
            fa.width, fa.height, fa.format, fb.width, fb.height, fb.format
        ));
    }
    for p in 0..fa.format.plane_count() {
        let (_, rows) = fa.plane_dims(p);
        for y in 0..rows {
            if fa.row(p, y) != fb.row(p, y) {
                return Err(format!("plane {p} row {y} differs"));
            }
        }
    }
    Ok(())
}

fn delay_of(ticks: i64, tb: TimeBase) -> Duration {
    let d = ticks.max(0) as u64;
    let ts = tb.0.den.max(1) as u64;
    Duration::new(d / ts, ((d % ts) * 1_000_000_000 / ts) as u32)
}

/// A decoded framework frame with its packet timing.
struct Got {
    image: HeifImage,
    pts: Option<i64>,
    duration: Option<i64>,
}

/// Every stream of the file through its registry decoder.
/// Streams, metadata and the decoded frames per stream index.
type Pumped = (
    Vec<StreamInfo>,
    Vec<(String, String)>,
    BTreeMap<u32, Vec<Got>>,
);

fn pump_all(ctx: &RuntimeContext, bytes: Vec<u8>) -> Result<Pumped, String> {
    let mut d = ctx
        .containers
        .open_demuxer("heif", Box::new(Cursor::new(bytes)), &ctx.codecs)
        .map_err(|e| format!("open: {e}"))?;
    let streams = d.streams().to_vec();
    let metadata = d.metadata().to_vec();
    let mut decoders: Vec<Option<Box<dyn Decoder>>> = streams
        .iter()
        .map(|s| ctx.codecs.first_decoder(&s.params).ok())
        .collect();
    let mut timing: Vec<VecDeque<(Option<i64>, Option<i64>)>> =
        vec![VecDeque::new(); streams.len()];
    let mut out: BTreeMap<u32, Vec<Got>> = BTreeMap::new();
    let mut errors: Vec<String> = Vec::new();
    fn drain(
        dec: &mut dyn Decoder,
        s: &StreamInfo,
        timing: &mut VecDeque<(Option<i64>, Option<i64>)>,
        out: &mut Vec<Got>,
        errors: &mut Vec<String>,
    ) {
        loop {
            match dec.receive_frame() {
                Ok(CoreFrame::Video(v)) => {
                    let (pts, duration) = timing.pop_front().unwrap_or((None, None));
                    match HeifImage::from_video_frame(&v, &s.params) {
                        Ok(image) => out.push(Got {
                            image,
                            pts,
                            duration,
                        }),
                        Err(e) => errors.push(format!("stream {}: frame: {e}", s.index)),
                    }
                }
                Ok(_) => {}
                Err(Error::NeedMore) | Err(Error::Eof) => return,
                Err(e) => {
                    errors.push(format!("stream {}: receive: {e}", s.index));
                    return;
                }
            }
        }
    }
    loop {
        match d.next_packet() {
            Ok(p) => {
                let i = p.stream_index as usize;
                let Some(Some(dec)) = decoders.get_mut(i) else {
                    continue;
                };
                timing[i].push_back((p.pts, p.duration));
                match dec.send_packet(&p) {
                    Ok(()) => drain(
                        dec.as_mut(),
                        &streams[i],
                        &mut timing[i],
                        out.entry(p.stream_index).or_default(),
                        &mut errors,
                    ),
                    Err(e) => {
                        // A refused packet yields no frame: its timing
                        // entry goes with it.
                        timing[i].pop_back();
                        errors.push(format!("stream {i}: send: {e}"));
                    }
                }
            }
            Err(Error::Eof) => break,
            Err(e) => {
                errors.push(format!("next_packet: {e}"));
                break;
            }
        }
    }
    for (i, dec) in decoders.iter_mut().enumerate() {
        if let Some(dec) = dec {
            let _ = dec.flush();
            drain(
                dec.as_mut(),
                &streams[i],
                &mut timing[i],
                out.entry(i as u32).or_default(),
                &mut errors,
            );
        }
    }
    if out.values().all(|v| v.is_empty()) && !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok((streams, metadata, out))
}

fn meta_of<'a>(metadata: &'a [(String, String)], key: &str) -> Option<&'a str> {
    metadata
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// One file: the list of discrepancies (empty = the paths agree).
fn compare(ctx: &RuntimeContext, path: &Path) -> Vec<String> {
    let mut issues = Vec::new();
    let bytes = std::fs::read(path).unwrap();
    let opts = DecodeOptions::default();
    let contract = decode_all_with(&bytes, &opts);
    let framework = pump_all(ctx, bytes.clone());
    let (frames, (streams, metadata, got)) = match (contract, framework) {
        (Err(_), Err(_)) => return issues,
        (Err(e), Ok((_, _, got))) => {
            // `decode_all` is all-or-nothing; the framework delivers
            // per packet. What it delivers must be what `decode`
            // (the primary alone) yields — nothing when that refuses.
            let n: usize = got.values().map(Vec::len).sum();
            let first = got
                .get(&0)
                .and_then(|v| v.iter().find(|g| g.pts == Some(0)));
            match (oxideav_heif::decode_with(&bytes, &opts), first) {
                (Ok(primary), Some(g)) => {
                    if let Err(e) = same_pixels(&g.image, &primary) {
                        issues.push(format!("primary after a burst refusal: {e}"));
                    }
                }
                (Ok(_), None) => issues.push(format!(
                    "contract decodes the primary but the framework has no frame for packet 0 ({e})"
                )),
                (Err(_), Some(_)) => issues.push(format!(
                    "contract refuses the primary ({e}) but the framework decodes packet 0 ({n} frames)"
                )),
                (Err(_), None) => {}
            }
            return issues;
        }
        (Ok(frames), Err(e)) => {
            issues.push(format!(
                "contract decodes {} frames but the framework refuses: {e}",
                frames.len()
            ));
            return issues;
        }
        (Ok(frames), Ok(f)) => (frames, f),
    };
    // Still items: every contract item frame is a packet of a still
    // stream with the same pixels; its stream describes it.
    let stills: Vec<&StreamInfo> = streams
        .iter()
        .filter(|s| {
            s.params.codec_id.as_str() == "heif"
                && meta_of(
                    &metadata,
                    &format!("stream:{}:composed_from_track", s.index),
                )
                .is_none()
        })
        .collect();
    let items: Vec<&Frame> = frames.iter().filter(|f| f.track_id.is_none()).collect();
    // `altr` alternatives of an item: a packet named after one member
    // of the group stands for the group (MIAF: the primary or an
    // alternative shall be processable; the decoder falls back).
    let parsed = oxideav_heif::HeifFile::parse(&bytes).ok();
    let alternatives = |id: u32| -> Vec<u32> {
        parsed
            .as_ref()
            .and_then(|f| f.meta().ok())
            .map(|m| {
                m.groups_containing(id, b"altr")
                    .into_iter()
                    .flat_map(|g| g.entity_ids.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    for (k, f) in items.iter().enumerate() {
        let id = f.item_id.unwrap();
        let alts = alternatives(id);
        let mut found = false;
        for s in &stills {
            let ids: Vec<u32> = meta_of(&metadata, &format!("stream:{}:item_ids", s.index))
                .unwrap_or("")
                .split(',')
                .filter_map(|x| x.parse().ok())
                .collect();
            let Some(pos) = ids
                .iter()
                .position(|&x| x == id)
                .or_else(|| ids.iter().position(|x| alts.contains(x)))
            else {
                continue;
            };
            found = true;
            let Some(g) = got.get(&s.index).and_then(|v| v.get(pos)) else {
                issues.push(format!("item {id}: stream {} has no frame {pos}", s.index));
                continue;
            };
            if let Err(e) = same_pixels(&g.image, &f.image) {
                issues.push(format!("item {id}: pixels: {e}"));
            }
            if g.image.color != f.image.color {
                issues.push(format!(
                    "item {id}: colour {:?} vs contract {:?}",
                    g.image.color, f.image.color
                ));
            }
            if (g.pts, g.duration) != (Some(pos as i64), Some(1)) {
                issues.push(format!(
                    "item {id}: timing {:?}/{:?}, expected {pos}/1",
                    g.pts, g.duration
                ));
            }
            if s.time_base != TimeBase::new(1, 1) {
                issues.push(format!("item {id}: stream time base {:?}", s.time_base));
            }
            if (s.params.width, s.params.height) != (Some(f.image.width), Some(f.image.height)) {
                issues.push(format!(
                    "item {id}: stream geometry {:?}x{:?} vs {}x{}",
                    s.params.width, s.params.height, f.image.width, f.image.height
                ));
            }
            let want_pf = f.image.format.to_core_labelled(&f.image.color);
            if s.params.pixel_format != Some(want_pf) {
                issues.push(format!(
                    "item {id}: stream label {:?} vs {:?}",
                    s.params.pixel_format, want_pf
                ));
            }
            if ColorInfo::from_color_signal(&s.params.color_signal) != f.image.color {
                issues.push(format!(
                    "item {id}: stream colour signal {:?} vs {:?}",
                    s.params.color_signal, f.image.color
                ));
            }
            if k == 0 && stills.first().map(|f| f.index) != Some(s.index) {
                issues.push(format!(
                    "primary item {id} is not on the first still stream ({})",
                    stills.first().map(|f| f.index).unwrap_or(u32::MAX)
                ));
            }
            if f.delay.is_some() {
                issues.push(format!("item {id}: contract delay on an item"));
            }
        }
        if !found {
            // The other view of a stereo pair rides its partner's
            // packet as a second, view-tagged frame (timing-less here).
            let as_view = stills.iter().any(|s| {
                got.get(&s.index)
                    .map(|v| {
                        v.iter()
                            .filter(|g| g.pts.is_none())
                            .any(|g| same_pixels(&g.image, &f.image).is_ok())
                    })
                    .unwrap_or(false)
            });
            if !as_view {
                let lists: Vec<String> = stills
                    .iter()
                    .map(|s| {
                        format!(
                            "{}=[{}]",
                            s.index,
                            meta_of(&metadata, &format!("stream:{}:item_ids", s.index))
                                .unwrap_or("")
                        )
                    })
                    .collect();
                issues.push(format!(
                    "item {id}: on no still stream ({})",
                    lists.join(" ")
                ));
            }
        }
    }
    // Tracks: every contract track frame comes from the composed
    // stream of that track (alpha) or its raw stream.
    let mut by_track: BTreeMap<u32, Vec<&Frame>> = BTreeMap::new();
    for f in frames.iter().filter(|f| f.track_id.is_some()) {
        by_track.entry(f.track_id.unwrap()).or_default().push(f);
    }
    for (track, want) in by_track {
        let composed = streams.iter().find(|s| {
            meta_of(
                &metadata,
                &format!("stream:{}:composed_from_track", s.index),
            ) == Some(track.to_string().as_str())
        });
        let raw = streams.iter().find(|s| {
            meta_of(&metadata, &format!("stream:{}:track_id", s.index))
                == Some(track.to_string().as_str())
        });
        let Some(s) = composed.or(raw) else {
            issues.push(format!("track {track}: no stream"));
            continue;
        };
        let empty = Vec::new();
        let have = got.get(&s.index).unwrap_or(&empty);
        if have.len() != want.len() {
            issues.push(format!(
                "track {track}: {} framework frames vs {} contract frames",
                have.len(),
                want.len()
            ));
            continue;
        }
        let mut last = None;
        for (i, (g, w)) in have.iter().zip(&want).enumerate() {
            if let Err(e) = same_pixels(&g.image, &w.image) {
                issues.push(format!("track {track} sample {i}: pixels: {e}"));
            }
            let delay = g.duration.map(|d| delay_of(d, s.time_base));
            if delay != w.delay {
                issues.push(format!(
                    "track {track} sample {i}: delay {delay:?} vs {:?}",
                    w.delay
                ));
            }
            match (g.pts, last) {
                (Some(p), Some(l)) if p <= l => {
                    issues.push(format!("track {track} sample {i}: pts {p} after {l}"))
                }
                _ => {}
            }
            last = g.pts.or(last);
        }
    }
    // `info()` describes the first still stream; a sequence-brand file
    // lists its tracks first.
    let seq_major = parsed
        .as_ref()
        .map(|f| {
            matches!(
                &f.file_type.major_brand,
                b"msf1"
                    | b"hevc"
                    | b"hevx"
                    | b"hevm"
                    | b"hevs"
                    | b"avis"
                    | b"avio"
                    | b"jpgs"
                    | b"avcs"
                    | b"vvis"
            )
        })
        .unwrap_or(false);
    if let Some(first) = streams.first() {
        let is_still = first.params.codec_id.as_str() == "heif"
            && meta_of(
                &metadata,
                &format!("stream:{}:composed_from_track", first.index),
            )
            .is_none();
        if seq_major && is_still && streams.len() > 1 {
            issues.push("sequence-brand file lists its still first".into());
        }
        if !seq_major && !is_still && !items.is_empty() {
            issues.push("image-brand file lists a track first".into());
        }
    }
    match (info(&bytes), stills.first()) {
        (Ok(i), Some(s)) if !items.is_empty() => {
            if (Some(i.width), Some(i.height)) != (s.params.width, s.params.height) {
                issues.push(format!(
                    "info {}x{} vs the first still stream {:?}x{:?}",
                    i.width, i.height, s.params.width, s.params.height
                ));
            }
            if i.frames as usize != frames.len() {
                issues.push(format!(
                    "info.frames {} vs {} decoded frames",
                    i.frames,
                    frames.len()
                ));
            }
        }
        (Err(e), _) => issues.push(format!("info(): {e}")),
        _ => {}
    }
    issues
}

#[test]
fn framework_path_equals_decode_all_over_every_file() {
    let ctx = context();
    let files = inputs();
    assert!(files.len() >= 40, "{} inputs", files.len());
    let mut rows = Vec::new();
    let mut bad = 0usize;
    let mut decoded = 0usize;
    for path in &files {
        let name = path
            .strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .unwrap_or(path)
            .display()
            .to_string();
        let started = std::time::Instant::now();
        let issues = compare(&ctx, path);
        eprintln!("{name}: {:.1} s", started.elapsed().as_secs_f64());
        let bytes = std::fs::read(path).unwrap();
        let class = match decode_all_with(&bytes, &DecodeOptions::default()) {
            Ok(f) => {
                decoded += 1;
                format!("{} frames", f.len())
            }
            Err(e) => format!("refused ({e})"),
        };
        if issues.is_empty() {
            rows.push(format!("{name:<70} {class:<24} agree"));
        } else {
            bad += 1;
            rows.push(format!("{name:<70} {class:<24} {}", issues.join(" | ")));
        }
    }
    let report = rows.join("\n");
    eprintln!("{report}");
    eprintln!(
        "decode matrix: {} files, {} decode on both paths, {} disagree",
        files.len(),
        decoded,
        bad
    );
    let dir = std::env::temp_dir().join("oxideav-heif-framework-matrix");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("decode_matrix.txt"), &report);
    assert_eq!(bad, 0, "{bad} files disagree between the paths");
    common::fixture_root();
}
