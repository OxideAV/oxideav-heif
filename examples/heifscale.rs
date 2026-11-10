//! Container-side scaling: what the item model costs as a file grows,
//! with the codecs out of the picture.
//!
//! ```text
//! heifscale [--runs N] [--decode] [items ...]      (default 1024 4096 16384 65536)
//! ```
//!
//! For each item count `n` two files are built with [`HeifWriter`] from
//! one 64×64 coded tile (encoded once, its bytes shared by every item):
//!
//! * `grid`  — `n` tiles + one `grid` item over them (rows × columns
//!   ≤ 256 × 256, so `n` ≤ 65 536), the primary;
//! * `burst` — `n` independent displayable items, each with its own
//!   Exif item (`cdsc`), the first the primary.
//!
//! and every container-side step is timed (median of `--runs`, default
//! 3): `write` (`HeifWriter::write_to_vec`), `parse`
//! (`HeifFile::parse_borrowed`), `info`, `graph`
//! (`build_primary_graph`), `check` (`miaf::check`), `items` (typed
//! properties + data of every item — the walk a demuxer or a re-mux
//! does), `open` (the `"heif"` demuxer opened: streams described),
//! `packets` (every packet pulled, one run; each still packet is the
//! file with its `pitm` naming the item) and `remux`
//! (`HeifWriter::from_files` + write). `--decode` adds the
//! primary's decode at 1 and 8 workers for the `grid` shape (the
//! codec's time, for scale). Prints a Markdown table; the README's
//! performance section carries the last run on the reference machine.

use std::io::Cursor;
use std::time::{Duration, Instant};

use oxideav_heif::derived::GridDescriptor;
use oxideav_heif::encode::{EncodeOptions, encode_still};
use oxideav_heif::props::{ItemProperties, Property};
use oxideav_heif::{HeifFile, HeifFrame, HeifPixelFormat, HeifWriter};

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn timed<T>(runs: usize, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut times = Vec::with_capacity(runs);
    let mut last = None;
    for _ in 0..runs {
        let t = Instant::now();
        let v = f();
        times.push(t.elapsed());
        last = Some(v);
    }
    (median(times), last.expect("runs >= 1"))
}

fn ms(d: Duration) -> String {
    let v = d.as_secs_f64() * 1e3;
    if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 1.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.3}")
    }
}

/// One 64×64 HEVC tile: its coded bytes and properties.
fn tile() -> (Vec<u8>, Vec<(Property, bool)>) {
    let format = HeifPixelFormat::new(oxideav_heif::Chroma::Yuv420, 8, false).expect("layout");
    let mut frame = HeifFrame::filled(64, 64, format, 128).expect("frame");
    for y in 0..64 {
        for x in 0..64 {
            frame.set_sample(0, x, y, ((x * 3 + y * 2) % 256) as u16);
        }
    }
    let opts = EncodeOptions::default()
        .with_thumbnail_max_dim(None)
        .with_grid_tile(None);
    let bytes = encode_still(&frame, &opts).expect("encode the tile");
    let file = HeifFile::parse(&bytes).expect("parse the tile file");
    let id = file.primary_item().expect("primary").id;
    let props = ItemProperties::resolve(file.meta().expect("meta"), id).expect("properties");
    let data = file.item_data_owned(id).expect("tile data");
    (
        data,
        props
            .entries
            .into_iter()
            .map(|e| (e.property, e.essential))
            .collect(),
    )
}

fn grid_file(n: usize, data: &[u8], props: &[(Property, bool)]) -> HeifWriter {
    let cols = (n as f64).sqrt().ceil() as usize;
    let cols = cols.clamp(1, 256);
    let rows = n.div_ceil(cols).clamp(1, 256);
    let n = rows * cols;
    let mut w = HeifWriter::new();
    let tiles: Vec<u32> = (0..n)
        .map(|_| w.add_coded_item(*b"hvc1", data.to_vec(), props.to_vec()))
        .collect();
    let grid = w
        .add_grid(
            GridDescriptor::new(rows as u16, cols as u16, 64 * cols as u32, 64 * rows as u32),
            &tiles,
            Vec::new(),
        )
        .expect("grid");
    w.set_primary(grid);
    w
}

fn burst_file(n: usize, data: &[u8], props: &[(Property, bool)]) -> HeifWriter {
    let mut w = HeifWriter::new();
    let exif = b"MM\0*\0\0\0\x08\0\0";
    let mut first = None;
    for _ in 0..n {
        let id = w.add_coded_item(*b"hvc1", data.to_vec(), props.to_vec());
        w.add_exif(id, exif);
        first.get_or_insert(id);
    }
    w.set_primary(first.expect("n >= 1"));
    w
}

fn main() {
    let mut runs = 3usize;
    let mut decode = false;
    let mut counts: Vec<usize> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--runs" => runs = args.next().and_then(|v| v.parse().ok()).unwrap_or(3).max(1),
            "--decode" => decode = true,
            other => counts.push(other.parse().expect("an item count")),
        }
    }
    if counts.is_empty() {
        counts = vec![1024, 4096, 16384, 65536];
    }
    let (data, props) = tile();
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    println!(
        "| shape | items | file | write | parse | info | graph | check | items | open | packets (n) | remux |{}",
        if decode { " decode 1 w | 8 w |" } else { "" }
    );
    println!(
        "|---|---|---|---|---|---|---|---|---|---|---|---|{}",
        if decode { "---|---|" } else { "" }
    );
    for &n in &counts {
        for shape in ["grid", "burst"] {
            let w = if shape == "grid" {
                grid_file(n.min(65536), &data, &props)
            } else {
                burst_file(n, &data, &props)
            };
            let (t_write, bytes) = timed(runs, || w.write_to_vec().expect("write"));
            let (t_parse, _) = timed(runs, || {
                HeifFile::parse_borrowed(&bytes)
                    .expect("parse")
                    .meta()
                    .is_ok()
            });
            let file = HeifFile::parse_borrowed(&bytes).expect("parse");
            let meta = file.meta().expect("meta");
            let (t_info, _) = timed(runs, || oxideav_heif::info(&bytes).expect("info").frames);
            let (t_graph, _) = timed(runs, || {
                oxideav_heif::derived::build_primary_graph(&file)
                    .expect("graph")
                    .coded_items()
                    .len()
            });
            let (t_check, _) = timed(runs, || {
                oxideav_heif::miaf::check(&file, oxideav_heif::MiafProfile::Miaf)
                    .expect("check")
                    .violations
                    .len()
            });
            let (t_items, _) = timed(runs, || {
                let mut total = 0usize;
                for it in &meta.items {
                    total += ItemProperties::resolve(meta, it.id)
                        .expect("props")
                        .entries
                        .len();
                    total += file.item_data(it.id).expect("data").len();
                }
                total
            });
            let (t_open, _) = timed(runs, || {
                ctx.containers
                    .open_demuxer("heif", Box::new(Cursor::new(bytes.clone())), &ctx.codecs)
                    .expect("demuxer")
                    .streams()
                    .len()
            });
            let mut d = ctx
                .containers
                .open_demuxer("heif", Box::new(Cursor::new(bytes.clone())), &ctx.codecs)
                .expect("demuxer");
            let t = Instant::now();
            let mut packets = 0usize;
            while d.next_packet().is_ok() {
                packets += 1;
            }
            let t_packets = t.elapsed();
            let (t_remux, _) = timed(runs, || {
                HeifWriter::from_files(&[&bytes])
                    .expect("from_files")
                    .write_to_vec()
                    .expect("re-write")
                    .len()
            });
            let mut row = format!(
                "| {shape} | {} | {} KiB | {} | {} | {} | {} | {} | {} | {} | {} ({}) | {} |",
                meta.items.len(),
                bytes.len() / 1024,
                ms(t_write),
                ms(t_parse),
                ms(t_info),
                ms(t_graph),
                ms(t_check),
                ms(t_items),
                ms(t_open),
                ms(t_packets),
                packets,
                ms(t_remux),
            );
            if decode {
                if shape == "grid" {
                    for threads in [1usize, 8] {
                        let (t, _) = timed(1, || {
                            oxideav_heif::decode_with(
                                &bytes,
                                &oxideav_heif::DecodeOptions::default().with_threads(Some(threads)),
                            )
                            .expect("decode")
                            .width
                        });
                        row.push_str(&format!(" {} |", ms(t)));
                    }
                } else {
                    row.push_str(" — | — |");
                }
            }
            println!("{row}");
        }
    }
    println!("\n(times in ms, median of {runs})");
}
