//! `heifdump` — decode a HEIF / HEIC / AVIF file with this crate and
//! write the primary image as PNG and / or raw planar samples; print
//! a one-line summary of what was decoded.
//!
//! ```text
//! heifdump photo.heic [--png out.png] [--raw out.yuv] [--item ID]
//!          [--framework] [--tree]
//! ```
//!
//! * `--png`: RGB / RGBA (8-bit, or 16-bit for deeper sources), the
//!   `nclx` matrix / range applied ([`oxideav_heif::rgb::to_rgb`]).
//! * `--raw`: the composed planar frame, planes concatenated (8-bit
//!   samples as bytes, deeper samples as little-endian words) — the
//!   layout a `rawvideo` black-box decode of the same file produces.
//! * `--item ID`: decode that item instead of the primary.
//! * `--framework`: go through the registry demuxer + `"heif"` codec.
//! * `--tree`: print the box walk and the item table.

use std::io::Cursor;

use oxideav_heif::decode::{decode_item, decode_primary, ItemDecoder};
use oxideav_heif::rgb::to_rgb;
use oxideav_heif::{HeifFile, HeifFrame};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: heifdump <file> [--png out.png] [--raw out.yuv] [--item ID] [--framework] [--tree]");
        std::process::exit(2);
    }
    let mut png = None;
    let mut raw = None;
    let mut item = None;
    let mut framework = false;
    let mut tree = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--png" => {
                png = args.get(i + 1).cloned();
                i += 1;
            }
            "--raw" => {
                raw = args.get(i + 1).cloned();
                i += 1;
            }
            "--item" => {
                item = args.get(i + 1).and_then(|s| s.parse::<u32>().ok());
                i += 1;
            }
            "--framework" => framework = true,
            "--tree" => tree = true,
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let bytes = match std::fs::read(&args[0]) {
        Ok(b) => b,
        Err(e) => {
            println!("ERR read: {e}");
            std::process::exit(1);
        }
    };
    if tree {
        print_tree(&bytes);
    }
    let result = if framework {
        decode_framework(bytes)
    } else {
        decode_direct(&bytes, item)
    };
    let (frame, colr, info) = match result {
        Ok(r) => r,
        Err(e) => {
            println!("ERR {e}");
            std::process::exit(1);
        }
    };
    println!(
        "OK {}x{} {:?} {}-bit alpha={} {}",
        frame.width,
        frame.height,
        frame.format.chroma,
        frame.format.bit_depth,
        frame.format.has_alpha,
        info
    );
    if let Some(p) = raw {
        let t = frame.tight();
        let mut out = Vec::new();
        for pl in &t.planes {
            out.extend_from_slice(&pl.data);
        }
        std::fs::write(p, out).expect("write raw");
    }
    if let Some(p) = png {
        let rgb = to_rgb(&frame, colr.as_ref()).expect("rgb conversion");
        std::fs::write(p, encode_png(&rgb)).expect("write png");
    }
}

type Decoded = (HeifFrame, Option<oxideav_heif::props::Colr>, String);

fn decode_direct(bytes: &[u8], item: Option<u32>) -> Result<Decoded, String> {
    let file = HeifFile::parse(bytes).map_err(|e| format!("parse: {e}"))?;
    let img = match item {
        Some(id) => decode_item(&file, id, ItemDecoder::direct()),
        None => decode_primary(&file, ItemDecoder::direct()),
    }
    .map_err(|e| format!("decode: {e}"))?;
    let meta = file.meta().map_err(|e| e.to_string())?;
    let info = format!(
        "item={} items={} thumbs={} exif={} xmp={} icc={} nclx={:?} depth={} prem={}",
        img.item_id,
        meta.items.len(),
        img.thumbnail_ids.len(),
        img.exif.as_ref().map(Vec::len).unwrap_or(0),
        img.xmp.as_ref().map(String::len).unwrap_or(0),
        img.icc_profile.as_ref().map(Vec::len).unwrap_or(0),
        img.nclx,
        img.depth.is_some(),
        img.premultiplied_alpha
    );
    Ok((img.frame, Some(img.nclx), info))
}

fn decode_framework(bytes: Vec<u8>) -> Result<Decoded, String> {
    use oxideav_core::{Frame, RuntimeContext};
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let mut cur = Cursor::new(bytes.clone());
    let name = ctx
        .containers
        .probe_input(&mut cur, None)
        .map_err(|e| format!("probe: {e}"))?;
    if name != "heif" {
        return Err(format!("probe picked '{name}'"));
    }
    let mut demuxer = ctx
        .containers
        .open_demuxer(&name, Box::new(Cursor::new(bytes)), &ctx.codecs)
        .map_err(|e| format!("open: {e}"))?;
    let streams = demuxer.streams().to_vec();
    let still = streams
        .iter()
        .find(|s| s.params.codec_id.as_str() == "heif")
        .ok_or("no still stream")?;
    let pkt = demuxer.next_packet().map_err(|e| format!("packet: {e}"))?;
    if pkt.stream_index != still.index {
        return Err("first packet is not the still".into());
    }
    let mut dec = ctx
        .codecs
        .first_decoder(&still.params)
        .map_err(|e| format!("decoder: {e}"))?;
    dec.send_packet(&pkt).map_err(|e| format!("send: {e}"))?;
    dec.flush().map_err(|e| e.to_string())?;
    let Frame::Video(v) = dec.receive_frame().map_err(|e| format!("receive: {e}"))? else {
        return Err("not a video frame".into());
    };
    let pf = still
        .params
        .pixel_format
        .ok_or("no pixel format announced")?;
    let (w, h) = (
        still.params.width.ok_or("no width")?,
        still.params.height.ok_or("no height")?,
    );
    let frame = HeifFrame::from_core(&v, w, h, pf).map_err(|e| format!("from_core: {e}"))?;
    let info = format!("streams={} pf={pf:?}", streams.len());
    Ok((frame, None, info))
}

fn print_tree(bytes: &[u8]) {
    let file = match HeifFile::parse(bytes) {
        Ok(f) => f,
        Err(e) => {
            println!("TREE parse error: {e}");
            return;
        }
    };
    println!(
        "ftyp major={} compat={:?}",
        oxideav_heif::boxes::fourcc_str(&file.file_type.major_brand),
        file.file_type
            .compatible_brands
            .iter()
            .map(oxideav_heif::boxes::fourcc_str)
            .collect::<Vec<_>>()
    );
    if let Ok(walk) = file.box_walk() {
        for e in walk {
            println!("{e:?}");
        }
    }
    if let Ok(meta) = file.meta() {
        println!("primary={:?}", meta.primary_item_id);
        for it in &meta.items {
            let props = oxideav_heif::props::ItemProperties::resolve(meta, it.id);
            let props: Vec<String> = match props {
                Ok(p) => p
                    .iter()
                    .map(|e| {
                        format!(
                            "{}{}",
                            oxideav_heif::boxes::fourcc_str(&e.property.box_type()),
                            if e.essential { "!" } else { "" }
                        )
                    })
                    .collect(),
                Err(e) => vec![format!("<{e}>")],
            };
            println!(
                "item {} type={} hidden={} name={:?} props={}",
                it.id,
                oxideav_heif::boxes::fourcc_str(&it.item_type),
                it.is_hidden(),
                it.name,
                props.join(",")
            );
        }
        for r in &meta.references {
            println!(
                "iref {} {} -> {:?}",
                oxideav_heif::boxes::fourcc_str(&r.reference_type),
                r.from_item_id,
                r.to_item_ids
            );
        }
    }
}

// --- minimal PNG writer (stored deflate blocks) ---------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (n, t) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *t = c;
    }
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks = data.chunks(65535).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(c) = chunks.next() {
        let last = chunks.peek().is_none();
        out.push(last as u8);
        let len = c.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(c);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let mut c = kind.to_vec();
    c.extend_from_slice(body);
    out.extend_from_slice(&c);
    out.extend_from_slice(&crc32(&c).to_be_bytes());
}

fn encode_png(img: &oxideav_heif::rgb::RgbImage) -> Vec<u8> {
    let depth: u8 = if img.bit_depth > 8 { 16 } else { 8 };
    let colour_type = if img.channels == 4 { 6 } else { 2 };
    let bps = (depth / 8) as usize;
    let stride = img.width as usize * img.channels * bps;
    let mut raw = Vec::with_capacity((stride + 1) * img.height as usize);
    // Scale deeper-than-8 samples to 16 bits so PNG viewers agree.
    let max = ((1u32 << img.bit_depth) - 1) as f64;
    for y in 0..img.height as usize {
        raw.push(0);
        for v in &img.data
            [y * img.width as usize * img.channels..(y + 1) * img.width as usize * img.channels]
        {
            if depth == 8 {
                raw.push(*v as u8);
            } else {
                let s = (*v as f64 / max * 65535.0).round() as u16;
                raw.extend_from_slice(&s.to_be_bytes());
            }
        }
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&img.width.to_be_bytes());
    ihdr.extend_from_slice(&img.height.to_be_bytes());
    ihdr.extend_from_slice(&[depth, colour_type, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}
