//! Coded image items → pixels through the oxideav codec crates
//! (`registry` feature).
//!
//! An `hvc1` / `hev1` item is one HEVC access unit of length-prefixed
//! NAL units (HEIF Annex B.2.2) whose parameter sets live in the `hvcC`
//! property; `oxideav-h265`'s registry decoder takes exactly that shape
//! (the `hvcC` record as `extradata`, length-prefixed packets). An
//! `av01` item is one AV1 temporal unit (AVIF §2.1) with the `av1C`
//! record as `extradata`; `oxideav-av1`'s registry decoder consumes it
//! verbatim.
//!
//! Decoding goes through the direct codec factories by default, or
//! through a caller-supplied [`CodecRegistry`] so alternative
//! implementations registered under the same ids are honoured.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, Decoder, Error as CoreError, Frame, Packet, TimeBase,
};

use crate::av1c::Av1Config;
use crate::derived::{ImageKind, ImageNode};
use crate::error::{HeifError, Result};
use crate::file::HeifFile;
use crate::hvcc::HevcConfig;
use crate::image::{Chroma, HeifFrame, HeifPixelFormat, HeifPlane};
use crate::meta::{ITEM_TYPE_AV01, ITEM_TYPE_HEV1, ITEM_TYPE_HVC1};
use crate::props::ItemProperties;

/// Codec id the HEVC decoder is registered under.
pub const CODEC_ID_HEVC: &str = "h265";
/// Codec id the AV1 decoder is registered under.
pub const CODEC_ID_AV1: &str = "av1";

/// Upper bound on the number of coded items decoded for one image.
pub const MAX_ITEM_DECODES: usize = 4096;

/// Decodes coded image items.
#[derive(Clone, Copy, Default)]
pub struct ItemDecoder<'r> {
    registry: Option<&'r CodecRegistry>,
}

impl<'r> ItemDecoder<'r> {
    /// Decode through the direct `oxideav-h265` / `oxideav-av1` factories.
    pub fn direct() -> Self {
        Self { registry: None }
    }

    /// Decode through `registry` (`"h265"` / `"av1"` ids).
    pub fn with_registry(registry: &'r CodecRegistry) -> Self {
        Self {
            registry: Some(registry),
        }
    }

    /// The codec parameters a coded item needs, as a container demuxer
    /// would fill them: id, `extradata`, `ispe` geometry, pixel format.
    pub fn codec_parameters(node: &ImageNode) -> Result<CodecParameters> {
        let ItemKind::Coded(kind) = classify(node)? else {
            return Err(HeifError::invalid(format!(
                "item {} is not a coded image",
                node.item.id
            )));
        };
        let (id, extradata, layout) = match kind {
            CodedKind::Hevc(cfg) => (CODEC_ID_HEVC, cfg.raw.clone(), hevc_layout(cfg)?),
            CodedKind::Av1(cfg) => (CODEC_ID_AV1, cfg.raw.clone(), av1_layout(cfg)?),
        };
        let mut params = CodecParameters::video(CodecId::new(id));
        params.extradata = extradata;
        if let Some((w, h)) = node.ispe() {
            params.width = Some(w);
            params.height = Some(h);
        }
        params.pixel_format = layout.to_core();
        Ok(params)
    }

    fn make(&self, params: &CodecParameters) -> Result<Box<dyn Decoder>> {
        let r = match self.registry {
            Some(reg) => reg.first_decoder(params),
            None => match params.codec_id.as_str() {
                CODEC_ID_HEVC => oxideav_h265::make_decoder(params),
                CODEC_ID_AV1 => oxideav_av1::registry::make_decoder(params),
                other => Err(CoreError::codec_not_found(other)),
            },
        };
        r.map_err(|e| HeifError::unsupported(format!("{}: {e}", params.codec_id)))
    }

    /// Decode one coded item to its reconstructed picture (no
    /// transformative properties applied).
    pub fn decode_coded(&self, file: &HeifFile, node: &ImageNode) -> Result<HeifFrame> {
        let ItemKind::Coded(kind) = classify(node)? else {
            return Err(HeifError::invalid(format!(
                "item {} is not a coded image",
                node.item.id
            )));
        };
        let layout = match &kind {
            CodedKind::Hevc(c) => hevc_layout(c)?,
            CodedKind::Av1(c) => av1_layout(c)?,
        };
        let params = Self::codec_parameters(node)?;
        let data = file.item_data_owned(node.item.id)?;
        if data.is_empty() {
            return Err(HeifError::invalid(format!(
                "item {}: empty payload",
                node.item.id
            )));
        }
        let mut dec = self.make(&params)?;
        let pkt = Packet::new(0, TimeBase::new(1, 1), data)
            .with_pts(0)
            .with_keyframe(true);
        dec.send_packet(&pkt).map_err(|e| {
            HeifError::invalid(format!(
                "item {}: decoder rejected the payload: {e}",
                node.item.id
            ))
        })?;
        dec.flush()
            .map_err(|e| HeifError::invalid(format!("item {}: flush: {e}", node.item.id)))?;
        let mut first: Option<oxideav_core::VideoFrame> = None;
        loop {
            match dec.receive_frame() {
                Ok(Frame::Video(v)) => {
                    if first.is_none() {
                        first = Some(v);
                    }
                }
                Ok(_) => {}
                Err(CoreError::NeedMore) | Err(CoreError::Eof) => break,
                Err(e) => {
                    return Err(HeifError::invalid(format!(
                        "item {}: decode failed: {e}",
                        node.item.id
                    )))
                }
            }
        }
        let vf = first.ok_or_else(|| {
            HeifError::invalid(format!(
                "item {}: decoder produced no picture",
                node.item.id
            ))
        })?;
        frame_from_planes(&vf, layout, node.ispe(), node.item.id)
    }
}

/// The two coded item kinds this crate decodes.
#[derive(Clone, Copy, Debug)]
pub enum CodedKind<'a> {
    /// `hvc1` / `hev1` with its `hvcC`.
    Hevc(&'a HevcConfig),
    /// `av01` with its `av1C`.
    Av1(&'a Av1Config),
}

/// Classification of a graph node for decoding.
#[derive(Clone, Copy, Debug)]
pub enum ItemKind<'a> {
    /// A coded item this crate can hand to a codec.
    Coded(CodedKind<'a>),
    /// A derived item (composed by the `compose` module).
    Derived,
}

/// Classify a node: coded (with its decoder configuration) or derived.
pub fn classify(node: &ImageNode) -> Result<ItemKind<'_>> {
    match &node.kind {
        ImageKind::Coded(t) => match *t {
            ITEM_TYPE_HVC1 | ITEM_TYPE_HEV1 => {
                let cfg = node.properties.hvcc().ok_or_else(|| {
                    HeifError::invalid(format!(
                        "item {}: hvc1 item without an hvcC property",
                        node.item.id
                    ))
                })?;
                Ok(ItemKind::Coded(CodedKind::Hevc(cfg)))
            }
            ITEM_TYPE_AV01 => {
                let cfg = node.properties.av1c().ok_or_else(|| {
                    HeifError::invalid(format!(
                        "item {}: av01 item without an av1C property",
                        node.item.id
                    ))
                })?;
                Ok(ItemKind::Coded(CodedKind::Av1(cfg)))
            }
            other => Err(HeifError::unsupported(format!(
                "item {}: coded image type '{}' has no decoder in this crate",
                node.item.id,
                crate::boxes::fourcc_str(&other)
            ))),
        },
        _ => Ok(ItemKind::Derived),
    }
}

/// Sample layout an `hvcC` record announces.
pub fn hevc_layout(cfg: &HevcConfig) -> Result<HeifPixelFormat> {
    let chroma = Chroma::from_idc(cfg.chroma_format_idc).ok_or_else(|| {
        HeifError::invalid(format!("hvcC chroma_format_idc {}", cfg.chroma_format_idc))
    })?;
    HeifPixelFormat::new(chroma, cfg.bit_depth_luma(), false)
}

/// Sample layout an `av1C` record announces.
pub fn av1_layout(cfg: &Av1Config) -> Result<HeifPixelFormat> {
    let chroma = Chroma::from_idc(cfg.chroma_format_idc()).expect("idc in 0..=3");
    HeifPixelFormat::new(chroma, cfg.bit_depth(), false)
}

/// Same-layout properties helper used by callers that only hold a
/// property list (no graph node).
pub fn layout_of(props: &ItemProperties) -> Option<HeifPixelFormat> {
    if let Some(h) = props.hvcc() {
        return hevc_layout(h).ok();
    }
    if let Some(a) = props.av1c() {
        return av1_layout(a).ok();
    }
    None
}

/// Interpret the planes a codec emitted as a tightly packed
/// [`HeifFrame`] of the announced layout, cropped to `ispe` when the
/// decoded picture is larger (codecs may emit the coded size).
fn frame_from_planes(
    vf: &oxideav_core::VideoFrame,
    layout: HeifPixelFormat,
    ispe: Option<(u32, u32)>,
    item_id: u32,
) -> Result<HeifFrame> {
    let planes = vf.image_planes();
    let bps = layout.bytes_per_sample();
    let need = layout.plane_count();
    if planes.len() < need {
        return Err(HeifError::invalid(format!(
            "item {item_id}: decoder emitted {} planes, layout {:?} needs {need}",
            planes.len(),
            layout
        )));
    }
    let y = &planes[0];
    if y.stride == 0 || y.stride % bps != 0 || y.data.is_empty() {
        return Err(HeifError::invalid(format!(
            "item {item_id}: luma plane stride {} incompatible with {}-byte samples",
            y.stride, bps
        )));
    }
    let dec_w = (y.stride / bps) as u32;
    let dec_h = (y.data.len() / y.stride) as u32;
    let (w, h) = match ispe {
        Some((iw, ih)) => {
            if iw > dec_w || ih > dec_h {
                return Err(HeifError::invalid(format!(
                    "item {item_id}: ispe {iw}x{ih} larger than the decoded {dec_w}x{dec_h} picture"
                )));
            }
            (iw, ih)
        }
        None => (dec_w, dec_h),
    };
    let mut out = Vec::with_capacity(need);
    for (p, src) in planes.iter().take(need).enumerate() {
        let (pw, ph) = layout.plane_dims(p, w, h);
        let row_bytes = pw as usize * bps;
        if src.stride < row_bytes || src.data.len() < src.stride * (ph as usize - 1) + row_bytes {
            return Err(HeifError::invalid(format!(
                "item {item_id}: plane {p} too small for {pw}x{ph} ({} bytes, stride {})",
                src.data.len(),
                src.stride
            )));
        }
        let mut data = Vec::with_capacity(row_bytes * ph as usize);
        for r in 0..ph as usize {
            data.extend_from_slice(&src.data[r * src.stride..r * src.stride + row_bytes]);
        }
        out.push(HeifPlane {
            stride: row_bytes,
            data,
        });
    }
    let f = HeifFrame {
        width: w,
        height: h,
        format: layout,
        planes: out,
    };
    f.validate()?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{VideoFrame, VideoPlane};

    #[test]
    fn frame_from_planes_crops_to_ispe() {
        let layout = HeifPixelFormat::new(Chroma::Yuv420, 8, false).unwrap();
        let vf = VideoFrame {
            pts: None,
            planes: vec![
                VideoPlane {
                    stride: 8,
                    data: (0..64).collect(),
                },
                VideoPlane {
                    stride: 4,
                    data: vec![1; 16],
                },
                VideoPlane {
                    stride: 4,
                    data: vec![2; 16],
                },
            ],
        };
        let f = frame_from_planes(&vf, layout, Some((6, 4)), 1).unwrap();
        assert_eq!((f.width, f.height), (6, 4));
        assert_eq!(f.planes[0].stride, 6);
        assert_eq!(f.sample(0, 5, 3), 3 * 8 + 5);
        assert_eq!(f.plane_dims(1), (3, 2));
        assert!(frame_from_planes(&vf, layout, Some((9, 4)), 1).is_err());
        let g = frame_from_planes(&vf, layout, None, 1).unwrap();
        assert_eq!((g.width, g.height), (8, 8));
        let mono = HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap();
        let m = frame_from_planes(&vf, mono, Some((8, 8)), 1).unwrap();
        assert_eq!(m.planes.len(), 1);
    }
}
