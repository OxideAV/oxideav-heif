//! Sample layouts announced by decoder configurations and the
//! decode-free prediction of an image item's output layout / size.
//!
//! Standalone (no codec, no framework): the `hvcC` / `av1C` / `avcC` /
//! `oinf` records say what chroma structure and depth a coded item
//! reconstructs to, and the composition layer's promotion rules say
//! what a derived item's output looks like. [`info`](crate::info)
//! reads the header this way; the framework demuxer announces the
//! still stream's parameters from the same prediction.

use crate::av1c::Av1Config;
use crate::compose::needs_444;
use crate::derived::{ImageKind, ImageNode};
use crate::error::{HeifError, Result};
use crate::hvcc::HevcConfig;
use crate::image::{Chroma, HeifPixelFormat};
use crate::props::{ItemProperties, Property};
use crate::vvcc::VvcConfig;

/// Sample layout an `hvcC` record announces.
pub fn hevc_layout(cfg: &HevcConfig) -> Result<HeifPixelFormat> {
    let chroma = Chroma::from_idc(cfg.chroma_format_idc).ok_or_else(|| {
        HeifError::invalid(format!("hvcC chroma_format_idc {}", cfg.chroma_format_idc))
    })?;
    HeifPixelFormat::new(chroma, cfg.bit_depth_luma(), false)
}

/// Sample layout a `vvcC` record announces: its `chroma_format_idc` /
/// `bit_depth_minus8` head, or the SPS in its arrays when
/// `ptl_present_flag` is 0.
pub fn vvc_layout(cfg: &VvcConfig) -> Result<HeifPixelFormat> {
    let (idc, depth) = cfg.sample_format()?;
    let chroma = Chroma::from_idc(idc)
        .ok_or_else(|| HeifError::invalid(format!("vvcC chroma_format_idc {idc}")))?;
    HeifPixelFormat::new(chroma, depth, false)
}

/// Sample layout an `av1C` record announces.
pub fn av1_layout(cfg: &Av1Config) -> Result<HeifPixelFormat> {
    let chroma = Chroma::from_idc(cfg.chroma_format_idc()).expect("idc in 0..=3");
    HeifPixelFormat::new(chroma, cfg.bit_depth(), false)
}

/// Sample layout of a coded item from its decoder-configuration
/// property (`lhvC` + `oinf` for layered items, else `hvcC` / `av1C` /
/// `avcC`); `None` when the properties carry none.
pub fn layout_of(props: &ItemProperties) -> Option<HeifPixelFormat> {
    if props.lhvc().is_some() {
        return layered_layout(props, props.tols().unwrap_or(0)).ok();
    }
    if let Some(h) = props.hvcc() {
        return hevc_layout(h).ok();
    }
    if let Some(a) = props.av1c() {
        return av1_layout(a).ok();
    }
    if let Some(a) = props.avcc() {
        return a.layout().ok();
    }
    if let Some(v) = props.vvcc() {
        return vvc_layout(v).ok();
    }
    None
}

/// Sample layout of an `lhv1` item's base layer: the `oinf` operating
/// point of output layer set 0 (its `maxChromaFormat` /
/// `maxBitDepthMinus8`), else that of the first operating point.
pub fn layered_base_layout(props: &ItemProperties) -> Result<HeifPixelFormat> {
    layered_layout(props, 0)
}

/// Sample layout of an `lhv1` item's output layer set `tols`: the
/// `oinf` operating point of that set (its `maxChromaFormat` /
/// `maxBitDepthMinus8`), else that of set 0, else the first one.
pub fn layered_layout(props: &ItemProperties, tols: u16) -> Result<HeifPixelFormat> {
    let oinf = props
        .oinf()
        .ok_or_else(|| HeifError::invalid("lhv1 item without an oinf property (HEIF B.2.2.1.3)"))?;
    let op = oinf
        .operating_point(tols)
        .or_else(|| oinf.operating_point(0))
        .or_else(|| oinf.operating_points.first())
        .ok_or_else(|| HeifError::invalid("oinf without operating points"))?;
    let chroma = Chroma::from_idc(op.max_chroma_format)
        .ok_or_else(|| HeifError::invalid("oinf maxChromaFormat"))?;
    HeifPixelFormat::new(chroma, 8 + op.max_bit_depth_minus8, false)
}

/// Predict the output layout and size of an image item without
/// decoding, by replaying the composition layer's promotion rules on
/// the container metadata (the still stream's parameters, [`info`](crate::info)).
pub fn predict_output(node: &ImageNode) -> Result<(HeifPixelFormat, (u32, u32))> {
    let (mut fmt, (mut w, mut h)) = match &node.kind {
        ImageKind::Coded(_) => {
            let f = layout_of(&node.properties).ok_or_else(|| {
                HeifError::invalid(format!(
                    "item {}: no decoder configuration to predict a layout from",
                    node.item.id
                ))
            })?;
            (f, node.reconstructed_size()?)
        }
        ImageKind::Grid(g) => {
            let first = node
                .inputs
                .first()
                .ok_or_else(|| HeifError::invalid("grid without inputs"))?;
            let (tf, (tw, th)) = predict_output(first)?;
            let any_alpha = node
                .inputs
                .iter()
                .any(|i| predict_output(i).map(|(f, _)| f.has_alpha).unwrap_or(false));
            let promote = needs_444(tf.chroma, tw % 2 == 1, th % 2 == 1)
                || needs_444(tf.chroma, g.output_width % 2 == 1, g.output_height % 2 == 1);
            let chroma = if promote { Chroma::Yuv444 } else { tf.chroma };
            (
                HeifPixelFormat::new(chroma, tf.bit_depth, any_alpha || tf.has_alpha)?,
                (g.output_width, g.output_height),
            )
        }
        ImageKind::Overlay(o) => {
            let mut layouts = Vec::with_capacity(node.inputs.len());
            for i in &node.inputs {
                layouts.push(predict_output(i)?);
            }
            let (ff, _) = layouts
                .first()
                .ok_or_else(|| HeifError::invalid("iovl without inputs"))?;
            let any_alpha = ff.chroma != Chroma::Mono && layouts.iter().any(|(f, _)| f.has_alpha);
            let promote = any_alpha
                || needs_444(
                    ff.chroma,
                    o.offsets.iter().any(|(x, _)| x.rem_euclid(2) == 1) || o.output_width % 2 == 1,
                    o.offsets.iter().any(|(_, y)| y.rem_euclid(2) == 1) || o.output_height % 2 == 1,
                )
                || layouts
                    .iter()
                    .any(|(f, (w, h))| needs_444(f.chroma, w % 2 == 1, h % 2 == 1));
            let chroma = if promote { Chroma::Yuv444 } else { ff.chroma };
            let translucent = o.canvas_fill[3] != 65535;
            (
                HeifPixelFormat::new(chroma, ff.bit_depth, translucent)?,
                (o.output_width, o.output_height),
            )
        }
        ImageKind::Identity | ImageKind::ToneMap(_) => {
            let first = node
                .inputs
                .first()
                .ok_or_else(|| HeifError::invalid("derived item without inputs"))?;
            predict_output(first)?
        }
        ImageKind::ColourFormatEnhancement(c) => {
            let mut geometry = Vec::with_capacity(node.inputs.len());
            for i in &node.inputs {
                let (f, size) = predict_output(i)?;
                geometry.push((size, f.bit_depth));
            }
            let (f, size, _) = crate::compose::plan_colour_format_enhancement(
                c,
                &geometry,
                node.properties.nclx(),
            )?;
            (f, size)
        }
        ImageKind::Tiled(t) => {
            // The tiles' layout comes from their tilC-associated decoder
            // configuration; odd tile / output sizes promote as a grid.
            let (w, h) = node.reconstructed_size()?;
            let f = layout_of(&t.tile_properties).ok_or_else(|| {
                HeifError::invalid(format!(
                    "item {}: no decoder configuration among the tile properties",
                    node.item.id
                ))
            })?;
            let (tw, th) = (t.config.tile_width, t.config.tile_height);
            let promote = needs_444(
                f.chroma,
                tw % 2 == 1 || w % 2 == 1,
                th % 2 == 1 || h % 2 == 1,
            );
            (
                HeifPixelFormat {
                    chroma: if promote { Chroma::Yuv444 } else { f.chroma },
                    ..f
                },
                (w, h),
            )
        }
    };
    for e in node.properties.transformative() {
        match &e.property {
            Property::Clap(c) => {
                let r = c.resolve(w, h)?;
                if needs_444(
                    fmt.chroma,
                    r.x % 2 == 1 || r.width % 2 == 1,
                    r.y % 2 == 1 || r.height % 2 == 1,
                ) {
                    fmt.chroma = Chroma::Yuv444;
                }
                w = r.width;
                h = r.height;
            }
            Property::Irot(r) => {
                if r.angle & 3 != 0 {
                    let (sx, sy) = fmt.chroma.shift();
                    let axis_swap = r.angle & 1 == 1 && sx != sy;
                    if axis_swap || needs_444(fmt.chroma, w % 2 == 1, h % 2 == 1) {
                        fmt.chroma = Chroma::Yuv444;
                    }
                    if r.angle & 1 == 1 {
                        std::mem::swap(&mut w, &mut h);
                    }
                }
            }
            Property::Iscl(s) => {
                let (nw, nh) = s.output_size(w, h)?;
                w = nw;
                h = nh;
            }
            _ => {}
        }
    }
    if node.alpha.is_some() {
        fmt.has_alpha = true;
    }
    Ok((fmt, (w, h)))
}

/// Sample layout a visual sample entry's decoder configuration
/// announces (`hvcC` / `av1C` / `avcC`); `None` without one.
pub fn entry_layout(entry: &crate::sequence::SampleEntry) -> Option<HeifPixelFormat> {
    if let Some(h) = &entry.hvcc {
        return hevc_layout(h).ok();
    }
    if let Some(a) = &entry.av1c {
        return av1_layout(a).ok();
    }
    if let Some(a) = &entry.avcc {
        return a.layout().ok();
    }
    if let Some(v) = &entry.vvcc {
        return vvc_layout(v).ok();
    }
    None
}
