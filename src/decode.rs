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

use std::collections::HashMap;

use crate::av1c::Av1Config;
use crate::compose::{
    apply_transforms, attach_alpha, composite_grid, composite_overlay, OverlayInput,
};
use crate::derived::{build_graph, ImageKind, ImageNode};
use crate::error::{HeifError, Result};
use crate::file::HeifFile;
use crate::hvcc::HevcConfig;
use crate::image::{Chroma, HeifFrame, HeifPixelFormat, HeifPlane};
use crate::meta::{ITEM_TYPE_AV01, ITEM_TYPE_HEV1, ITEM_TYPE_HVC1};
use crate::props::{Colr, ItemProperties};

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

#[doc(hidden)]
/// The two coded item kinds this crate decodes.
#[derive(Clone, Copy, Debug)]
pub enum CodedKind<'a> {
    /// `hvc1` / `hev1` with its `hvcC`.
    Hevc(&'a HevcConfig),
    /// `av01` with its `av1C`.
    Av1(&'a Av1Config),
}

#[doc(hidden)]
/// Classification of a graph node for decoding.
#[derive(Clone, Copy, Debug)]
pub enum ItemKind<'a> {
    /// A coded item this crate can hand to a codec.
    Coded(CodedKind<'a>),
    /// A derived item (composed by the `compose` module).
    Derived,
}

#[doc(hidden)]
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

#[doc(hidden)]
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

/// A fully reconstructed image item: the output image (§6.3) with its
/// alpha auxiliary attached, plus the descriptive metadata a renderer
/// needs.
#[derive(Clone, Debug)]
pub struct DecodedImage {
    /// The item that was decoded.
    pub item_id: u32,
    /// The output image; carries an alpha plane when the item (or the
    /// top of its derivation) has an alpha auxiliary.
    pub frame: HeifFrame,
    /// `prem`: colour samples are pre-multiplied by the alpha plane.
    pub premultiplied_alpha: bool,
    /// The depth-map auxiliary (its own output image), when present.
    pub depth: Option<HeifFrame>,
    /// Effective CICP colour information: the item's `nclx`, else the
    /// MIAF §7.3.6.4 default.
    pub nclx: Colr,
    /// `true` when `nclx` came from the file rather than the default.
    pub nclx_explicit: bool,
    /// ICC profile (`rICC` / `prof` colr), when present.
    pub icc_profile: Option<Vec<u8>>,
    /// Exif payload with the HEIF offset word resolved (Annex A.2.1):
    /// the bytes from the TIFF header on.
    pub exif: Option<Vec<u8>>,
    /// XMP packet (Annex A.3), when present.
    pub xmp: Option<String>,
    /// Item ids of the thumbnails (`thmb`) of this image.
    pub thumbnail_ids: Vec<u32>,
    /// The item's typed properties (for `pixi`, `clli`, `mdcv`, …).
    pub properties: ItemProperties,
    /// The ISO 21496-1 gain map attached through a `tmap` item whose
    /// first `dimg` input is this image (or this image itself when it
    /// is the `tmap`), decoded but not applied: [`DecodedImage::frame`]
    /// stays the baseline rendition. Apply with
    /// [`DecodedImage::apply_gain_map`].
    pub gain_map: Option<GainMapAttachment>,
}

/// A decoded gain map and its metadata (see [`DecodedImage::gain_map`]).
#[derive(Clone, Debug)]
pub struct GainMapAttachment {
    /// The `tmap` item.
    pub tmap_item_id: u32,
    /// The gain-map image item (second `dimg` input of the `tmap`).
    pub gain_map_item_id: u32,
    /// The parsed `tmap` payload.
    pub metadata: crate::gainmap::GainMapMetadata,
    /// The gain map's output image.
    pub frame: HeifFrame,
    /// The gain-map item's own `nclx` (its YCbCr matrix), when any.
    pub colr: Option<Colr>,
    /// The alternate image's colour information (the `tmap` item's
    /// `nclx`), when any.
    pub alternate_colr: Option<Colr>,
}

impl DecodedImage {
    /// Apply the attached gain map for a target HDR headroom (log₂ of
    /// the display's HDR / SDR white ratio; the base headroom leaves
    /// the base untouched, the alternate headroom applies the map
    /// fully). Linear RGB in the gain-map application space.
    pub fn apply_gain_map(&self, h_target: f64) -> Result<crate::gainmap::LinearRgbImage> {
        let gm = self
            .gain_map
            .as_ref()
            .ok_or_else(|| HeifError::invalid("image carries no gain map"))?;
        crate::gainmap::apply_gain_map(
            &self.frame,
            Some(&self.nclx),
            &gm.frame,
            gm.colr.as_ref(),
            gm.alternate_colr.as_ref(),
            &gm.metadata,
            h_target,
        )
    }
}

impl DecodedImage {
    /// Output width in pixels.
    pub fn width(&self) -> u32 {
        self.frame.width
    }

    /// Output height in pixels.
    pub fn height(&self) -> u32 {
        self.frame.height
    }
}

/// Decodes an item and everything it derives from, caching coded
/// reconstructions so shared inputs decode once.
struct Session<'a, 'r> {
    file: &'a HeifFile,
    decoder: ItemDecoder<'r>,
    cache: HashMap<u32, HeifFrame>,
    decodes: usize,
}

impl Session<'_, '_> {
    /// Reconstructed image of a node (§6.3, first bullet): decoded
    /// picture or derivation result, *before* the node's own
    /// transformative properties.
    fn reconstruct(&mut self, node: &ImageNode) -> Result<HeifFrame> {
        match &node.kind {
            ImageKind::Coded(_) => {
                if let Some(f) = self.cache.get(&node.item.id) {
                    return Ok(f.clone());
                }
                if self.decodes >= MAX_ITEM_DECODES {
                    return Err(HeifError::exhausted(format!(
                        "more than {MAX_ITEM_DECODES} coded items in one image"
                    )));
                }
                self.decodes += 1;
                let f = self.decoder.decode_coded(self.file, node)?;
                self.cache.insert(node.item.id, f.clone());
                Ok(f)
            }
            ImageKind::Grid(g) => {
                let mut tiles = Vec::with_capacity(node.inputs.len());
                for t in &node.inputs {
                    tiles.push(self.output(t)?);
                }
                composite_grid(g, &tiles)
            }
            ImageKind::Overlay(o) => {
                let mut frames = Vec::with_capacity(node.inputs.len());
                for i in &node.inputs {
                    frames.push((self.output(i)?, i.premultiplied_alpha && i.alpha.is_some()));
                }
                let inputs: Vec<OverlayInput<'_>> = frames
                    .iter()
                    .map(|(f, prem)| OverlayInput {
                        frame: f,
                        premultiplied: *prem,
                    })
                    .collect();
                composite_overlay(o, &inputs, node.properties.nclx())
            }
            ImageKind::Identity => {
                let input = node.inputs.first().ok_or_else(|| {
                    HeifError::invalid(format!("iden item {} has no input", node.item.id))
                })?;
                self.output(input)
            }
            ImageKind::ToneMap(_) => {
                // ISO/IEC 21496-1 gain-map application is out of scope;
                // the base image (first input) is the SDR rendition.
                let input = node.inputs.first().ok_or_else(|| {
                    HeifError::invalid(format!("tmap item {} has no input", node.item.id))
                })?;
                self.output(input)
            }
        }
    }

    /// Output image of a node (§6.3, second bullet): reconstruction
    /// with the transformative chain applied, then the alpha
    /// auxiliary attached (its own output image, §6.9.1).
    fn output(&mut self, node: &ImageNode) -> Result<HeifFrame> {
        let unsupported = node.properties.unsupported_essential();
        if !unsupported.is_empty() {
            return Err(HeifError::unsupported(format!(
                "item {}: essential properties not recognised: {}",
                node.item.id,
                unsupported
                    .iter()
                    .map(crate::boxes::fourcc_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let rec = self.reconstruct(node)?;
        let mut out = apply_transforms(&rec, node.properties.transformative(), false)?;
        if let Some(a) = &node.alpha {
            let alpha = self.output(a)?;
            out = attach_alpha(&out, &alpha)?;
        }
        Ok(out)
    }
}

/// Decode `item_id` (coded or derived) to its output image with alpha,
/// depth, colour information and metadata resolved.
pub fn decode_item(
    file: &HeifFile,
    item_id: u32,
    decoder: ItemDecoder<'_>,
) -> Result<DecodedImage> {
    let node = build_graph(file, item_id)?;
    let mut session = Session {
        file,
        decoder,
        cache: HashMap::new(),
        decodes: 0,
    };
    let frame = session.output(&node)?;
    let depth = match &node.depth {
        Some(d) => Some(session.output(d)?),
        None => None,
    };
    let (nclx, nclx_explicit) = match node.properties.nclx() {
        Some(c) => (c.clone(), true),
        None => (Colr::MIAF_DEFAULT, false),
    };
    let icc_profile = node.properties.icc_profile().map(<[u8]>::to_vec);
    let gain_map = find_gain_map(file, &node, &mut session)?;
    let mut exif = None;
    let mut xmp = None;
    for m in &node.metadata {
        if m.item_type == crate::meta::ITEM_TYPE_EXIF && exif.is_none() {
            exif = Some(exif_payload(&file.item_data(m.id)?)?);
        } else if m.is_xmp() && xmp.is_none() {
            xmp = Some(String::from_utf8_lossy(&file.item_data(m.id)?).into_owned());
        }
    }
    Ok(DecodedImage {
        item_id,
        premultiplied_alpha: node.premultiplied_alpha && node.alpha.is_some(),
        frame,
        depth,
        nclx,
        nclx_explicit,
        icc_profile,
        exif,
        xmp,
        thumbnail_ids: node.thumbnails.iter().map(|t| t.item.id).collect(),
        properties: node.properties.clone(),
        gain_map,
    })
}

/// Locate and decode the gain map of `node`: the node itself when it
/// is a `tmap`, else a `tmap` item whose first `dimg` input is the
/// node. The `tmap` body is the ISO 21496-1 C.2 metadata; its second
/// input is the gain-map image. An unparsable / unsupported payload
/// yields `None` (C.2.3: fall back to the base image).
fn find_gain_map(
    file: &HeifFile,
    node: &ImageNode,
    session: &mut Session<'_, '_>,
) -> Result<Option<GainMapAttachment>> {
    let meta = file.meta()?;
    let (tmap_id, body, inputs): (u32, Vec<u8>, Vec<u32>) = match &node.kind {
        ImageKind::ToneMap(body) => (
            node.item.id,
            body.clone(),
            meta.derivation_inputs(node.item.id),
        ),
        _ => {
            let Some(t) = meta.items.iter().find(|it| {
                it.item_type == crate::meta::ITEM_TYPE_TMAP
                    && meta.derivation_inputs(it.id).first() == Some(&node.item.id)
            }) else {
                return Ok(None);
            };
            (
                t.id,
                file.item_data_owned(t.id)?,
                meta.derivation_inputs(t.id),
            )
        }
    };
    if inputs.len() != 2 {
        return Ok(None);
    }
    let Ok(metadata) = crate::gainmap::GainMapMetadata::parse_tmap_body(&body) else {
        return Ok(None);
    };
    let gain_node = build_graph(file, inputs[1])?;
    let frame = session.output(&gain_node)?;
    let tmap_props = ItemProperties::resolve(meta, tmap_id)?;
    Ok(Some(GainMapAttachment {
        tmap_item_id: tmap_id,
        gain_map_item_id: inputs[1],
        metadata,
        frame,
        colr: gain_node.properties.nclx().cloned(),
        alternate_colr: tmap_props.nclx().cloned(),
    }))
}

/// Decode the primary item (`pitm`).
pub fn decode_primary(file: &HeifFile, decoder: ItemDecoder<'_>) -> Result<DecodedImage> {
    let id = file.primary_item()?.id;
    decode_item(file, id, decoder)
}

/// Strip the `exif_tiff_header_offset` word of an `Exif` item body
/// (HEIF Annex A.2.1) and return the bytes from the TIFF header on.
pub fn exif_payload(item: &[u8]) -> Result<Vec<u8>> {
    if item.len() < 4 {
        return Err(HeifError::invalid("Exif item shorter than its offset word"));
    }
    let off = u32::from_be_bytes([item[0], item[1], item[2], item[3]]) as usize;
    let start = 4usize
        .checked_add(off)
        .filter(|s| *s <= item.len())
        .ok_or_else(|| {
            HeifError::invalid(format!("Exif tiff header offset {off} past the item"))
        })?;
    Ok(item[start..].to_vec())
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
