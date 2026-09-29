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
    CodecId, CodecParameters, CodecRegistry, Decoder, Error as CoreError, ExecutionContext, Frame,
    Packet, TimeBase,
};

use std::collections::HashMap;

use crate::av1c::Av1Config;
use crate::compose::{
    apply_transforms_owned, attach_alpha, composite_overlay, GridCanvas, OverlayInput,
};
use crate::derived::{build_graph, ImageKind, ImageNode};
use crate::error::{HeifError, Result};
use crate::file::HeifFile;
use crate::hvcc::HevcConfig;
use crate::image::{Chroma, HeifFrame, HeifPixelFormat, HeifPlane};
use crate::meta::{ITEM_TYPE_AV01, ITEM_TYPE_AVC1, ITEM_TYPE_HEV1, ITEM_TYPE_HVC1, ITEM_TYPE_LHV1};
use crate::props::{Colr, ItemProperties};

/// Codec id the HEVC decoder is registered under.
pub const CODEC_ID_HEVC: &str = "h265";
/// Codec id the AV1 decoder is registered under.
pub const CODEC_ID_AV1: &str = "av1";
/// Codec id the AVC decoder is registered under.
pub const CODEC_ID_AVC: &str = "h264";

/// Upper bound on the number of coded items decoded for one image.
pub const MAX_ITEM_DECODES: usize = 4096;

/// What a `tmap` (tone-map) derived image item decodes to when it is
/// the item being decoded (HEIF Amd 1 §6.6.2.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToneMapOutput {
    /// The base input image (the SDR rendition an `altr`-aware reader
    /// without tone-map support displays); the decoded gain map rides
    /// along in [`DecodedImage::gain_map`] for on-demand application.
    /// The default, so SDR pipelines get an SDR picture.
    #[default]
    Base,
    /// The normative reconstruction (§6.6.2.4.1): the gain map fully
    /// applied, in the `tmap` item's own `colr`, at the depth its
    /// `pixi` hints (else the base's). A `tmap` that is the *input* of
    /// another derived item is always reconstructed this way.
    Applied,
}

/// Decodes coded image items.
#[derive(Clone, Copy, Default)]
pub struct ItemDecoder<'r> {
    registry: Option<&'r CodecRegistry>,
    tone_map: ToneMapOutput,
    reference_white_nits: Option<f64>,
    base_layer_fallback: bool,
    threads: usize,
}

impl<'r> ItemDecoder<'r> {
    /// Decode through the direct `oxideav-h265` / `oxideav-av1` factories.
    pub fn direct() -> Self {
        Self {
            registry: None,
            tone_map: ToneMapOutput::Base,
            reference_white_nits: None,
            base_layer_fallback: false,
            threads: 1,
        }
    }

    /// Decode through `registry` (`"h265"` / `"av1"` ids).
    pub fn with_registry(registry: &'r CodecRegistry) -> Self {
        Self {
            registry: Some(registry),
            tone_map: ToneMapOutput::Base,
            reference_white_nits: None,
            base_layer_fallback: false,
            threads: 1,
        }
    }

    /// Grant a thread budget (oxideav-core's threading contract: serial
    /// until told otherwise). The independent coded items of a `grid`
    /// decode on up to [`ExecutionContext::effective_workers`] workers
    /// at once, each codec instance serial. The output is byte-identical
    /// to the serial decode for every budget.
    pub fn with_execution_context(mut self, ctx: &ExecutionContext) -> Self {
        self.threads = ctx.threads.max(1);
        self
    }

    /// The thread budget ([`ItemDecoder::with_execution_context`]).
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Select what a decoded `tmap` item yields (see [`ToneMapOutput`]).
    pub fn with_tone_map(mut self, output: ToneMapOutput) -> Self {
        self.tone_map = output;
        self
    }

    /// The normative tone-mapped reconstruction for `tmap` items.
    pub fn tone_mapped(self) -> Self {
        self.with_tone_map(ToneMapOutput::Applied)
    }

    /// The selected [`ToneMapOutput`].
    pub fn tone_map_output(&self) -> ToneMapOutput {
        self.tone_map
    }

    /// Decode the base layer of an `lhv1` item whose `tols` asks for
    /// an output layer set with enhancement layers, instead of the
    /// refusal [`HeifError::layered_hevc`] (recognisable through
    /// [`HeifError::layered_hevc_info`]).
    pub fn base_layer_fallback(mut self) -> Self {
        self.base_layer_fallback = true;
        self
    }

    /// The HDR reference white (cd/m²) a PQ-coded tone-mapped
    /// reconstruction is anchored to (default
    /// [`crate::gainmap::DEFAULT_HDR_REFERENCE_WHITE_NITS`]).
    pub fn with_reference_white(mut self, nits: f64) -> Self {
        self.reference_white_nits = Some(nits);
        self
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
        let mut options = oxideav_core::CodecOptions::new();
        let (id, extradata, layout) = match kind {
            CodedKind::Hevc(cfg) => (CODEC_ID_HEVC, cfg.raw.clone(), hevc_layout(cfg)?),
            CodedKind::Av1(cfg) => (CODEC_ID_AV1, cfg.raw.clone(), av1_layout(cfg)?),
            CodedKind::Avc(cfg) => (CODEC_ID_AVC, cfg.raw.clone(), cfg.layout()?),
            CodedKind::LayeredHevc(cfg) => {
                let plan = LayeredPlan::of(node, cfg)?;
                match &plan.extradata {
                    // hvcC ++ lhvC: the multi-layer decoder's record pair
                    // (HEIF B.2.3.2 / oxideav-h265 `layer` / `ols`).
                    Some(x) => {
                        options = match plan.layer {
                            Some(l) => options.set("layer", l.to_string()),
                            None => options.set("ols", plan.tols.to_string()),
                        };
                        (CODEC_ID_HEVC, x.clone(), plan.layout)
                    }
                    // No base record: the base layer travels Annex B
                    // (parameter sets in band), so the decoder gets none.
                    None => (CODEC_ID_HEVC, Vec::new(), plan.layout),
                }
            }
        };
        let mut params = CodecParameters::video(CodecId::new(id));
        params.extradata = extradata;
        params.options = options;
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
                CODEC_ID_AVC => oxideav_h264::h264_decoder::make_decoder(params),
                other => Err(CoreError::codec_not_found(other)),
            },
        };
        r.map_err(|e| HeifError::unsupported(format!("{}: {e}", params.codec_id)))
    }

    /// Decode one coded item to its reconstructed picture (no
    /// transformative properties applied).
    pub fn decode_coded<D: AsRef<[u8]>>(
        &self,
        file: &HeifFile<D>,
        node: &ImageNode,
    ) -> Result<HeifFrame> {
        // The codec instance stays serial: handing a single still's
        // codec the budget measured slower (r463, 12 MP HEVC 0.30 s →
        // 0.34 s), so the budget is spent on independent items.
        let job = self.prepare(file, node)?;
        let dec = self.make(&job.params)?;
        job.run(dec)
    }

    /// Everything a coded item's decode needs, gathered from the file
    /// (the codec instance is made separately, so jobs can run on
    /// worker threads without touching the file).
    fn prepare<D: AsRef<[u8]>>(&self, file: &HeifFile<D>, node: &ImageNode) -> Result<CodedJob> {
        self.prepare_with(node, |id| file.item_data(id), node.ispe())
    }

    /// [`ItemDecoder::prepare`] over a caller-supplied payload source
    /// and output geometry (a `tili` tile: the tile bytes from the offset
    /// table, the `tilC` tile size).
    fn prepare_with<'d>(
        &self,
        node: &ImageNode,
        item_data: impl Fn(u32) -> Result<std::borrow::Cow<'d, [u8]>>,
        ispe: Option<(u32, u32)>,
    ) -> Result<CodedJob> {
        let ItemKind::Coded(kind) = classify(node)? else {
            return Err(HeifError::invalid(format!(
                "item {} is not a coded image",
                node.item.id
            )));
        };
        let mut params = Self::codec_parameters(node)?;
        let mut layers: Option<Vec<u8>> = None;
        let owned = |id: u32| item_data(id).map(std::borrow::Cow::into_owned);
        let (layout, data) = match &kind {
            CodedKind::Hevc(c) => (hevc_layout(c)?, owned(node.item.id)?),
            CodedKind::Av1(c) => (av1_layout(c)?, owned(node.item.id)?),
            CodedKind::Avc(c) => (c.layout()?, owned(node.item.id)?),
            CodedKind::LayeredHevc(cfg) => {
                let plan = LayeredPlan::of(node, cfg)?;
                match &plan.extradata {
                    Some(_) => {
                        // HEIF B.2.2.1.3 + §6.5.11: the tols output layer
                        // set decodes in full; an lsel picks one
                        // reconstructed image, otherwise every output
                        // layer comes back (base first) and the item's
                        // image is the first.
                        if self.base_layer_fallback && plan.layer.is_none() {
                            params.options = params.options.set("layer", "0");
                        } else if plan.layer.is_none() {
                            layers = Some(plan.output_layers.clone());
                        }
                        (plan.layout, owned(node.item.id)?)
                    }
                    None => {
                        // Without a base record only the base layer can be
                        // decoded (Annex B, filtered to nuh_layer_id 0):
                        // an output layer set with enhancement layers is a
                        // typed refusal unless the caller settles for it.
                        let enhancement = plan.output_layers.iter().any(|l| *l != 0);
                        if enhancement && !self.base_layer_fallback {
                            return Err(HeifError::layered_hevc(node.item.id, plan.tols));
                        }
                        (
                            plan.layout,
                            base_layer_annex_b(cfg, &item_data(node.item.id)?)?,
                        )
                    }
                }
            }
        };
        if data.is_empty() {
            return Err(HeifError::invalid(format!(
                "item {}: empty payload",
                node.item.id
            )));
        }
        Ok(CodedJob {
            item_id: node.item.id,
            params,
            data,
            layout,
            ispe,
            layers,
        })
    }

    /// Decode a coded item to every reconstructed image it yields: one
    /// for single-layer items and for layered items with an `lsel`,
    /// one per output layer of the `tols` output layer set (increasing
    /// `nuh_layer_id`, the base first) for a layered item without one
    /// (HEIF §6.5.11 requires an `lsel` in that case; the frames are
    /// offered anyway, tagged by layer).
    pub fn decode_coded_layers<D: AsRef<[u8]>>(
        &self,
        file: &HeifFile<D>,
        node: &ImageNode,
    ) -> Result<Vec<LayerFrame>> {
        let job = self.prepare(file, node)?;
        let dec = self.make(&job.params)?;
        job.run_all(dec)
    }
}

/// One reconstructed image of a coded item with the layer it belongs
/// to (single-layer items: layer 0).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerFrame {
    /// `nuh_layer_id` of the output layer.
    pub layer_id: u8,
    /// The reconstructed (or, on [`DecodedImage::layers`], output) image.
    pub frame: HeifFrame,
}

/// How an `lhv1` item decodes (HEIF B.2.2.1.3 / §6.5.11 / §6.5.29).
struct LayeredPlan {
    /// `hvcC` ++ `lhvC` when the item carries a base record.
    extradata: Option<Vec<u8>>,
    /// The `lsel` layer, when any.
    layer: Option<u16>,
    /// `target_ols_idx`.
    tols: u16,
    /// Output layers of the target output layer set (increasing
    /// `nuh_layer_id`), `[0]` when the `oinf` does not list the set.
    output_layers: Vec<u8>,
    /// Sample layout of the operating point.
    layout: HeifPixelFormat,
}

impl LayeredPlan {
    fn of(node: &ImageNode, cfg: &crate::lhvc::LhevcConfig) -> Result<Self> {
        let props = &node.properties;
        let tols = props.tols().unwrap_or(0);
        let oinf = props.oinf().ok_or_else(|| {
            HeifError::invalid(format!(
                "item {}: lhv1 item without an oinf property (HEIF B.2.2.1.3)",
                node.item.id
            ))
        })?;
        let mut output_layers = oinf.output_layers(tols);
        if output_layers.is_empty() {
            output_layers.push(0);
        }
        output_layers.sort_unstable();
        output_layers.dedup();
        let layer = props.lsel().map(|l| l.layer_id);
        if let Some(l) = layer {
            if !output_layers.iter().any(|o| *o as u16 == l) {
                return Err(HeifError::invalid(format!(
                    "item {}: lsel layer {l} is not an output layer of output layer set {tols} ({output_layers:?})",
                    node.item.id
                )));
            }
        }
        let extradata = props.hvcc().map(|h| {
            let mut x = h.raw.clone();
            x.extend_from_slice(&cfg.raw);
            x
        });
        Ok(Self {
            extradata,
            layer,
            tols,
            output_layers,
            layout: layered_layout(props, tols)?,
        })
    }
}

/// One coded item's decode, detached from the file.
struct CodedJob {
    item_id: u32,
    params: CodecParameters,
    data: Vec<u8>,
    layout: HeifPixelFormat,
    ispe: Option<(u32, u32)>,
    /// The output layers whose pictures the decoder emits in turn
    /// (`None`: a single picture, layer 0).
    layers: Option<Vec<u8>>,
}

impl CodedJob {
    fn run(self, dec: Box<dyn Decoder>) -> Result<HeifFrame> {
        let mut frames = self.run_all(dec)?;
        Ok(frames.swap_remove(0).frame)
    }

    fn run_all(self, mut dec: Box<dyn Decoder>) -> Result<Vec<LayerFrame>> {
        let id = self.item_id;
        let pkt = Packet::new(0, TimeBase::new(1, 1), self.data)
            .with_pts(0)
            .with_keyframe(true);
        dec.send_packet(&pkt).map_err(|e| {
            HeifError::invalid(format!("item {id}: decoder rejected the payload: {e}"))
        })?;
        dec.flush()
            .map_err(|e| HeifError::invalid(format!("item {id}: flush: {e}")))?;
        let want = self.layers.as_ref().map(Vec::len).unwrap_or(1);
        let mut frames: Vec<oxideav_core::VideoFrame> = Vec::with_capacity(want);
        loop {
            match dec.receive_frame() {
                Ok(Frame::Video(v)) => {
                    if frames.len() < want {
                        frames.push(v);
                    }
                }
                Ok(_) => {}
                Err(CoreError::NeedMore) | Err(CoreError::Eof) => break,
                Err(e) => return Err(HeifError::invalid(format!("item {id}: decode failed: {e}"))),
            }
        }
        drop(dec);
        if frames.is_empty() {
            return Err(HeifError::invalid(format!(
                "item {id}: decoder produced no picture"
            )));
        }
        let layers = self.layers.unwrap_or_else(|| vec![0]);
        if frames.len() < layers.len() {
            return Err(HeifError::invalid(format!(
                "item {id}: decoder produced {} of the {} output layers {layers:?}",
                frames.len(),
                layers.len()
            )));
        }
        frames
            .into_iter()
            .zip(layers)
            .map(|(vf, layer_id)| {
                // A layer's own picture tags its layer when the codec
                // says so; otherwise the emission order (increasing
                // nuh_layer_id) is the oinf output-layer order.
                let layer_id = vf
                    .layer()
                    .map(|l| l.layer_id.min(u8::MAX as u16) as u8)
                    .unwrap_or(layer_id);
                Ok(LayerFrame {
                    layer_id,
                    frame: frame_from_planes(vf, self.layout, self.ispe, id)?,
                })
            })
            .collect()
    }
}

#[doc(hidden)]
/// The coded item kinds this crate decodes.
#[derive(Clone, Copy, Debug)]
pub enum CodedKind<'a> {
    /// `hvc1` / `hev1` with its `hvcC`.
    Hevc(&'a HevcConfig),
    /// `av01` with its `av1C`.
    Av1(&'a Av1Config),
    /// `avc1` with its `avcC`.
    Avc(&'a crate::avcc::AvcConfig),
    /// `lhv1` with its `lhvC` (base layer decoded).
    LayeredHevc(&'a crate::lhvc::LhevcConfig),
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

/// The base layer (`nuh_layer_id` 0) of an `lhv1` access unit as an
/// Annex B stream: the record's parameter sets first, then the item's
/// length-prefixed NAL units, every unit filtered to layer 0.
pub fn base_layer_annex_b(cfg: &crate::lhvc::LhevcConfig, item: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(item.len() + 64);
    let mut push = |nal: &[u8]| {
        if crate::lhvc::nuh_layer_id(nal) == Some(0) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
    };
    for n in cfg.nal_units() {
        push(n);
    }
    for n in crate::hvcc::split_length_prefixed(item, cfg.length_size)? {
        push(n);
    }
    if out.is_empty() {
        return Err(HeifError::invalid(
            "lhv1 item carries no base-layer NAL units",
        ));
    }
    Ok(out)
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
            ITEM_TYPE_AVC1 => {
                let cfg = node.properties.avcc().ok_or_else(|| {
                    HeifError::invalid(format!(
                        "item {}: avc1 item without an avcC property (HEIF E.2.3)",
                        node.item.id
                    ))
                })?;
                Ok(ItemKind::Coded(CodedKind::Avc(cfg)))
            }
            ITEM_TYPE_LHV1 => {
                let cfg = node.properties.lhvc().ok_or_else(|| {
                    HeifError::invalid(format!(
                        "item {}: lhv1 item without an lhvC property (HEIF B.2.3.2)",
                        node.item.id
                    ))
                })?;
                Ok(ItemKind::Coded(CodedKind::LayeredHevc(cfg)))
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
    None
}

/// Interpret the planes a codec emitted as a tightly packed
/// [`HeifFrame`] of the announced layout, cropped to `ispe` when the
/// decoded picture is larger (codecs may emit the coded size). A plane
/// the codec already emitted tight at the output size is moved, not
/// copied.
fn frame_from_planes(
    vf: oxideav_core::VideoFrame,
    layout: HeifPixelFormat,
    ispe: Option<(u32, u32)>,
    item_id: u32,
) -> Result<HeifFrame> {
    let bps = layout.bytes_per_sample();
    let need = layout.plane_count();
    let image_planes = vf.image_plane_count();
    if image_planes < need {
        return Err(HeifError::invalid(format!(
            "item {item_id}: decoder emitted {image_planes} planes, layout {layout:?} needs {need}"
        )));
    }
    let (dec_w, dec_h) = {
        let y = &vf.planes[0];
        if y.stride == 0 || y.stride % bps != 0 || y.data.is_empty() {
            return Err(HeifError::invalid(format!(
                "item {item_id}: luma plane stride {} incompatible with {}-byte samples",
                y.stride, bps
            )));
        }
        ((y.stride / bps) as u32, (y.data.len() / y.stride) as u32)
    };
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
    for (p, src) in vf.planes.into_iter().take(need).enumerate() {
        let (pw, ph) = layout.plane_dims(p, w, h);
        let row_bytes = pw as usize * bps;
        if src.stride < row_bytes || src.data.len() < src.stride * (ph as usize - 1) + row_bytes {
            return Err(HeifError::invalid(format!(
                "item {item_id}: plane {p} too small for {pw}x{ph} ({} bytes, stride {})",
                src.data.len(),
                src.stride
            )));
        }
        let tight = row_bytes * ph as usize;
        let data = if src.stride == row_bytes && src.data.len() == tight {
            src.data
        } else if src.stride == row_bytes {
            let mut d = src.data;
            d.truncate(tight);
            d
        } else {
            let mut data = Vec::with_capacity(tight);
            for r in 0..ph as usize {
                data.extend_from_slice(&src.data[r * src.stride..r * src.stride + row_bytes]);
            }
            data
        };
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
    /// Every output image of a layered (`lhv1`) item decoded without an
    /// `lsel` — one per output layer of its `tols` output layer set,
    /// increasing `nuh_layer_id`, each with the item's transforms
    /// applied ([`DecodedImage::frame`] is the first). Empty for
    /// single-layer items and for layered items with an `lsel` (HEIF
    /// §6.5.11: the selected layer is the item's one image).
    pub layers: Vec<LayerFrame>,
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

/// Decodes an item and everything it derives from. Coded items used
/// more than once in the graph (a shared input, an alpha shared by
/// several masters) are decoded once and kept only until their last
/// use; single-use reconstructions move through without copies.
struct Session<'a, 'r, D> {
    file: &'a HeifFile<D>,
    decoder: ItemDecoder<'r>,
    cache: HashMap<u32, HeifFrame>,
    /// Remaining uses of every coded item of the graph(s) being decoded.
    uses: HashMap<u32, usize>,
    decodes: usize,
    /// The extra output layers of a layered root item (see
    /// [`DecodedImage::layers`]).
    root_layers: Vec<LayerFrame>,
}

/// Count the coded items `node` reaches through `dimg` inputs and
/// alpha / depth auxiliaries (the edges [`Session::output`] follows).
fn count_coded_uses(node: &ImageNode, uses: &mut HashMap<u32, usize>) {
    if let ImageKind::Coded(_) = node.kind {
        *uses.entry(node.item.id).or_insert(0) += 1;
    }
    for i in &node.inputs {
        count_coded_uses(i, uses);
    }
    if let Some(a) = &node.alpha {
        count_coded_uses(a, uses);
    }
}

impl<D: AsRef<[u8]>> Session<'_, '_, D> {
    fn count_decode(&mut self, n: usize) -> Result<()> {
        if self.decodes + n > MAX_ITEM_DECODES {
            return Err(HeifError::exhausted(format!(
                "more than {MAX_ITEM_DECODES} coded items in one image"
            )));
        }
        self.decodes += n;
        Ok(())
    }

    /// A decoded coded item: from the cache (the last use takes it
    /// out), or freshly decoded (cached when another use follows).
    fn coded(&mut self, node: &ImageNode) -> Result<HeifFrame> {
        let id = node.item.id;
        let left = self.uses.get(&id).copied().unwrap_or(1).saturating_sub(1);
        self.uses.insert(id, left);
        if let Some(f) = self.cache.remove(&id) {
            if left > 0 {
                self.cache.insert(id, f.clone());
            }
            return Ok(f);
        }
        if let Some(cexg) = node.properties.cexg() {
            if cexg.tile_count() > 1 {
                let f = self.reconstruct_constrained_extents(node, cexg)?;
                if left > 0 {
                    self.cache.insert(id, f.clone());
                }
                return Ok(f);
            }
        }
        self.count_decode(1)?;
        let f = if node.depth_in_chain == 0 && is_multi_output_layered(node) {
            // The root item yields every output layer; the first is the
            // item's image, the rest ride along.
            let mut frames = self.decoder.decode_coded_layers(self.file, node)?;
            let first = frames.remove(0);
            for l in frames.iter_mut() {
                let out =
                    std::mem::replace(&mut l.frame, HeifFrame::zeroed(1, 1, first.frame.format)?);
                l.frame = apply_transforms_owned(out, node.properties.transformative(), false)?;
            }
            self.root_layers = frames;
            first.frame
        } else {
            self.decoder.decode_coded(self.file, node)?
        };
        if left > 0 {
            self.cache.insert(id, f.clone());
        }
        Ok(f)
    }

    /// Reconstructed image of a node (§6.3, first bullet): decoded
    /// picture or derivation result, *before* the node's own
    /// transformative properties.
    fn reconstruct(&mut self, node: &ImageNode) -> Result<HeifFrame> {
        match &node.kind {
            ImageKind::Coded(_) => self.coded(node),
            ImageKind::Grid(g) => {
                let mut canvas = GridCanvas::new(g);
                if self.decoder.threads > 1 && self.parallel_tiles(node) {
                    self.decode_tiles_parallel(&node.inputs, &mut canvas)?;
                } else {
                    for (i, t) in node.inputs.iter().enumerate() {
                        let tile = self.output(t)?;
                        canvas.place(i, &tile)?;
                    }
                }
                canvas.finish()
            }
            ImageKind::Tiled(t) => self.reconstruct_tiled(node, t),
            ImageKind::ColourFormatEnhancement(c) => {
                let mut frames = Vec::with_capacity(node.inputs.len());
                for i in &node.inputs {
                    frames.push(self.output(i)?);
                }
                crate::compose::composite_colour_format_enhancement(
                    c,
                    &frames,
                    node.properties.nclx(),
                )
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
            ImageKind::ToneMap(body) => {
                let base = node.inputs.first().ok_or_else(|| {
                    HeifError::invalid(format!("tmap item {} has no input", node.item.id))
                })?;
                // §6.6.2.4.1: a tmap feeding another derived item is
                // always the fully applied image; the item itself
                // follows the decoder's policy.
                let applied =
                    self.decoder.tone_map == ToneMapOutput::Applied || node.depth_in_chain > 0;
                if !applied {
                    return self.output(base);
                }
                let gain = node.inputs.get(1).ok_or_else(|| {
                    HeifError::invalid(format!("tmap item {} has no gain map input", node.item.id))
                })?;
                let metadata = crate::gainmap::GainMapMetadata::parse_tmap_body(body)?;
                let base_frame = self.output(base)?;
                let gain_frame = self.output(gain)?;
                let bit_depth = node
                    .properties
                    .pixi()
                    .and_then(|p| p.bits_per_channel.first().copied())
                    .filter(|d| (8..=16).contains(d))
                    .unwrap_or(base_frame.format.bit_depth);
                crate::gainmap::reconstruct_tone_map(
                    &base_frame,
                    base.properties.nclx(),
                    &gain_frame,
                    gain.properties.nclx(),
                    node.properties.nclx(),
                    &metadata,
                    bit_depth,
                    self.decoder
                        .reference_white_nits
                        .unwrap_or(crate::gainmap::DEFAULT_HDR_REFERENCE_WHITE_NITS),
                )
            }
        }
    }

    /// `true` when every tile of a grid is a plain coded item — no
    /// transform, auxiliary or unrecognised essential property, used
    /// once — so the tiles can decode as independent jobs.
    fn parallel_tiles(&self, grid: &ImageNode) -> bool {
        grid.inputs.len() > 1
            && grid.inputs.iter().all(|t| {
                matches!(t.kind, ImageKind::Coded(_))
                    && t.alpha.is_none()
                    && t.properties.transformative().next().is_none()
                    && t.properties.unsupported_essential().is_empty()
                    && self.uses.get(&t.item.id).copied().unwrap_or(1) == 1
                    && !self.cache.contains_key(&t.item.id)
            })
    }

    /// Decode the tiles of a grid on up to `threads` workers, placing
    /// each into `canvas` as it completes (the canvas plus the tiles in
    /// flight is all that is held). Codec instances are made on the
    /// workers and run serial; the placement is order-independent, so
    /// the canvas is byte-identical to the serial decode.
    fn decode_tiles_parallel(
        &mut self,
        tiles: &[ImageNode],
        canvas: &mut GridCanvas,
    ) -> Result<()> {
        self.count_decode(tiles.len())?;
        let mut jobs = Vec::with_capacity(tiles.len());
        for (i, t) in tiles.iter().enumerate() {
            self.uses.insert(t.item.id, 0);
            jobs.push((i, self.decoder.prepare(self.file, t)?));
        }
        self.run_tile_jobs(jobs, canvas)
    }

    /// A `tili` item (Amd 2:2026 §6.11): the tiles addressed by the
    /// item's `deti` offset table, each decoded as a coded picture of
    /// `tile_item_type` under the `tilC`-associated properties, placed
    /// row-major into a `tile_width × tile_height` grid and cropped to
    /// the `ispe` (§6.11.2: the padding beyond the true size). Empty
    /// tiles (offset all ones) render as neutral grey — the text leaves
    /// them to the reader.
    fn reconstruct_tiled(
        &mut self,
        node: &ImageNode,
        t: &crate::derived::TiledItem,
    ) -> Result<HeifFrame> {
        let id = node.item.id;
        let cfg = &t.config;
        if !cfg.extra_dimensions.is_empty() {
            return Err(HeifError::unsupported(format!(
                "tili item {id}: {} extra dimensions (only 2-D tiled images are composed)",
                cfg.extra_dimensions.len()
            )));
        }
        let Some((tile_type, _)) = &cfg.in_file_tiles else {
            return Err(HeifError::unsupported(format!(
                "tili item {id}: tiles stored in external files"
            )));
        };
        let (w, h) = node.reconstructed_size()?;
        let (cols, rows) = cfg
            .tile_grid(w, h)
            .ok_or_else(|| HeifError::invalid(format!("tili item {id}: zero tile size")))?;
        let num_tiles = cfg
            .tile_count(w, h)
            .ok_or_else(|| HeifError::exhausted(format!("tili item {id}: tile count overflow")))?;
        if num_tiles > crate::tiled::MAX_TILES {
            return Err(HeifError::exhausted(format!(
                "tili item {id}: {num_tiles} tiles (cap {})",
                crate::tiled::MAX_TILES
            )));
        }
        if rows > u16::MAX as u32 || cols > u16::MAX as u32 {
            return Err(HeifError::exhausted(format!(
                "tili item {id}: {cols}x{rows} tiles"
            )));
        }
        let meta = self.file.meta()?;
        let loc = meta
            .location(id)
            .ok_or_else(|| HeifError::invalid(format!("tili item {id} has no iloc entry")))?;
        let dref = match loc.data_reference_index {
            0 => None,
            n => meta.data_references.get(n as usize - 1),
        }
        .filter(|d| &d.entry_type == b"deti")
        .ok_or_else(|| {
            HeifError::invalid(format!(
                "tili item {id}: data_reference_index {} does not name a deti entry (Amd 2 §6.11.2)",
                loc.data_reference_index
            ))
        })?;
        let deti = crate::tiled::DataEntryTiledItem::parse(dref)?;
        let data = self.file.item_data(id)?;
        let spans = deti.tile_spans(&data, num_tiles)?;
        let desc = crate::derived::GridDescriptor {
            rows: rows as u16,
            columns: cols as u16,
            output_width: w,
            output_height: h,
        };
        let mut canvas = GridCanvas::new(&desc);
        // Every tile is "a coded image item of tile_item_type" with the
        // tilC-associated properties.
        let tile_node = ImageNode {
            item: crate::meta::ItemInfo {
                item_type: *tile_type,
                ..node.item.clone()
            },
            kind: ImageKind::Coded(*tile_type),
            properties: t.tile_properties.clone(),
            inputs: Vec::new(),
            alpha: None,
            depth: None,
            other_auxiliaries: Vec::new(),
            premultiplied_alpha: false,
            thumbnails: Vec::new(),
            metadata: Vec::new(),
            depth_in_chain: node.depth_in_chain + 1,
        };
        let mut jobs = Vec::new();
        let mut empty = Vec::new();
        for (i, span) in spans.iter().enumerate() {
            match span {
                Some((off, len)) => {
                    let bytes = &data[*off as usize..(*off + *len) as usize];
                    let job = self.decoder.prepare_with(
                        &tile_node,
                        |_| Ok(std::borrow::Cow::Borrowed(bytes)),
                        Some((cfg.tile_width, cfg.tile_height)),
                    )?;
                    jobs.push((i, job));
                }
                None => empty.push(i),
            }
        }
        if jobs.is_empty() {
            return Err(HeifError::unsupported(format!(
                "tili item {id}: every tile is empty"
            )));
        }
        self.count_decode(jobs.len())?;
        self.run_tile_jobs(jobs, &mut canvas)?;
        for i in empty {
            canvas.place_blank(i)?;
        }
        canvas.finish()
    }

    /// A coded item with a `cexg` property (HEIF Amd 1:2025 §6.5.41): every
    /// `iloc` extent is one independently decodable tile of a
    /// `rows × columns` grid of `image_tile_width × image_tile_height`
    /// tiles, row-major in extent order, cropped to the `ispe`.
    fn reconstruct_constrained_extents(
        &mut self,
        node: &ImageNode,
        cexg: &crate::props::Cexg,
    ) -> Result<HeifFrame> {
        let id = node.item.id;
        let (w, h) = node.reconstructed_size()?;
        let extents = self.file.item_extents(id)?;
        if extents.len() != cexg.tile_count() {
            return Err(HeifError::invalid(format!(
                "item {id}: {} iloc extents for a cexg of {}x{} tiles (shall be equal, §6.5.41.1)",
                extents.len(),
                cexg.columns,
                cexg.rows
            )));
        }
        let covers = cexg.tile_width as u64 * cexg.columns as u64 >= w as u64
            && cexg.tile_height as u64 * cexg.rows as u64 >= h as u64;
        if cexg.tile_width == 0 || cexg.tile_height == 0 || !covers {
            return Err(HeifError::invalid(format!(
                "item {id}: cexg {}x{} tiles of {}x{} do not cover the {w}x{h} image",
                cexg.columns, cexg.rows, cexg.tile_width, cexg.tile_height
            )));
        }
        if cexg.rows > u16::MAX as u32 || cexg.columns > u16::MAX as u32 {
            return Err(HeifError::exhausted(format!(
                "item {id}: cexg {}x{} tiles",
                cexg.columns, cexg.rows
            )));
        }
        if cexg.extent_config.is_some() {
            // The per-extent ExtentDecoderConfigurationRecord is
            // codec-specific and undefined for the codecs here; the
            // item's own configuration properties decode the tiles.
            return Err(HeifError::unsupported(format!(
                "item {id}: cexg with an ExtentDecoderConfigurationRecord"
            )));
        }
        let desc = crate::derived::GridDescriptor {
            rows: cexg.rows as u16,
            columns: cexg.columns as u16,
            output_width: w,
            output_height: h,
        };
        let mut canvas = GridCanvas::new(&desc);
        let mut jobs = Vec::with_capacity(extents.len());
        for (i, bytes) in extents.iter().enumerate() {
            let job = self.decoder.prepare_with(
                node,
                |_| Ok(std::borrow::Cow::Borrowed(*bytes)),
                Some((cexg.tile_width, cexg.tile_height)),
            )?;
            jobs.push((i, job));
        }
        self.count_decode(jobs.len())?;
        self.run_tile_jobs(jobs, &mut canvas)?;
        canvas.finish()
    }

    /// Run prepared tile jobs on up to `threads` workers (inline when
    /// the budget is serial), placing each result into `canvas`.
    fn run_tile_jobs(
        &mut self,
        jobs: Vec<(usize, CodedJob)>,
        canvas: &mut GridCanvas,
    ) -> Result<()> {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Mutex};
        let workers =
            ExecutionContext::with_threads(self.decoder.threads).effective_workers(jobs.len());
        if workers == 1 {
            for (i, job) in jobs {
                let dec = self.decoder.make(&job.params)?;
                let f = job.run(dec)?;
                canvas.place(i, &f)?;
            }
            return Ok(());
        }
        let queue = Mutex::new(jobs.into_iter());
        let abort = AtomicBool::new(false);
        let decoder = self.decoder;
        let (tx, rx) = mpsc::sync_channel::<(usize, Result<HeifFrame>)>(workers);
        std::thread::scope(|scope| -> Result<()> {
            for _ in 0..workers {
                let tx = tx.clone();
                let (queue, abort) = (&queue, &abort);
                scope.spawn(move || loop {
                    if abort.load(Ordering::Relaxed) {
                        break;
                    }
                    let next = queue.lock().map(|mut q| q.next()).unwrap_or(None);
                    let Some((i, job)) = next else {
                        break;
                    };
                    let r = decoder.make(&job.params).and_then(|d| job.run(d));
                    if tx.send((i, r)).is_err() {
                        break;
                    }
                });
            }
            drop(tx);
            let mut result = Ok(());
            for (i, r) in rx.iter() {
                if result.is_err() {
                    continue;
                }
                match r.and_then(|f| canvas.place(i, &f)) {
                    Ok(()) => {}
                    Err(e) => {
                        abort.store(true, Ordering::Relaxed);
                        result = Err(e);
                    }
                }
            }
            result
        })
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
        let mut out = apply_transforms_owned(rec, node.properties.transformative(), false)?;
        if let Some(a) = &node.alpha {
            let alpha = self.output(a)?;
            out = attach_alpha(&out, &alpha)?;
        }
        Ok(out)
    }
}

/// Decode `item_id` (coded or derived) to its output image with alpha,
/// depth, colour information and metadata resolved.
pub fn decode_item<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    item_id: u32,
    decoder: ItemDecoder<'_>,
) -> Result<DecodedImage> {
    let node = build_graph(file, item_id)?;
    let mut uses = HashMap::new();
    count_coded_uses(&node, &mut uses);
    if let Some(d) = &node.depth {
        count_coded_uses(d, &mut uses);
    }
    if let Some((_, _, inputs)) = gain_map_target(file, &node)? {
        if let Some(g) = inputs.get(1) {
            if let Ok(gain_node) = build_graph(file, *g) {
                count_coded_uses(&gain_node, &mut uses);
            }
        }
    }
    let mut session = Session {
        file,
        decoder,
        cache: HashMap::new(),
        uses,
        decodes: 0,
        root_layers: Vec::new(),
    };
    let frame = session.output(&node)?;
    let depth = match &node.depth {
        Some(d) => Some(session.output(d)?),
        None => None,
    };
    // A tmap decoded to its base rendition carries the base's colour
    // information; the applied rendition carries the tmap's (§6.6.2.4.1).
    let colour_node = match (&node.kind, decoder.tone_map, node.inputs.first()) {
        (ImageKind::ToneMap(_), ToneMapOutput::Base, Some(base)) => base,
        _ => &node,
    };
    let (nclx, nclx_explicit) = match colour_node.properties.nclx() {
        Some(c) => (c.clone(), true),
        None => (Colr::MIAF_DEFAULT, false),
    };
    let icc_profile = colour_node.properties.icc_profile().map(<[u8]>::to_vec);
    let gain_map = find_gain_map(file, &node, &mut session)?;
    let mut exif = None;
    let mut xmp = None;
    for m in &node.metadata {
        if m.item_type == crate::meta::ITEM_TYPE_EXIF && exif.is_none() {
            exif = Some(exif_payload(&file.item_data(m.id)?)?);
        } else if m.item_type == crate::meta::ITEM_TYPE_DEXF && exif.is_none() {
            // Amd 2:2026 A.2.1: a deflate()-compressed ExifDataBlock.
            if let Some(block) = inflate_metadata(&file.item_data(m.id)?)? {
                exif = Some(exif_payload(&block)?);
            }
        } else if m.is_xmp() && xmp.is_none() {
            let body = file.item_data(m.id)?;
            let deflated = m
                .content_encoding
                .as_deref()
                .map(|e| e.eq_ignore_ascii_case("deflate"))
                .unwrap_or(false);
            if deflated {
                if let Some(packet) = inflate_metadata(&body)? {
                    xmp = Some(String::from_utf8_lossy(&packet).into_owned());
                }
            } else {
                xmp = Some(String::from_utf8_lossy(&body).into_owned());
            }
        }
    }
    let mut img = DecodedImage {
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
        layers: Vec::new(),
    };
    if !session.root_layers.is_empty() {
        img.layers.push(LayerFrame {
            layer_id: first_output_layer(&node),
            frame: img.frame.clone(),
        });
        img.layers.append(&mut session.root_layers);
    }
    Ok(img)
}

/// `true` for an `lhv1` item whose `tols` output layer set has more
/// than one output layer and that carries no `lsel`.
fn is_multi_output_layered(node: &ImageNode) -> bool {
    node.item.item_type == ITEM_TYPE_LHV1
        && node.properties.lsel().is_none()
        && node.properties.hvcc().is_some()
        && node
            .properties
            .oinf()
            .map(|o| o.output_layers(node.properties.tols().unwrap_or(0)).len() > 1)
            .unwrap_or(false)
}

/// The lowest output layer of a layered item's `tols` set (0 otherwise).
fn first_output_layer(node: &ImageNode) -> u8 {
    node.properties
        .oinf()
        .and_then(|o| {
            o.output_layers(node.properties.tols().unwrap_or(0))
                .into_iter()
                .min()
        })
        .unwrap_or(0)
}

/// Locate and decode the gain map of `node`: the node itself when it
/// is a `tmap`, else a `tmap` item whose first `dimg` input is the
/// node. The `tmap` body is the ISO 21496-1 C.2 metadata; its second
/// input is the gain-map image. An unparsable / unsupported payload
/// yields `None` (C.2.3: fall back to the base image).
fn find_gain_map<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    node: &ImageNode,
    session: &mut Session<'_, '_, D>,
) -> Result<Option<GainMapAttachment>> {
    let meta = file.meta()?;
    let Some((tmap_id, body, inputs)) = gain_map_target(file, node)? else {
        return Ok(None);
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

/// The `tmap` item (id, body, `dimg` inputs) whose gain map belongs to
/// `node`: the node itself when it is a `tmap`, else a `tmap` whose
/// first input is the node.
#[allow(clippy::type_complexity)]
fn gain_map_target<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    node: &ImageNode,
) -> Result<Option<(u32, Vec<u8>, Vec<u32>)>> {
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
    Ok(Some((tmap_id, body, inputs)))
}

/// Decode the primary item (`pitm`).
pub fn decode_primary<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    decoder: ItemDecoder<'_>,
) -> Result<DecodedImage> {
    let id = file.primary_item()?.id;
    decode_item(file, id, decoder)
}

/// Inflate a `deflate`-encoded metadata item (RFC 1951; `dExf` items
/// and `content_encoding = "deflate"` XMP, HEIF Amd 2:2026 A.2.1 /
/// O.4.3), capped at [`crate::file::MAX_ITEM_BYTES`]. `None` when the
/// crate is built without the `deflate` feature.
pub fn inflate_metadata(body: &[u8]) -> Result<Option<Vec<u8>>> {
    #[cfg(feature = "deflate")]
    {
        compcol::vec::decompress_to_vec_capped::<compcol::deflate::Deflate>(
            body,
            crate::file::MAX_ITEM_BYTES,
        )
        .map(Some)
        .map_err(|e| HeifError::invalid(format!("deflate metadata item: {e}")))
    }
    #[cfg(not(feature = "deflate"))]
    {
        let _ = body;
        Ok(None)
    }
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
        let f = frame_from_planes(vf.clone(), layout, Some((6, 4)), 1).unwrap();
        assert_eq!((f.width, f.height), (6, 4));
        assert_eq!(f.planes[0].stride, 6);
        assert_eq!(f.sample(0, 5, 3), 3 * 8 + 5);
        assert_eq!(f.plane_dims(1), (3, 2));
        assert!(frame_from_planes(vf.clone(), layout, Some((9, 4)), 1).is_err());
        let g = frame_from_planes(vf.clone(), layout, None, 1).unwrap();
        assert_eq!((g.width, g.height), (8, 8));
        let mono = HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap();
        let m = frame_from_planes(vf, mono, Some((8, 8)), 1).unwrap();
        assert_eq!(m.planes.len(), 1);
    }
}
