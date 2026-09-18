//! Pixel composition of derived images and transformative properties
//! (ISO/IEC 23008-12 §6.3, §6.5.9–§6.5.12, §6.6.2, §6.9.1; ISO/IEC
//! 23000-22 §7.3.6.7), framework-free on [`HeifFrame`].
//!
//! * [`composite_grid`] — §6.6.2.3: tiles in row-major order, trimmed
//!   on the right / bottom to the declared output size.
//! * [`composite_overlay`] — §6.6.2.2: canvas filled with the sRGB
//!   `canvas_fill_value` (converted with the H.273 matrix the output
//!   colour information names), inputs painted bottom-most first at
//!   their offsets, clipped to the canvas, alpha planes blended with
//!   the §6.9.1 straight / pre-multiplied "visual context" update
//!   (per pixel, so alpha inputs promote the canvas to 4:4:4).
//!   A translucent fill yields an output alpha plane.
//! * [`apply_clap`] / [`apply_irot`] / [`apply_imir`] — the
//!   transformative properties; [`apply_transforms`] runs an item's
//!   chain in association order.
//! * [`attach_alpha`] — §6.9.1 alpha auxiliary attachment (resized to
//!   the master when the sizes differ, depth-matched).
//!
//! Chroma subsampling and odd geometry follow MIAF §7.3.6.7's rule for
//! `clap`: whenever an operation would need a sub-sample chroma
//! position (odd crop edge / size, odd overlay offset, odd grid tile
//! size, 90° rotation of an odd picture), the picture is first
//! implicitly promoted to 4:4:4 and the result stays 4:4:4.

use crate::derived::{GridDescriptor, OverlayDescriptor};
use crate::error::{HeifError, Result};
use crate::image::{Chroma, HeifFrame, HeifPixelFormat};
use crate::props::{Clap, Colr, Imir, Irot, Property, PropertyEntry};

#[doc(hidden)]
/// Whether an operation touching an odd luma column (`x_odd`) or row
/// (`y_odd`) in a `chroma`-subsampled picture needs a chroma sample
/// that does not exist — the trigger for the implicit 4:4:4 promotion.
pub fn needs_444(chroma: Chroma, x_odd: bool, y_odd: bool) -> bool {
    let (sx, sy) = chroma.shift();
    (sx == 1 && x_odd) || (sy == 1 && y_odd)
}

fn ensure_444(f: &HeifFrame, x_odd: bool, y_odd: bool) -> Result<std::borrow::Cow<'_, HeifFrame>> {
    if needs_444(f.format.chroma, x_odd, y_odd) {
        Ok(std::borrow::Cow::Owned(f.promote_to_444()?))
    } else {
        Ok(std::borrow::Cow::Borrowed(f))
    }
}

fn same_layout(a: &HeifFrame, b: &HeifFrame) -> bool {
    a.format.chroma == b.format.chroma && a.format.bit_depth == b.format.bit_depth
}

/// Copy the `w × h` block of `src` at `(sx, sy)` into `dst` at `(dx, dy)`
/// (luma coordinates; every plane copied at its own resolution). The
/// caller guarantees the chroma alignment.
fn blit(
    dst: &mut HeifFrame,
    (dx, dy): (u32, u32),
    src: &HeifFrame,
    (sx, sy): (u32, u32),
    (w, h): (u32, u32),
) {
    let bps = dst.format.bytes_per_sample();
    let planes = dst.format.plane_count().min(src.format.plane_count());
    for p in 0..planes {
        let (shx, shy) = if p == 0 || Some(p) == dst.format.alpha_plane() {
            (0, 0)
        } else {
            dst.format.chroma.shift()
        };
        let pw = (w + (1 << shx) - 1) >> shx;
        let ph = (h + (1 << shy) - 1) >> shy;
        let (dpw, dph) = dst.plane_dims(p);
        let (spw, sph) = src.plane_dims(p);
        let pw = pw
            .min(dpw.saturating_sub(dx >> shx))
            .min(spw.saturating_sub(sx >> shx));
        let ph = ph
            .min(dph.saturating_sub(dy >> shy))
            .min(sph.saturating_sub(sy >> shy));
        let row_bytes = pw as usize * bps;
        for r in 0..ph as usize {
            let s_off =
                ((sy >> shy) as usize + r) * src.planes[p].stride + (sx >> shx) as usize * bps;
            let d_off =
                ((dy >> shy) as usize + r) * dst.planes[p].stride + (dx >> shx) as usize * bps;
            let src_row = &src.planes[p].data[s_off..s_off + row_bytes];
            dst.planes[p].data[d_off..d_off + row_bytes].copy_from_slice(src_row);
        }
    }
}

/// Crop `f` to the `w × h` rectangle at `(x, y)`.
pub fn crop(f: &HeifFrame, x: u32, y: u32, w: u32, h: u32) -> Result<HeifFrame> {
    if w == 0
        || h == 0
        || x as u64 + w as u64 > f.width as u64
        || y as u64 + h as u64 > f.height as u64
    {
        return Err(HeifError::invalid(format!(
            "crop {w}x{h}@({x},{y}) outside the {}x{} picture",
            f.width, f.height
        )));
    }
    let src = ensure_444(f, x % 2 == 1 || w % 2 == 1, y % 2 == 1 || h % 2 == 1)?;
    let mut out = HeifFrame::zeroed(w, h, src.format)?;
    blit(&mut out, (0, 0), &src, (x, y), (w, h));
    Ok(out)
}

/// Apply a `clap` property (§6.5.9).
pub fn apply_clap(f: &HeifFrame, clap: &Clap) -> Result<HeifFrame> {
    let r = clap.resolve(f.width, f.height)?;
    crop(f, r.x, r.y, r.width, r.height)
}

/// Apply an `irot` property (§6.5.10): `angle × 90°` anti-clockwise.
pub fn apply_irot(f: &HeifFrame, irot: &Irot) -> Result<HeifFrame> {
    let angle = irot.angle & 3;
    if angle == 0 {
        return Ok(f.clone());
    }
    let src = ensure_444(f, f.width % 2 == 1, f.height % 2 == 1)?;
    let (ow, oh) = if angle % 2 == 1 {
        (src.height, src.width)
    } else {
        (src.width, src.height)
    };
    let mut out = HeifFrame::zeroed(ow, oh, src.format)?;
    for p in 0..src.format.plane_count() {
        let (pw, ph) = src.plane_dims(p);
        for y in 0..ph {
            for x in 0..pw {
                let v = src.sample(p, x, y);
                // Anti-clockwise rotation by 90°: (x, y) → (y, pw − 1 − x).
                let (nx, ny) = match angle {
                    1 => (y, pw - 1 - x),
                    2 => (pw - 1 - x, ph - 1 - y),
                    _ => (ph - 1 - y, x),
                };
                out.set_sample(p, nx, ny, v);
            }
        }
    }
    Ok(out)
}

/// Apply an `imir` property (§6.5.12): axis 0 exchanges top and bottom,
/// axis 1 exchanges left and right.
pub fn apply_imir(f: &HeifFrame, imir: &Imir) -> Result<HeifFrame> {
    let mut out = f.tight();
    let bps = out.format.bytes_per_sample();
    for p in 0..out.format.plane_count() {
        let (pw, ph) = out.plane_dims(p);
        let stride = out.planes[p].stride;
        let data = &mut out.planes[p].data;
        if imir.axis & 1 == 0 {
            for y in 0..(ph / 2) as usize {
                let (a, b) = (y * stride, (ph as usize - 1 - y) * stride);
                let (lo, hi) = data.split_at_mut(b);
                lo[a..a + stride].swap_with_slice(&mut hi[..stride]);
            }
        } else {
            for y in 0..ph as usize {
                let row = &mut data[y * stride..y * stride + pw as usize * bps];
                if bps == 1 {
                    row.reverse();
                } else {
                    let n = pw as usize;
                    for i in 0..n / 2 {
                        let j = n - 1 - i;
                        let (a0, a1) = (row[2 * i], row[2 * i + 1]);
                        row[2 * i] = row[2 * j];
                        row[2 * i + 1] = row[2 * j + 1];
                        row[2 * j] = a0;
                        row[2 * j + 1] = a1;
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Run an item's transformative chain in association order (§6.3).
/// `essential_only` skips non-essential transforms (a permitted reader
/// choice); the default reader applies every recognised transform.
pub fn apply_transforms<'a>(
    f: &HeifFrame,
    chain: impl IntoIterator<Item = &'a PropertyEntry>,
    essential_only: bool,
) -> Result<HeifFrame> {
    let mut cur: Option<HeifFrame> = None;
    for e in chain {
        if essential_only && !e.essential {
            continue;
        }
        let input = cur.as_ref().unwrap_or(f);
        let next = match &e.property {
            Property::Clap(c) => apply_clap(input, c)?,
            Property::Irot(r) => apply_irot(input, r)?,
            Property::Imir(m) => apply_imir(input, m)?,
            Property::Iscl(_) => {
                return Err(HeifError::unsupported(
                    "iscl (image scaling) transformative property",
                ))
            }
            _ => continue,
        };
        cur = Some(next);
    }
    Ok(cur.unwrap_or_else(|| f.clone()))
}

/// Nearest-neighbour resize of a monochrome / planar frame.
pub fn resize_nearest(f: &HeifFrame, w: u32, h: u32) -> Result<HeifFrame> {
    if (w, h) == (f.width, f.height) {
        return Ok(f.clone());
    }
    let mut out = HeifFrame::zeroed(w, h, f.format)?;
    for p in 0..f.format.plane_count() {
        let (sw, sh) = f.plane_dims(p);
        let (dw, dh) = out.plane_dims(p);
        for y in 0..dh {
            let sy = ((y as u64 * sh as u64) / dh as u64) as u32;
            for x in 0..dw {
                let sx = ((x as u64 * sw as u64) / dw as u64) as u32;
                out.set_sample(p, x, y, f.sample(p, sx, sy));
            }
        }
    }
    Ok(out)
}

/// Rescale every sample of a monochrome frame from `f.format.bit_depth`
/// to `depth` (shift, rounding on narrowing).
pub fn rescale_depth(f: &HeifFrame, depth: u8) -> Result<HeifFrame> {
    if f.format.bit_depth == depth {
        return Ok(f.clone());
    }
    let fmt = HeifPixelFormat::new(f.format.chroma, depth, f.format.has_alpha)?;
    let mut out = HeifFrame::zeroed(f.width, f.height, fmt)?;
    let from = f.format.bit_depth;
    for p in 0..f.format.plane_count() {
        let (pw, ph) = f.plane_dims(p);
        for y in 0..ph {
            for x in 0..pw {
                let v = f.sample(p, x, y) as u32;
                let nv = if depth > from {
                    v << (depth - from)
                } else {
                    let s = from - depth;
                    ((v + (1 << (s - 1))) >> s).min((1 << depth) - 1)
                };
                out.set_sample(p, x, y, nv as u16);
            }
        }
    }
    Ok(out)
}

/// Attach an alpha auxiliary (§6.9.1) to `master`: the auxiliary's luma
/// plane is used, resized to the master's size when they differ, and
/// rescaled to the master's bit depth.
pub fn attach_alpha(master: &HeifFrame, alpha: &HeifFrame) -> Result<HeifFrame> {
    let luma = HeifFrame {
        width: alpha.width,
        height: alpha.height,
        format: HeifPixelFormat::new(Chroma::Mono, alpha.format.bit_depth, false)?,
        planes: vec![alpha.planes[0].clone()],
    };
    let sized = resize_nearest(&luma, master.width, master.height)?;
    let depth = rescale_depth(&sized, master.format.bit_depth)?;
    master.with_alpha_plane(&depth)
}

/// §6.6.2.3 grid composition. Every tile must share one layout and one
/// size; tiles carrying alpha planes produce an output alpha plane
/// (tiles without one count as opaque).
pub fn composite_grid(desc: &GridDescriptor, tiles: &[HeifFrame]) -> Result<HeifFrame> {
    if tiles.len() != desc.tile_count() {
        return Err(HeifError::invalid(format!(
            "grid: {} tiles for a {}x{} layout",
            tiles.len(),
            desc.rows,
            desc.columns
        )));
    }
    let first = &tiles[0];
    let (tw, th) = (first.width, first.height);
    for (i, t) in tiles.iter().enumerate() {
        if t.width != tw || t.height != th {
            return Err(HeifError::invalid(format!(
                "grid: tile {i} is {}x{}, tile 0 is {tw}x{th}",
                t.width, t.height
            )));
        }
        if !same_layout(t, first) {
            return Err(HeifError::unsupported(format!(
                "grid: tile {i} layout {:?} differs from tile 0 {:?}",
                t.format, first.format
            )));
        }
    }
    let cols = desc.columns as u64;
    let rows = desc.rows as u64;
    if tw as u64 * cols < desc.output_width as u64 || th as u64 * rows < desc.output_height as u64 {
        return Err(HeifError::invalid(format!(
            "grid: {cols}x{rows} tiles of {tw}x{th} do not cover the {}x{} canvas",
            desc.output_width, desc.output_height
        )));
    }
    let any_alpha = tiles.iter().any(|t| t.format.has_alpha);
    // Odd tile sizes in a subsampled layout put tile origins on odd
    // chroma positions: promote.
    let promote = needs_444(first.format.chroma, tw % 2 == 1, th % 2 == 1)
        || needs_444(
            first.format.chroma,
            desc.output_width % 2 == 1,
            desc.output_height % 2 == 1,
        );
    let chroma = if promote {
        Chroma::Yuv444
    } else {
        first.format.chroma
    };
    let fmt = HeifPixelFormat::new(chroma, first.format.bit_depth, any_alpha)?;
    let full_w = (tw as u64 * cols) as u32;
    let full_h = (th as u64 * rows) as u32;
    let mut canvas = HeifFrame::filled(full_w, full_h, fmt, 0)?;
    if let Some(a) = fmt.alpha_plane() {
        let max = fmt.max_value();
        let plane = &mut canvas.planes[a];
        if fmt.bytes_per_sample() == 1 {
            plane.data.fill(max as u8);
        } else {
            for px in plane.data.chunks_exact_mut(2) {
                px.copy_from_slice(&max.to_le_bytes());
            }
        }
    }
    for (i, t) in tiles.iter().enumerate() {
        let (r, c) = ((i as u64 / cols) as u32, (i as u64 % cols) as u32);
        let src = if promote {
            std::borrow::Cow::Owned(t.promote_to_444()?)
        } else {
            std::borrow::Cow::Borrowed(t)
        };
        blit(&mut canvas, (c * tw, r * th), &src, (0, 0), (tw, th));
    }
    if full_w == desc.output_width && full_h == desc.output_height {
        return Ok(canvas);
    }
    crop(&canvas, 0, 0, desc.output_width, desc.output_height)
}

/// One overlay input: its decoded output image (alpha plane attached
/// when it has an alpha auxiliary) and whether its colour samples are
/// pre-multiplied by that alpha (`prem` reference).
#[derive(Clone, Debug)]
pub struct OverlayInput<'a> {
    /// The input image.
    pub frame: &'a HeifFrame,
    /// `prem` reference present from the master to its alpha.
    pub premultiplied: bool,
}

/// Convert an sRGB `canvas_fill_value` (16-bit R, G, B) to the coded
/// YCbCr triple of an output described by `colr` (H.273 matrices; the
/// MIAF §7.3.6.4 default when `None`).
pub fn fill_to_ycbcr(fill: [u16; 3], depth: u8, colr: Option<&Colr>) -> [u16; 3] {
    let (matrix, full_range) = match colr {
        Some(Colr::Nclx {
            matrix, full_range, ..
        }) => (*matrix, *full_range),
        _ => (6, true),
    };
    let max = ((1u32 << depth) - 1) as f64;
    let r = fill[0] as f64 / 65535.0;
    let g = fill[1] as f64 / 65535.0;
    let b = fill[2] as f64 / 65535.0;
    let scale = |v: f64| (v * max).round().clamp(0.0, max) as u16;
    // H.273 equations 38–41 / Table 4 constants.
    let (kr, kb) = match matrix {
        0 => {
            // Identity (GBR): Y = G, Cb = B, Cr = R (equations 41–43).
            return [scale(g), scale(b), scale(r)];
        }
        1 => (0.2126, 0.0722),
        4 => (0.30, 0.11),
        5 | 6 | 2 => (0.299, 0.114),
        7 => (0.212, 0.087),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.299, 0.114),
    };
    let kg = 1.0 - kr - kb;
    let y = kr * r + kg * g + kb * b;
    let cb = (b - y) / (2.0 * (1.0 - kb));
    let cr = (r - y) / (2.0 * (1.0 - kr));
    let mid = (1u32 << (depth - 1)) as f64;
    let sh = (1u32 << (depth - 8)) as f64;
    if full_range {
        [
            scale(y),
            (cb * max + mid).round().clamp(0.0, max) as u16,
            (cr * max + mid).round().clamp(0.0, max) as u16,
        ]
    } else {
        [
            ((219.0 * y + 16.0) * sh).round().clamp(0.0, max) as u16,
            ((224.0 * cb + 128.0) * sh).round().clamp(0.0, max) as u16,
            ((224.0 * cr + 128.0) * sh).round().clamp(0.0, max) as u16,
        ]
    }
}

/// §6.6.2.2 overlay composition. Every input must share one layout
/// (chroma, depth). `colr` describes the output (used for the fill
/// colour conversion); the fill's `A` seeds the canvas opacity — a
/// translucent fill yields an output alpha plane.
pub fn composite_overlay(
    desc: &OverlayDescriptor,
    inputs: &[OverlayInput<'_>],
    colr: Option<&Colr>,
) -> Result<HeifFrame> {
    if inputs.len() != desc.offsets.len() {
        return Err(HeifError::invalid(format!(
            "iovl: {} inputs for {} offsets",
            inputs.len(),
            desc.offsets.len()
        )));
    }
    let first = inputs
        .first()
        .ok_or_else(|| HeifError::invalid("iovl: no inputs"))?
        .frame;
    for (i, inp) in inputs.iter().enumerate() {
        if !same_layout(inp.frame, first) {
            return Err(HeifError::unsupported(format!(
                "iovl: input {i} layout {:?} differs from input 0 {:?}",
                inp.frame.format, first.format
            )));
        }
    }
    let depth = first.format.bit_depth;
    let max = ((1u32 << depth) - 1) as u64;
    let fill_alpha = (desc.canvas_fill[3] as u64 * max + 32767) / 65535;
    let canvas_translucent = fill_alpha < max;
    // Odd offsets or canvas sizes in a subsampled layout promote to
    // 4:4:4; so does any alpha-carrying input, because §6.9.1 blends
    // "each co-located pixel" and a subsampled chroma plane cannot hold
    // a per-pixel edge (the alpha boundary would smear over 2x2
    // blocks).
    let any_alpha = inputs.iter().any(|i| i.frame.format.has_alpha);
    let promote = any_alpha
        || needs_444(
            first.format.chroma,
            desc.offsets.iter().any(|(h, _)| h.rem_euclid(2) == 1) || desc.output_width % 2 == 1,
            desc.offsets.iter().any(|(_, v)| v.rem_euclid(2) == 1) || desc.output_height % 2 == 1,
        )
        || inputs.iter().any(|i| {
            needs_444(
                first.format.chroma,
                i.frame.width % 2 == 1,
                i.frame.height % 2 == 1,
            )
        });
    let chroma = if promote {
        Chroma::Yuv444
    } else {
        first.format.chroma
    };
    let out_fmt = HeifPixelFormat::new(chroma, depth, canvas_translucent)?;
    let colour_planes = chroma.colour_planes();
    let fill = fill_to_ycbcr(
        [
            desc.canvas_fill[0],
            desc.canvas_fill[1],
            desc.canvas_fill[2],
        ],
        depth,
        colr,
    );
    // Working canvas: colour pre-multiplied by canvas opacity, plus an
    // opacity plane, all in u64 fixed point scaled by `max`.
    let (ow, oh) = (desc.output_width, desc.output_height);
    let mut colour: Vec<Vec<u64>> = Vec::with_capacity(colour_planes);
    let mut dims: Vec<(u32, u32)> = Vec::with_capacity(colour_planes);
    for (p, &fv) in fill.iter().enumerate().take(colour_planes) {
        let (pw, ph) = out_fmt.plane_dims(p, ow, oh);
        dims.push((pw, ph));
        let v = fv as u64 * fill_alpha;
        colour.push(vec![v; pw as usize * ph as usize]);
    }
    let mut opacity: Vec<u64> = vec![fill_alpha; ow as usize * oh as usize];
    let (shx, shy) = chroma.shift();
    for (inp, &(hoff, voff)) in inputs.iter().zip(&desc.offsets) {
        let src = if promote {
            std::borrow::Cow::Owned(inp.frame.promote_to_444()?)
        } else {
            std::borrow::Cow::Borrowed(inp.frame)
        };
        let alpha_plane = src.format.alpha_plane();
        // Visible luma rectangle of this input on the canvas.
        let x0 = hoff.max(0) as i64;
        let y0 = voff.max(0) as i64;
        let x1 = (hoff as i64 + src.width as i64).min(ow as i64);
        let y1 = (voff as i64 + src.height as i64).min(oh as i64);
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        for p in 0..colour_planes {
            let (sx, sy) = if p == 0 { (0, 0) } else { (shx, shy) };
            let (pw, _) = dims[p];
            let px0 = x0 >> sx;
            let py0 = y0 >> sy;
            let px1 = (x1 + (1 << sx) - 1) >> sx;
            let py1 = (y1 + (1 << sy) - 1) >> sy;
            for cy in py0..py1 {
                for cx in px0..px1 {
                    // Source sample position in this plane.
                    let lx = (cx << sx) - hoff as i64;
                    let ly = (cy << sy) - voff as i64;
                    let sxp = (lx >> sx) as u32;
                    let syp = (ly >> sy) as u32;
                    let m = src.sample(p, sxp, syp) as u64;
                    // Alpha of this sample: average over the covered luma block.
                    let a = match alpha_plane {
                        None => max,
                        Some(ap) => {
                            let mut sum = 0u64;
                            let mut n = 0u64;
                            for dy in 0..(1 << sy) {
                                for dx in 0..(1 << sx) {
                                    let ax = lx + dx;
                                    let ay = ly + dy;
                                    if ax >= 0
                                        && ay >= 0
                                        && (ax as u32) < src.width
                                        && (ay as u32) < src.height
                                    {
                                        sum += src.sample(ap, ax as u32, ay as u32) as u64;
                                        n += 1;
                                    }
                                }
                            }
                            (sum + n / 2).checked_div(n).unwrap_or(0)
                        }
                    };
                    let idx = cy as usize * pw as usize + cx as usize;
                    let vi = colour[p][idx]; // pre-multiplied by canvas opacity
                                             // Straight: vu = m·α + vi·(1 − α); pre-multiplied: vu = m + vi·(1 − α).
                    let m_term = if inp.premultiplied { m * max } else { m * a };
                    colour[p][idx] = (m_term + (vi * (max - a) + max / 2) / max).min(max * max);
                }
            }
        }
        // Opacity: α + a·(1 − α), on the luma grid.
        for cy in y0..y1 {
            for cx in x0..x1 {
                let a = match alpha_plane {
                    None => max,
                    Some(ap) => {
                        src.sample(ap, (cx - hoff as i64) as u32, (cy - voff as i64) as u32) as u64
                    }
                };
                let idx = cy as usize * ow as usize + cx as usize;
                let ci = opacity[idx];
                opacity[idx] = (a + (ci * (max - a) + max / 2) / max).min(max);
            }
        }
    }
    // Resolve: un-premultiply by the canvas opacity.
    let mut out = HeifFrame::zeroed(ow, oh, out_fmt)?;
    for p in 0..colour_planes {
        let (pw, ph) = dims[p];
        let (sx, sy) = if p == 0 { (0, 0) } else { (shx, shy) };
        for y in 0..ph {
            for x in 0..pw {
                let idx = y as usize * pw as usize + x as usize;
                let pre = colour[p][idx];
                // Opacity at this plane position: average of the covered block.
                let mut sum = 0u64;
                let mut n = 0u64;
                for dy in 0..(1u32 << sy) {
                    for dx in 0..(1u32 << sx) {
                        let ax = (x << sx) + dx;
                        let ay = (y << sy) + dy;
                        if ax < ow && ay < oh {
                            sum += opacity[ay as usize * ow as usize + ax as usize];
                            n += 1;
                        }
                    }
                }
                let a = (sum + n / 2).checked_div(n).unwrap_or(0);
                // Fully transparent (a == 0): keep the fill colour.
                let v = match (pre + a / 2).checked_div(a) {
                    Some(q) => q.min(max),
                    None => fill[p] as u64,
                };
                out.set_sample(p, x, y, v as u16);
            }
        }
    }
    if let Some(ap) = out_fmt.alpha_plane() {
        for y in 0..oh {
            for x in 0..ow {
                let a = opacity[y as usize * ow as usize + x as usize].min(max);
                out.set_sample(ap, x, y, a as u16);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(chroma: Chroma, depth: u8, alpha: bool) -> HeifPixelFormat {
        HeifPixelFormat::new(chroma, depth, alpha).unwrap()
    }

    /// A 4×4 4:4:4 8-bit frame whose luma is `y * 16 + x`.
    fn ramp(w: u32, h: u32, chroma: Chroma) -> HeifFrame {
        let mut f = HeifFrame::zeroed(w, h, fmt(chroma, 8, false)).unwrap();
        for y in 0..h {
            for x in 0..w {
                f.set_sample(0, x, y, (y * 16 + x) as u16);
            }
        }
        if chroma != Chroma::Mono {
            let (cw, ch) = f.plane_dims(1);
            for y in 0..ch {
                for x in 0..cw {
                    f.set_sample(1, x, y, (100 + y * 16 + x) as u16);
                    f.set_sample(2, x, y, (200 + y * 16 + x) as u16);
                }
            }
        }
        f
    }

    #[test]
    fn crop_even_keeps_chroma_layout_odd_promotes() {
        let f = ramp(8, 8, Chroma::Yuv420);
        let c = crop(&f, 2, 4, 4, 2).unwrap();
        assert_eq!(c.format.chroma, Chroma::Yuv420);
        assert_eq!(c.sample(0, 0, 0), 4 * 16 + 2);
        assert_eq!(c.sample(1, 0, 0), 100 + 2 * 16 + 1);
        let o = crop(&f, 1, 0, 3, 3).unwrap();
        assert_eq!(o.format.chroma, Chroma::Yuv444);
        assert_eq!(o.sample(0, 0, 0), 1);
        assert_eq!(o.sample(1, 0, 0), 100, "chroma replicated from (0,0)");
        assert!(crop(&f, 7, 0, 2, 2).is_err());
    }

    #[test]
    fn clap_1x1_from_64x64() {
        let f = ramp(64, 64, Chroma::Yuv420);
        let c = Clap {
            width_n: 1,
            width_d: 1,
            height_n: 1,
            height_d: 1,
            horiz_off_n: -63,
            horiz_off_d: 2,
            vert_off_n: -63,
            vert_off_d: 2,
        };
        let out = apply_clap(&f, &c).unwrap();
        assert_eq!((out.width, out.height), (1, 1));
        assert_eq!(out.sample(0, 0, 0), 0);
    }

    #[test]
    fn rotation_and_mirroring() {
        let f = ramp(4, 2, Chroma::Yuv444);
        let r = apply_irot(&f, &Irot { angle: 1 }).unwrap();
        assert_eq!((r.width, r.height), (2, 4));
        // Anti-clockwise 90°: the top-right source pixel (3,0) lands at
        // the top-left (0,0); (0,0) lands at the bottom-left (0,3).
        assert_eq!(r.sample(0, 0, 0), 3);
        assert_eq!(r.sample(0, 0, 3), 0);
        assert_eq!(r.sample(0, 1, 0), 16 + 3);
        let r2 = apply_irot(&f, &Irot { angle: 2 }).unwrap();
        assert_eq!(r2.sample(0, 0, 0), 16 + 3);
        let r3 = apply_irot(&f, &Irot { angle: 3 }).unwrap();
        assert_eq!(r3.sample(0, 0, 0), 16);
        assert_eq!(r3.sample(0, 1, 3), 3);
        // Four quarter turns are the identity.
        let mut q = f.clone();
        for _ in 0..4 {
            q = apply_irot(&q, &Irot { angle: 1 }).unwrap();
        }
        assert_eq!(q, f);
        let v = apply_imir(&f, &Imir { axis: 0 }).unwrap();
        assert_eq!(v.sample(0, 0, 0), 16);
        let h = apply_imir(&f, &Imir { axis: 1 }).unwrap();
        assert_eq!(h.sample(0, 0, 0), 3);
        assert_eq!(h.sample(2, 0, 1), 200 + 16 + 3);
        // Odd 4:2:0 rotation promotes to 4:4:4.
        let odd = ramp(3, 2, Chroma::Yuv420);
        let ro = apply_irot(&odd, &Irot { angle: 1 }).unwrap();
        assert_eq!(ro.format.chroma, Chroma::Yuv444);
        assert_eq!((ro.width, ro.height), (2, 3));
        // 16-bit horizontal mirror.
        let mut deep = HeifFrame::zeroed(3, 1, fmt(Chroma::Mono, 10, false)).unwrap();
        deep.set_sample(0, 0, 0, 1000);
        deep.set_sample(0, 2, 0, 7);
        let m = apply_imir(&deep, &Imir { axis: 1 }).unwrap();
        assert_eq!(m.sample(0, 0, 0), 7);
        assert_eq!(m.sample(0, 2, 0), 1000);
    }

    #[test]
    fn transform_chain_order_and_essential_filter() {
        let f = ramp(4, 2, Chroma::Yuv444);
        let chain = vec![
            PropertyEntry {
                index: 1,
                essential: true,
                property: Property::Irot(Irot { angle: 1 }),
            },
            PropertyEntry {
                index: 2,
                essential: false,
                property: Property::Imir(Imir { axis: 1 }),
            },
        ];
        let all = apply_transforms(&f, &chain, false).unwrap();
        assert_eq!((all.width, all.height), (2, 4));
        assert_eq!(all.sample(0, 0, 0), 16 + 3);
        let ess = apply_transforms(&f, &chain, true).unwrap();
        assert_eq!(ess.sample(0, 0, 0), 3);
        let iscl = vec![PropertyEntry {
            index: 1,
            essential: true,
            property: Property::Iscl(crate::props::Iscl {
                width_num: 1,
                width_den: 2,
                height_num: 1,
                height_den: 2,
            }),
        }];
        assert!(apply_transforms(&f, &iscl, false).is_err());
    }

    #[test]
    fn grid_assembles_row_major_and_trims() {
        let mk = |v: u16| HeifFrame::filled(2, 2, fmt(Chroma::Yuv420, 8, false), v).unwrap();
        let tiles = [mk(1), mk(2), mk(3), mk(4)];
        let d = GridDescriptor {
            rows: 2,
            columns: 2,
            output_width: 3,
            output_height: 4,
        };
        let g = composite_grid(&d, &tiles).unwrap();
        assert_eq!((g.width, g.height), (3, 4));
        assert_eq!(g.format.chroma, Chroma::Yuv444, "odd output width promotes");
        assert_eq!(g.sample(0, 0, 0), 1);
        assert_eq!(g.sample(0, 2, 0), 2);
        assert_eq!(g.sample(0, 0, 3), 3);
        assert_eq!(g.sample(0, 2, 3), 4);
        let d4 = GridDescriptor {
            output_width: 4,
            output_height: 4,
            ..d
        };
        let g4 = composite_grid(&d4, &tiles).unwrap();
        assert_eq!(g4.format.chroma, Chroma::Yuv420);
        assert_eq!(g4.sample(1, 1, 1), 4);
        assert!(composite_grid(&d, &tiles[..3]).is_err());
        let big = GridDescriptor {
            output_width: 5,
            ..d
        };
        assert!(composite_grid(&big, &tiles).is_err());
        // Alpha on one tile → output alpha, other tiles opaque.
        let mut with_a = mk(9)
            .with_alpha_plane(&HeifFrame::filled(2, 2, fmt(Chroma::Mono, 8, false), 10).unwrap())
            .unwrap();
        with_a.format.has_alpha = true;
        let tiles_a = [with_a, mk(2), mk(3), mk(4)];
        let ga = composite_grid(&d4, &tiles_a).unwrap();
        assert!(ga.format.has_alpha);
        assert_eq!(ga.sample(3, 0, 0), 10);
        assert_eq!(ga.sample(3, 3, 3), 255);
    }

    #[test]
    fn fill_colour_conversion() {
        // Neutral grey is depth-scaled luma with mid chroma for every matrix.
        assert_eq!(
            fill_to_ycbcr([16384, 16384, 16384], 8, None),
            [64, 128, 128]
        );
        assert_eq!(
            fill_to_ycbcr([65535, 65535, 65535], 10, None),
            [1023, 512, 512]
        );
        let limited = Colr::Nclx {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
        };
        assert_eq!(
            fill_to_ycbcr([65535, 65535, 65535], 8, Some(&limited)),
            [235, 128, 128]
        );
        assert_eq!(fill_to_ycbcr([0, 0, 0], 8, Some(&limited)), [16, 128, 128]);
        // Identity matrix keeps GBR.
        let id = Colr::Nclx {
            primaries: 1,
            transfer: 13,
            matrix: 0,
            full_range: true,
        };
        assert_eq!(
            fill_to_ycbcr([65535, 0, 32768], 8, Some(&id)),
            [0, 128, 255]
        );
        // Pure red, BT.601 full range: Y ≈ 76, Cb ≈ 85, Cr = 255.
        assert_eq!(fill_to_ycbcr([65535, 0, 0], 8, None), [76, 85, 255]);
    }

    #[test]
    fn overlay_paints_offsets_and_alpha() {
        let base = HeifFrame::filled(4, 4, fmt(Chroma::Yuv444, 8, false), 10).unwrap();
        let mut stamp = HeifFrame::filled(2, 2, fmt(Chroma::Yuv444, 8, true), 200).unwrap();
        // Alpha: left column opaque, right column half.
        for y in 0..2 {
            stamp.set_sample(3, 0, y, 255);
            stamp.set_sample(3, 1, y, 128);
        }
        let d = OverlayDescriptor {
            canvas_fill: [0, 0, 0, 65535],
            output_width: 4,
            output_height: 4,
            offsets: vec![(0, 0), (1, 1)],
        };
        let out = composite_overlay(
            &d,
            &[
                OverlayInput {
                    frame: &base,
                    premultiplied: false,
                },
                OverlayInput {
                    frame: &stamp,
                    premultiplied: false,
                },
            ],
            None,
        )
        .unwrap();
        assert!(!out.format.has_alpha);
        assert_eq!(out.sample(0, 0, 0), 10);
        assert_eq!(out.sample(0, 1, 1), 200, "opaque stamp column");
        // half: 200*128/255 + 10*127/255 ≈ 100.4 + 4.98 = 105.
        assert_eq!(out.sample(0, 2, 1), 105);
        assert_eq!(out.sample(0, 3, 3), 10);
        // Pre-multiplied input: vu = m + vi(1-α).
        let out_p = composite_overlay(
            &d,
            &[
                OverlayInput {
                    frame: &base,
                    premultiplied: false,
                },
                OverlayInput {
                    frame: &stamp,
                    premultiplied: true,
                },
            ],
            None,
        )
        .unwrap();
        assert_eq!(out_p.sample(0, 2, 1), 205);
        // Negative offset clips; canvas outside inputs shows the fill.
        let d2 = OverlayDescriptor {
            canvas_fill: [65535, 65535, 65535, 65535],
            output_width: 3,
            output_height: 3,
            offsets: vec![(-2, -2)],
        };
        let o2 = composite_overlay(
            &d2,
            &[OverlayInput {
                frame: &base,
                premultiplied: false,
            }],
            None,
        )
        .unwrap();
        assert_eq!(o2.sample(0, 0, 0), 10);
        assert_eq!(o2.sample(0, 2, 2), 255);
        assert_eq!(o2.sample(1, 2, 2), 128);
        // Translucent canvas → output alpha; a transparent hole keeps
        // the fill colour with alpha 0.
        let d3 = OverlayDescriptor {
            canvas_fill: [0, 0, 0, 0],
            output_width: 4,
            output_height: 4,
            offsets: vec![(1, 1)],
        };
        let o3 = composite_overlay(
            &d3,
            &[OverlayInput {
                frame: &stamp,
                premultiplied: false,
            }],
            None,
        )
        .unwrap();
        assert!(o3.format.has_alpha);
        assert_eq!(o3.sample(3, 0, 0), 0);
        assert_eq!(o3.sample(3, 1, 1), 255);
        assert_eq!(o3.sample(3, 2, 1), 128);
        assert_eq!(o3.sample(0, 1, 1), 200);
        assert_eq!(o3.sample(0, 2, 1), 200, "un-premultiplied colour survives");
        // Odd offset in 4:2:0 promotes to 4:4:4.
        let b420 = HeifFrame::filled(4, 4, fmt(Chroma::Yuv420, 8, false), 10).unwrap();
        let s420 = HeifFrame::filled(2, 2, fmt(Chroma::Yuv420, 8, false), 99).unwrap();
        let d4 = OverlayDescriptor {
            canvas_fill: [0, 0, 0, 65535],
            output_width: 4,
            output_height: 4,
            offsets: vec![(0, 0), (1, 0)],
        };
        let o4 = composite_overlay(
            &d4,
            &[
                OverlayInput {
                    frame: &b420,
                    premultiplied: false,
                },
                OverlayInput {
                    frame: &s420,
                    premultiplied: false,
                },
            ],
            None,
        )
        .unwrap();
        assert_eq!(o4.format.chroma, Chroma::Yuv444);
        assert_eq!(o4.sample(0, 1, 0), 99);
        assert_eq!(o4.sample(0, 3, 0), 10);
        // Even offsets keep 4:2:0.
        let d5 = OverlayDescriptor {
            offsets: vec![(0, 0), (2, 2)],
            ..d4.clone()
        };
        let o5 = composite_overlay(
            &d5,
            &[
                OverlayInput {
                    frame: &b420,
                    premultiplied: false,
                },
                OverlayInput {
                    frame: &s420,
                    premultiplied: false,
                },
            ],
            None,
        )
        .unwrap();
        assert_eq!(o5.format.chroma, Chroma::Yuv420);
        assert_eq!(o5.sample(0, 3, 3), 99);
        assert_eq!(o5.sample(1, 1, 1), 99);
        assert_eq!(o5.sample(1, 0, 0), 10);
    }

    #[test]
    fn alpha_attachment_resizes_and_rescales() {
        let master = HeifFrame::filled(4, 4, fmt(Chroma::Yuv420, 8, false), 1).unwrap();
        let alpha = HeifFrame::filled(2, 2, fmt(Chroma::Yuv420, 10, false), 1023).unwrap();
        let out = attach_alpha(&master, &alpha).unwrap();
        assert!(out.format.has_alpha);
        assert_eq!(out.sample(3, 3, 3), 255);
        let small = rescale_depth(
            &HeifFrame::filled(1, 1, fmt(Chroma::Mono, 8, false), 255).unwrap(),
            10,
        )
        .unwrap();
        assert_eq!(small.sample(0, 0, 0), 1020);
    }
}
