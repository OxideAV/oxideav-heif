#![no_main]

//! Structured composition fuzz: the first bytes shape a `grid` or
//! `iovl` descriptor (parsed with the real parsers) and a set of
//! small synthetic input frames (layout, size, alpha, fill values);
//! the rest steer a clap / irot / imir chain. Canvas and tile sizes
//! are capped so the fuzzer probes geometry / offset / alpha edge
//! cases rather than allocation size.

use libfuzzer_sys::fuzz_target;
use oxideav_heif::compose::{
    apply_clap, apply_imir, apply_irot, attach_alpha, composite_grid, composite_overlay, crop,
    OverlayInput,
};
use oxideav_heif::derived::{GridDescriptor, OverlayDescriptor};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};
use oxideav_heif::props::{Clap, Imir, Irot};

const MAX_DIM: u32 = 40;

fn frame(seed: &[u8], w: u32, h: u32, chroma: Chroma, depth: u8, alpha: bool) -> Option<HeifFrame> {
    let fmt = HeifPixelFormat::new(chroma, depth, alpha).ok()?;
    let mut f = HeifFrame::filled(w, h, fmt, seed.first().copied().unwrap_or(0) as u16).ok()?;
    for (i, b) in seed.iter().enumerate().take(64) {
        let p = i % f.format.plane_count();
        let (pw, ph) = f.plane_dims(p);
        let x = (*b as u32) % pw;
        let y = (i as u32) % ph;
        f.set_sample(p, x, y, (*b as u16) << (depth - 8));
    }
    Some(f)
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let chroma = match data[0] & 3 {
        0 => Chroma::Mono,
        1 => Chroma::Yuv420,
        2 => Chroma::Yuv422,
        _ => Chroma::Yuv444,
    };
    let depth = match (data[0] >> 2) & 3 {
        0 => 8,
        1 => 10,
        2 => 12,
        _ => 16,
    };
    let alpha = data[0] & 0x10 != 0;
    let tw = 1 + (data[1] as u32) % MAX_DIM;
    let th = 1 + (data[2] as u32) % MAX_DIM;
    let kind = data[3] & 1;
    let body = &data[4..];
    if kind == 0 {
        // Grid: descriptor from the bytes, then tiles of tw×th.
        let Ok(mut g) = GridDescriptor::parse(body) else {
            return;
        };
        g.rows = 1 + (g.rows - 1) % 3;
        g.columns = 1 + (g.columns - 1) % 3;
        g.output_width = 1 + (g.output_width - 1) % (MAX_DIM * 3);
        g.output_height = 1 + (g.output_height - 1) % (MAX_DIM * 3);
        let mut tiles = Vec::new();
        for i in 0..g.tile_count() {
            let Some(t) = frame(&body[i.min(body.len().saturating_sub(1))..], tw, th, chroma, depth, alpha && i % 2 == 0) else {
                return;
            };
            tiles.push(t);
        }
        if let Ok(out) = composite_grid(&g, &tiles) {
            let _ = out.validate();
            transforms(&out, body);
        }
    } else {
        let n = 1 + (body.first().copied().unwrap_or(0) as usize) % 3;
        let Ok(mut o) = OverlayDescriptor::parse(body, n) else {
            return;
        };
        o.output_width = 1 + (o.output_width - 1) % (MAX_DIM * 2);
        o.output_height = 1 + (o.output_height - 1) % (MAX_DIM * 2);
        for off in o.offsets.iter_mut() {
            off.0 = off.0.clamp(-(MAX_DIM as i32) * 2, MAX_DIM as i32 * 2);
            off.1 = off.1.clamp(-(MAX_DIM as i32) * 2, MAX_DIM as i32 * 2);
        }
        let mut frames = Vec::new();
        for i in 0..n {
            let Some(f) = frame(&body[i.min(body.len().saturating_sub(1))..], tw, th, chroma, depth, alpha && i % 2 == 1) else {
                return;
            };
            frames.push((f, i % 2 == 0));
        }
        let inputs: Vec<OverlayInput<'_>> = frames
            .iter()
            .map(|(f, prem)| OverlayInput {
                frame: f,
                premultiplied: *prem,
            })
            .collect();
        if let Ok(out) = composite_overlay(&o, &inputs, None) {
            let _ = out.validate();
            transforms(&out, body);
        }
    }
});

fn transforms(f: &HeifFrame, steer: &[u8]) {
    let s = |i: usize| steer.get(i).copied().unwrap_or(0);
    let clap = Clap {
        width_n: 1 + (s(4) as u32) % MAX_DIM,
        width_d: 1 + (s(5) as u32) % 3,
        height_n: 1 + (s(6) as u32) % MAX_DIM,
        height_d: 1 + (s(7) as u32) % 3,
        horiz_off_n: s(8) as i8 as i32,
        horiz_off_d: 1 + (s(9) as u32) % 3,
        vert_off_n: s(10) as i8 as i32,
        vert_off_d: 1 + (s(11) as u32) % 3,
    };
    let cur = apply_clap(f, &clap).unwrap_or_else(|_| f.clone());
    let cur = apply_irot(&cur, &Irot { angle: s(12) & 3 }).unwrap_or(cur);
    let cur = apply_imir(&cur, &Imir { axis: s(13) & 1 }).unwrap_or(cur);
    let _ = crop(&cur, s(14) as u32 % cur.width, s(15) as u32 % cur.height, 1, 1);
    if let Some(a) = cur.alpha_as_frame() {
        let _ = attach_alpha(&cur.without_alpha(), &a);
    }
    let _ = cur.promote_to_444();
    let _ = cur.tight();
}
