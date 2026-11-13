//! Coded image items → pixels through the oxideav codec crates
//! (`registry` feature).
//!
//! An `hvc1` / `hev1` item is one HEVC access unit of length-prefixed
//! NAL units (HEIF Annex B.2.2) whose parameter sets live in the `hvcC`
//! property; `oxideav-h265`'s registry decoder takes exactly that shape
//! (the `hvcC` record as `extradata`, length-prefixed packets). An
//! `av01` item is one AV1 temporal unit (AVIF §2.1) with the `av1C`
//! record as `extradata`; `oxideav-av1`'s registry decoder consumes it
//! verbatim. A `vvc1` item is one VVC access unit (HEIF L.2.2) with
//! the `vvcC` record as `extradata`; it is decoded by this crate's
//! [`crate::vvcdec`] wrapper over `oxideav-h266`'s stream decoder
//! (that crate's registry factory is a parser-only placeholder), so
//! VVC never goes through a caller-supplied registry.
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
    GridCanvas, OverlayInput, apply_transforms_owned, attach_alpha, composite_overlay,
};
use crate::derived::{ImageKind, ImageNode, build_graph};
use crate::error::{HeifError, Result};
use crate::file::HeifFile;
use crate::hvcc::HevcConfig;
use crate::image::{HeifFrame, HeifPixelFormat, Plane};
use crate::meta::{
    ITEM_TYPE_AV01, ITEM_TYPE_AVC1, ITEM_TYPE_HEV1, ITEM_TYPE_HVC1, ITEM_TYPE_LHV1, ITEM_TYPE_VVC1,
};
use crate::props::{Colr, ItemProperties};
use crate::vvcc::VvcConfig;

pub use crate::layout::{
    av1_layout, hevc_layout, layered_base_layout, layered_layout, layout_of, vvc_layout,
};

/// Codec id the HEVC decoder is registered under.
pub const CODEC_ID_HEVC: &str = "h265";
/// Codec id the AV1 decoder is registered under.
pub const CODEC_ID_AV1: &str = "av1";
/// Codec id the AVC decoder is registered under.
pub const CODEC_ID_AVC: &str = "h264";
/// Codec id the VVC decoder is reached under (see [`crate::vvcdec`]).
pub const CODEC_ID_VVC: &str = crate::vvcdec::CODEC_ID;

/// Upper bound on the number of coded items decoded for one image.
pub const MAX_ITEM_DECODES: usize = 4096;

/// What a `tmap` (tone-map) derived image item decodes to when it is
/// the item being decoded (HEIF Amd 1 §6.6.2.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
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
    /// `true`: an `lhv1` item whose enhancement picture is not an IRAP
    /// picture is refused instead of decoded from its access unit.
    irap_layers_only: bool,
    threads: usize,
    tile_resolver: Option<&'r dyn crate::tiled::ExternalTileResolver>,
    decryptor: Option<&'r dyn crate::cenc::BlockCipher>,
    cenc_reading: crate::cenc::CencReading,
}

impl<'r> ItemDecoder<'r> {
    /// Decode through the direct `oxideav-h265` / `oxideav-av1` factories.
    pub fn direct() -> Self {
        Self {
            registry: None,
            tone_map: ToneMapOutput::Base,
            reference_white_nits: None,
            base_layer_fallback: false,
            irap_layers_only: false,
            threads: 1,
            tile_resolver: None,
            decryptor: None,
            cenc_reading: crate::cenc::CencReading::strict(),
        }
    }

    /// Decode through `registry` (`"h265"` / `"av1"` ids).
    pub fn with_registry(registry: &'r CodecRegistry) -> Self {
        Self {
            registry: Some(registry),
            tone_map: ToneMapOutput::Base,
            reference_white_nits: None,
            base_layer_fallback: false,
            irap_layers_only: false,
            threads: 1,
            tile_resolver: None,
            decryptor: None,
            cenc_reading: crate::cenc::CencReading::strict(),
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

    /// Grant a thread budget as a plain worker count (`0` / `1` =
    /// serial); see [`ItemDecoder::with_execution_context`].
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }

    /// Resolve the external tiles of `tili` items (HEIF Amd 2:2026
    /// §6.11.2: one HEIF file per tile, addressed by URL) through
    /// `resolver`. Without one, such an item is the typed refusal
    /// [`HeifError::external_tile_unresolved`] naming the first missing
    /// reference: the decoder never reaches a filesystem or a network
    /// on its own.
    pub fn with_tile_resolver(
        mut self,
        resolver: &'r dyn crate::tiled::ExternalTileResolver,
    ) -> Self {
        self.tile_resolver = Some(resolver);
        self
    }

    /// Decrypt items protected with Common Encryption (an `ipro`
    /// scheme + `ienc` property + `auxr`-referenced `auxi` item,
    /// ISO/IEC 23001-7 Amd 1 §8.3 / §8.4) through `cipher`, the caller's
    /// AES-128 keyed by KID. Without one such an item is the typed
    /// refusal [`HeifError::encrypted_item`].
    pub fn with_decryptor(mut self, cipher: &'r dyn crate::cenc::BlockCipher) -> Self {
        self.decryptor = Some(cipher);
        self
    }

    /// How protection records are read ([`crate::cenc::CencReading`]:
    /// the Amd 1 syntax by default, or also the staged conformance
    /// files' layout).
    pub fn with_cenc_reading(mut self, reading: crate::cenc::CencReading) -> Self {
        self.cenc_reading = reading;
        self
    }

    /// The thread budget ([`ItemDecoder::with_execution_context`]).
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// A codec instance for `params` through the configured route
    /// (registry or direct factories), granted this decoder's thread
    /// budget when the codec runs byte-identical under one
    /// ([`Self::make_budgeted`]).
    pub(crate) fn instantiate(&self, params: &CodecParameters) -> Result<Box<dyn Decoder>> {
        self.make_budgeted(params)
    }

    /// Whether a codec instance for `params` is handed the thread
    /// budget (round 476): the AV1 decoder (oxideav-av1 0.1.21) walks
    /// a multi-tile intra frame tile-parallel and runs its post-filters
    /// by row band under an `ExecutionContext`, byte-identical to the
    /// serial decode for any budget — a 12 MP tiled still decodes in
    /// 0.456 s serial → 0.102 s on 8 workers (12 MP single-item AVIF,
    /// this machine). The HEVC decoder is left serial: a single-item
    /// 12 MP HEIC measured 0.438 s → 0.415 s on 8 workers (r476; r463
    /// had it slower), not worth the threads — its budget is spent on
    /// independent items (grid tiles) instead.
    fn codec_takes_budget(params: &CodecParameters) -> bool {
        params.codec_id.as_str() == CODEC_ID_AV1
    }

    /// [`Self::make`] plus the thread budget for the codecs that take
    /// it ([`Self::codec_takes_budget`]); serial budgets change nothing.
    fn make_budgeted(&self, params: &CodecParameters) -> Result<Box<dyn Decoder>> {
        let mut dec = self.make(params)?;
        if self.threads > 1 && Self::codec_takes_budget(params) {
            dec.set_execution_context(&ExecutionContext::with_threads(self.threads));
        }
        Ok(dec)
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

    /// Decode only the base layer of an `lhv1` item without an `lsel`
    /// (and of one whose selected enhancement layer cannot be output),
    /// instead of its `tols` output layer set — or of the refusal
    /// [`HeifError::layered_hevc`] (recognisable through
    /// [`HeifError::layered_hevc_info`]) for a bitstream that does not
    /// carry those layers.
    pub fn base_layer_fallback(mut self) -> Self {
        self.base_layer_fallback = true;
        self
    }

    /// How an `lhv1` item whose enhancement picture is not an IRAP
    /// picture is treated. Such a layer starts uninitialised (H.265
    /// F.8.1.3) and a conforming decoder withholds its picture from
    /// output (F.8.1.6) — but an HEVC image item is independently
    /// coded (HEIF B.2.2.1.1) and holds exactly one access unit
    /// (B.2.2.1.3), so the picture can only be predicted from the
    /// pictures of that access unit. By default (`false`) the item
    /// therefore decodes: the HEVC decoder is asked to output the
    /// pictures of layers that are not initialised
    /// (`output_uninitialized_layers`). `true` keeps the reading of
    /// B.2.2.1.3 NOTE 1 ("an initial IRAP access unit") and refuses
    /// the item typed — or, with
    /// [`ItemDecoder::base_layer_fallback`], decodes its base layer.
    pub fn with_irap_layers_only(mut self, irap_layers_only: bool) -> Self {
        self.irap_layers_only = irap_layers_only;
        self
    }

    /// The setting of [`ItemDecoder::with_irap_layers_only`].
    pub fn irap_layers_only(&self) -> bool {
        self.irap_layers_only
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
            CodedKind::Hevc(cfg) => {
                // HEIF B.2.2.1.2: an `hvc1` item is its base layer. Its
                // `hvcC` VPS may describe an L-HEVC bitstream whose
                // default output layer set lies above layer 0, so the
                // decoder is pointed at nuh_layer_id 0 (single-layer
                // streams ignore the option).
                options = options.set("layer", "0");
                (CODEC_ID_HEVC, cfg.raw.clone(), hevc_layout(cfg)?)
            }
            CodedKind::Av1(cfg) => (CODEC_ID_AV1, cfg.raw.clone(), av1_layout(cfg)?),
            CodedKind::Avc(cfg) => (CODEC_ID_AVC, cfg.raw.clone(), cfg.layout()?),
            CodedKind::Vvc(cfg) => {
                // HEIF L.2.2.1.2: the item's tols steers the OPI rule.
                if let Some(t) = node.properties.tols() {
                    options = options.set("tols", t.to_string());
                }
                (CODEC_ID_VVC, cfg.to_bytes(), vvc_layout(cfg)?)
            }
            CodedKind::LayeredHevc(cfg) => {
                let plan = LayeredPlan::of(node, cfg)?;
                match &plan.extradata {
                    // hvcC ++ lhvC: the multi-layer decoder's record pair
                    // (HEIF B.2.3.2 / oxideav-h265 `layer` / `ols`).
                    Some(x) => {
                        options = match plan.layer {
                            Some(l) => options.set("layer", l.to_string()),
                            None => options.set(OPTION_OLS, plan.tols.to_string()),
                        };
                        (CODEC_ID_HEVC, x.clone(), plan.layout)
                    }
                    // No base record: the access unit travels Annex B
                    // (every layer's parameter sets in band), so the
                    // decoder gets no extradata, only the operating point.
                    None => {
                        options = match plan.layer {
                            Some(l) => options.set("layer", l.to_string()),
                            None => options.set(OPTION_OLS, plan.tols.to_string()),
                        };
                        (CODEC_ID_HEVC, Vec::new(), plan.layout)
                    }
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
        // VVC always decodes through this crate's wrapper over
        // oxideav-h266's stream decoder: that crate's registry factory
        // is a parser-only placeholder on its published line, so a
        // registry route would resolve to a decoder that rejects every
        // packet.
        if params.codec_id.as_str() == CODEC_ID_VVC {
            return crate::vvcdec::make_decoder(params)
                .map_err(|e| HeifError::unsupported(format!("{}: {e}", params.codec_id)));
        }
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
        // The budget reaches the codecs that run byte-identical under
        // it (AV1); the others stay serial and the budget is spent on
        // independent items (see `codec_takes_budget`).
        let job = self.prepare(file, node)?;
        Ok(job.execute(self, true)?.swap_remove(0).frame)
    }

    /// Everything a coded item's decode needs, gathered from the file
    /// (the codec instance is made separately, so jobs can run on
    /// worker threads without touching the file).
    fn prepare<D: AsRef<[u8]>>(&self, file: &HeifFile<D>, node: &ImageNode) -> Result<CodedJob> {
        let mut job = self.prepare_with(node, |id| self.item_payload(file, id), node.ispe())?;
        job.external_base = self.external_base(file, node, &job)?;
        job.predicted = predicted_stream(file, node, &job.data)?;
        Ok(job)
    }

    /// An item's data, decrypted when the item is protected (an `ipro`
    /// scheme named by its `item_protection_index`, ISO/IEC 23001-7
    /// Amd 1 §8.3 / §8.4): the `ienc` keys, the `CencSampleAuxiliaryDataFormat`
    /// of the `auxi` item it names through `auxr` (the one whose `iaux`
    /// type is the scheme, or that has no `iaux`; the multiple-key form
    /// when `aux_info_type_parameter` is 1) and the caller's cipher.
    fn item_payload<'f, D: AsRef<[u8]>>(
        &self,
        file: &'f HeifFile<D>,
        id: u32,
    ) -> Result<std::borrow::Cow<'f, [u8]>> {
        let meta = file.meta()?;
        let protected = meta.item(id).is_some_and(|i| i.protection_index != 0);
        if !protected {
            return file.item_data(id);
        }
        let scheme = meta
            .protection_scheme(id)
            .and_then(|s| s.scheme_type)
            .ok_or_else(|| {
                HeifError::invalid(format!(
                    "item {id}: item_protection_index names no ipro scheme"
                ))
            })?;
        let props = crate::props::ItemProperties::resolve(meta, id)?;
        let Some(ienc) = props.ienc() else {
            return Err(HeifError::unsupported(format!(
                "item {id}: protected with '{}' without an ienc property",
                crate::boxes::fourcc_str(&scheme)
            )));
        };
        let Some(cipher) = self.decryptor else {
            return Err(HeifError::encrypted_item(
                id,
                &crate::boxes::fourcc_str(&scheme),
            ));
        };
        let mut aux: Option<(u32, u32)> = None;
        for a in meta.references_from(id, b"auxr") {
            let ap = crate::props::ItemProperties::resolve(meta, a)?;
            // No `iaux`, or one whose aux_info_type is all zero (not a
            // four-character code): the implied type is the scheme
            // (Amd 1 §8.4.1).
            let (ty, param) = ap.iaux().map_or((scheme, 0), |x| {
                let ty = if x.aux_info_type == [0; 4] {
                    scheme
                } else {
                    x.aux_info_type
                };
                (ty, x.aux_info_type_parameter)
            });
            if ty == scheme {
                aux = Some((a, param));
                break;
            }
        }
        let reading = self.cenc_reading;
        let swapped;
        let ienc = if reading.compat {
            swapped = ienc.clone().with_swapped_pattern();
            &swapped
        } else {
            ienc
        };
        let aux = match aux {
            Some((a, param)) => {
                crate::cenc::CencAuxData::parse_with(&file.item_data(a)?, ienc, param, reading)?
            }
            // No auxiliary information: constant IVs, the whole item
            // protected (§7.1: empty auxiliary information is omitted).
            None => crate::cenc::CencAuxData::parse(&[], ienc, 0)?,
        };
        let data = file.item_data(id)?;
        crate::cenc::decrypt_with(scheme, ienc, &aux, &data, cipher, reading)
            .map(std::borrow::Cow::Owned)
            .map_err(|e| match e {
                HeifError::InvalidData(m) => HeifError::invalid(format!("item {id}: {m}")),
                other => other,
            })
    }

    /// HEIF B.2.2.1.3: the decoded picture of the image item an `lhv1`
    /// item names through an `exbl` reference serves as its
    /// `nuh_layer_id` 0 picture (H.265 F.8.1.8 hybrid scalability — the
    /// base layer may be coded with another codec, e.g. an `avc1`
    /// item). `None` for every other item and for a base-layer-only
    /// decode ([`ItemDecoder::base_layer_fallback`] without an `lsel`,
    /// which then has no layer to decode — refused below).
    fn external_base<D: AsRef<[u8]>>(
        &self,
        file: &HeifFile<D>,
        node: &ImageNode,
        job: &CodedJob,
    ) -> Result<Option<HeifFrame>> {
        if !matches!(node.kind, ImageKind::Coded(t) if t == ITEM_TYPE_LHV1) {
            return Ok(None);
        }
        let Ok(meta) = file.meta() else {
            return Ok(None);
        };
        let targets = meta.references_from(node.item.id, &crate::meta::reference::EXBL);
        let Some(&base_id) = targets.first() else {
            return Ok(None);
        };
        let id = node.item.id;
        if targets.len() > 1 {
            return Err(HeifError::invalid(format!(
                "item {id}: {} exbl references (one external base layer item)",
                targets.len()
            )));
        }
        if job.base_only {
            return Err(HeifError::invalid(format!(
                "item {id}: the base layer of this lhv1 item is item {base_id} (exbl); decode that item"
            )));
        }
        let base = build_graph(file, base_id)?;
        if !matches!(base.kind, ImageKind::Coded(_)) {
            return Err(HeifError::invalid(format!(
                "item {id}: exbl names item {base_id}, which is not a coded image item"
            )));
        }
        if !meta
            .references_from(base_id, &crate::meta::reference::EXBL)
            .is_empty()
        {
            return Err(HeifError::invalid(format!(
                "item {id}: exbl base item {base_id} has an external base layer of its own"
            )));
        }
        // "the decoded pixel array of that other image item": its
        // reconstructed picture, no transformative property applied.
        self.decode_coded(file, &base).map(Some)
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
        let mut base_only = false;
        let mut expect_layers: Option<(u16, Vec<u8>)> = None;
        let owned = |id: u32| item_data(id).map(std::borrow::Cow::into_owned);
        let (layout, data) = match &kind {
            // HEIF B.2.2.1.2: readers ignore the NAL units with
            // nuh_layer_id > 0 an `hvc1` item may carry (the access unit
            // an `lhv1` item of the same bitstream shares).
            CodedKind::Hevc(c) => (
                hevc_layout(c)?,
                base_layer_units(owned(node.item.id)?, c.length_size),
            ),
            CodedKind::Av1(c) => (av1_layout(c)?, owned(node.item.id)?),
            CodedKind::Avc(c) => (c.layout()?, owned(node.item.id)?),
            CodedKind::Vvc(c) => (vvc_layout(c)?, owned(node.item.id)?),
            CodedKind::LayeredHevc(cfg) => {
                let plan = LayeredPlan::of(node, cfg)?;
                let payload = owned(node.item.id)?;
                // HEIF B.2.2.1.3 NOTE 1: an `lhv1` item is an initial
                // IRAP access unit. A non-IRAP picture of a layer above
                // the base starts that layer uninitialised, and H.265
                // F.8.1.6 sets PicOutputFlag to 0 for it — the item has
                // no picture of that output layer.
                let wanted: Vec<u8> = match plan.layer {
                    Some(l) => vec![l.min(63) as u8],
                    None => plan.output_layers.clone(),
                };
                let non_irap = first_non_irap_layer(&payload, cfg.length_size, &wanted);
                // HEIF B.2.2.1.1: the item is independently coded, so
                // a non-IRAP enhancement picture is predicted from the
                // pictures of its own access unit alone; the decoder
                // outputs it when asked to (the picture F.8.1.6 would
                // withhold). `irap_layers_only` keeps the refusal.
                let uninitialised = non_irap.filter(|_| self.irap_layers_only);
                if non_irap.is_some() && uninitialised.is_none() {
                    params.options = params.options.set(OPTION_UNINITIALIZED_LAYERS, "true");
                }
                // HEIF B.2.2.1.3 + §6.5.11: the tols output layer set
                // decodes in full; an lsel picks one reconstructed
                // image, otherwise every output layer comes back (base
                // first) and the item's image is the first. The caller
                // may settle for the base layer instead.
                let fallback =
                    self.base_layer_fallback && (plan.layer.is_none() || uninitialised.is_some());
                if let (Some((layer, nal_type)), false) = (uninitialised, fallback) {
                    return Err(HeifError::invalid(format!(
                        "item {}: the layer-{layer} picture of this lhv1 item is not an IRAP \
                         picture (nal_unit_type {nal_type}); an lhv1 item is an initial IRAP \
                         access unit (HEIF B.2.2.1.3) and H.265 F.8.1.6 outputs no picture \
                         of a layer that is not initialised",
                        node.item.id
                    )));
                }
                if fallback {
                    params.options = params.options.set("layer", "0");
                    base_only = true;
                } else {
                    if plan.layer.is_none() {
                        layers = Some(plan.output_layers.clone());
                    }
                    expect_layers = Some((plan.tols, wanted));
                }
                let data = match (&plan.extradata, fallback) {
                    // hvcC ++ lhvC extradata: the access unit as stored.
                    (Some(_), _) => payload,
                    // No base record, base layer only: Annex B filtered
                    // to nuh_layer_id 0.
                    (None, true) => base_layer_annex_b(cfg, &payload)?,
                    // No base record: the `lhvC` arrays carry every
                    // layer's parameter sets (the base layer's included
                    // when the base is internal), so the whole access
                    // unit travels Annex B with them in band.
                    (None, false) => layered_annex_b(cfg, &payload)?,
                };
                (plan.layout, data)
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
            base_only,
            expect_layers,
            external_base: None,
            predicted: None,
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
        self.prepare(file, node)?.execute(self, true)
    }
}

/// One reconstructed image of a coded item with the layer it belongs
/// to (single-layer items: layer 0).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LayerFrame {
    /// `nuh_layer_id` of the output layer.
    pub layer_id: u8,
    /// The reconstructed (or, on [`DecodedImage::layers`], output) image.
    pub frame: HeifFrame,
}
impl LayerFrame {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(layer_id: u8, frame: HeifFrame) -> Self {
        Self { layer_id, frame }
    }
}

/// The HEVC decoder option that outputs the pictures of layers that
/// are not initialised (oxideav-h265 0.0.18).
const OPTION_UNINITIALIZED_LAYERS: &str = "output_uninitialized_layers";

/// The HEVC decoder options that pick an output layer set / decode a
/// whole layer set (oxideav-h265 0.0.18 for the latter).
const OPTION_OLS: &str = "ols";
const OPTION_LAYER_SET: &str = "layerset";

/// Pictures read from a layer-set decode of one access unit (a layer
/// set holds at most 64 layers, one picture each).
const MAX_LAYER_SET_PICTURES: usize = 64;

/// `true` when `frames` are, in order, pictures of the layers
/// `expected` (a picture the codec does not tag counts as layer 0).
fn tagged_as(frames: &[(oxideav_core::VideoFrame, StreamShape)], expected: &[u8]) -> bool {
    frames.len() >= expected.len()
        && frames
            .iter()
            .zip(expected)
            .all(|((vf, _), want)| vf.layer().map_or(0, |l| l.layer_id) == u16::from(*want))
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
        if let Some(l) = layer
            && !output_layers.iter().any(|o| *o as u16 == l)
        {
            return Err(HeifError::invalid(format!(
                "item {}: lsel layer {l} is not an output layer of output layer set {tols} ({output_layers:?})",
                node.item.id
            )));
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
    /// An `lhv1` item decoded to its base layer alone
    /// ([`ItemDecoder::base_layer_fallback`]).
    base_only: bool,
    /// An `lhv1` item's operating point: `target_ols_idx` and the
    /// `nuh_layer_id` of each picture the item yields, in order (the
    /// output layers, or the `lsel` layer). A picture the codec does
    /// not tag with that layer means the bitstream did not carry it.
    expect_layers: Option<(u16, Vec<u8>)>,
    /// The `exbl` item's decoded picture: the F.8.1.8 external base
    /// layer of an `lhv1` item (HEIF B.2.2.1.3).
    external_base: Option<HeifFrame>,
    /// A predictively coded item (HEIF §6.4.9): the Annex B bitstream
    /// of its `pred` references' item data in reference order followed
    /// by its own, and the decode-order index of its access unit.
    predicted: Option<(Vec<u8>, u64)>,
}

impl CodedJob {
    /// Decode the job: through a codec instance from `decoder` (made
    /// with the item decoder's budget when `budgeted`), or — for an
    /// `lhv1` item with an external base layer — through the HEVC
    /// sequence decoder fed that base picture.
    fn execute(self, decoder: &ItemDecoder<'_>, budgeted: bool) -> Result<Vec<LayerFrame>> {
        if self.external_base.is_some() {
            return self.run_external_base();
        }
        if self.predicted.is_some() {
            return self.run_predicted();
        }
        let make = |params: &CodecParameters| {
            if budgeted {
                decoder.make_budgeted(params)
            } else {
                decoder.make(params)
            }
        };
        let dec = make(&self.params)?;
        self.run_all(dec, &make)
    }

    /// The parameters that decode this job's access unit through the
    /// codec's layer-set target instead of its output-layer-set one:
    /// every layer of the layer set comes out, whatever the VPS's
    /// output layer sets select. `None` unless the job is a layered
    /// item decoded to the output layers of its `tols` set.
    fn layer_set_params(&self) -> Option<CodecParameters> {
        let (tols, _) = self.expect_layers.as_ref()?;
        self.layers.as_ref()?;
        self.params.options.get(OPTION_OLS)?;
        let mut params = self.params.clone();
        let mut options = oxideav_core::CodecOptions::new();
        for (k, v) in self.params.options.iter() {
            if k != OPTION_OLS {
                options.insert(k, v);
            }
        }
        params.options = options.set(OPTION_LAYER_SET, tols.to_string());
        Some(params)
    }

    /// HEIF §6.4.9: the bitstream formed by the `pred` references'
    /// item data followed by the item's own decodes through one HEVC
    /// sequence decoder; the item's picture is the base-layer picture
    /// of its own (the last) access unit, wherever output order puts it.
    fn run_predicted(mut self) -> Result<Vec<LayerFrame>> {
        use oxideav_h265::sequence::{LayerTarget, SequenceDecoder};
        let id = self.item_id;
        let (stream, au) = self.predicted.take().expect("checked by execute");
        let mut seq = SequenceDecoder::new();
        seq.set_layer_target(LayerTarget::Layer(0));
        let fail = |e: oxideav_h265::sequence::SequenceError| {
            HeifError::invalid(format!(
                "item {id}: predictively coded item decode failed: {e}"
            ))
        };
        seq.push_annexb(&stream).map_err(fail)?;
        let decoded = seq.finish().map_err(fail)?;
        let f = decoded
            .into_iter()
            .find(|f| f.au_index == au && f.layer_id == 0)
            .ok_or_else(|| {
                HeifError::invalid(format!(
                    "item {id}: the predicted bitstream yields no picture for the item's access unit"
                ))
            })?;
        let pic = if f.crop.is_whole(&f.picture) {
            f.picture
        } else {
            f.output_picture()
        };
        Ok(vec![LayerFrame {
            layer_id: 0,
            frame: heif_frame_from_picture(&pic, self.layout, self.ispe, id)?,
        }])
    }

    /// HEIF B.2.2.1.3 `exbl` / H.265 F.8.1.8: the access unit's
    /// enhancement layers decoded against the external base picture
    /// (installed as the layer-0 inter-layer reference of the access
    /// unit; F.8.1.8 treats it as an IRAP picture). The operating point
    /// is the item's (`layer` / `ols` option as for the registry path).
    fn run_external_base(mut self) -> Result<Vec<LayerFrame>> {
        use oxideav_h265::sequence::{ExternalBasePicture, LayerTarget, SequenceDecoder};
        let id = self.item_id;
        let base = self.external_base.take().expect("checked by execute");
        let picture = h265_picture(&base);
        let option = |k: &str| -> Option<u32> { self.params.options.get(k)?.parse().ok() };
        let target = match (option("layer"), option("ols")) {
            (Some(l), _) if l < 64 => LayerTarget::Layer(l as u8),
            (_, Some(o)) => LayerTarget::Ols(o as usize),
            _ => LayerTarget::HighestOls,
        };
        let uninitialized = self.params.options.get(OPTION_UNINITIALIZED_LAYERS) == Some("true");
        let fail = |e: oxideav_h265::sequence::SequenceError| {
            HeifError::invalid(format!(
                "item {id}: decode against the external base layer failed: {e}"
            ))
        };
        let run = |target: LayerTarget| {
            let picture = picture.clone();
            let mut seq = SequenceDecoder::new();
            seq.set_layer_target(target);
            seq.set_output_uninitialized_layers(uninitialized);
            seq.set_external_base_layer(move |_| Some(ExternalBasePicture::new(picture.clone())));
            seq.push_annexb(&self.data).map_err(fail)?;
            let mut decoded = seq.finish().map_err(fail)?;
            decoded.retain(|f| f.output);
            decoded.sort_by_key(|f| f.layer_id);
            Ok::<_, HeifError>(decoded)
        };
        let mut decoded = run(target)?;
        // As in `run_all`: when the VPS's output layer set yields other
        // layers than the item's `oinf` names, the layer set is decoded
        // in full and those layers are taken from it.
        if let (LayerTarget::Ols(o), Some(_), Some((_, expected))) =
            (target, &self.layers, &self.expect_layers)
            && !decoded
                .iter()
                .map(|f| f.layer_id)
                .eq(expected.iter().copied())
            && let Ok(mut all) = run(LayerTarget::LayerSet(o))
        {
            all.retain(|f| expected.contains(&f.layer_id));
            if all.iter().map(|f| f.layer_id).eq(expected.iter().copied()) {
                decoded = all;
            }
        }
        if decoded.is_empty() {
            return Err(HeifError::invalid(format!(
                "item {id}: decoder produced no picture"
            )));
        }
        let want = self.layers.as_ref().map(Vec::len).unwrap_or(1);
        if decoded.len() < want {
            return Err(HeifError::invalid(format!(
                "item {id}: decoder produced {} of the {want} output layers",
                decoded.len()
            )));
        }
        decoded.truncate(want);
        if let Some((tols, expected)) = &self.expect_layers {
            let got: Vec<u8> = decoded.iter().map(|f| f.layer_id).collect();
            if got != *expected {
                return Err(HeifError::layered_hevc(id, *tols));
            }
        }
        decoded
            .into_iter()
            .map(|f| {
                let layer_id = f.layer_id;
                let pic = if f.crop.is_whole(&f.picture) {
                    f.picture
                } else {
                    f.output_picture()
                };
                Ok(LayerFrame {
                    layer_id,
                    frame: heif_frame_from_picture(&pic, self.layout, self.ispe, id)?,
                })
            })
            .collect()
    }

    fn run_all(
        self,
        dec: Box<dyn Decoder>,
        make: &dyn Fn(&CodecParameters) -> Result<Box<dyn Decoder>>,
    ) -> Result<Vec<LayerFrame>> {
        let id = self.item_id;
        let layer_set = self.layer_set_params();
        let pkt = Packet::new(0, TimeBase::new(1, 1), self.data)
            .with_pts(0)
            .with_keyframe(true);
        let want = self.layers.as_ref().map(Vec::len).unwrap_or(1);
        // Each picture with the shape it is read at: the item's own
        // (layout, ispe) until the codec announces a mid-stream change
        // with a `Frame::StreamParams` event (oxideav-core 0.1.43; a
        // later output layer of a different size or layout), the
        // announced one from then on. Never a picture itself.
        let start = (self.layout, self.ispe);
        let pump = |mut dec: Box<dyn Decoder>,
                    limit: usize|
         -> Result<Vec<(oxideav_core::VideoFrame, StreamShape)>> {
            dec.send_packet(&pkt).map_err(|e| {
                HeifError::invalid(format!("item {id}: decoder rejected the payload: {e}"))
            })?;
            dec.flush()
                .map_err(|e| HeifError::invalid(format!("item {id}: flush: {e}")))?;
            let mut shape = start;
            let mut frames = Vec::with_capacity(limit.min(4));
            loop {
                match dec.receive_frame() {
                    Ok(Frame::Video(v)) => {
                        if frames.len() < limit {
                            frames.push((v, shape));
                        }
                    }
                    Ok(Frame::StreamParams(p)) => adopt_stream_params(&p, &mut shape),
                    Ok(_) => {}
                    Err(CoreError::NeedMore) | Err(CoreError::Eof) => break,
                    Err(e) => {
                        return Err(HeifError::invalid(format!("item {id}: decode failed: {e}")));
                    }
                }
            }
            Ok(frames)
        };
        let mut frames = pump(dec, want)?;
        // The item's `oinf` names the output layers of its `tols` set;
        // the codec derives them from the VPS, whose default output
        // layer rule may select fewer (a primary picture next to an
        // auxiliary alpha / depth layer when only the highest layer is
        // an output layer). The layer set is then decoded in full and
        // the layers the `oinf` names are taken from it.
        if let (Some(params), Some((_, expected))) = (layer_set, &self.expect_layers)
            && !tagged_as(&frames, expected)
            && let Ok(dec) = make(&params)
            && let Ok(all) = pump(dec, MAX_LAYER_SET_PICTURES)
        {
            let mut picked = Vec::with_capacity(expected.len());
            for want in expected {
                if let Some(f) = all
                    .iter()
                    .find(|(vf, _)| vf.layer().map(|l| l.layer_id) == Some(u16::from(*want)))
                {
                    picked.push(f.clone());
                }
            }
            if picked.len() == expected.len() {
                frames = picked;
            }
        }
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
        if let Some((tols, expected)) = &self.expect_layers {
            for ((vf, _), want) in frames.iter().zip(expected) {
                match vf.layer().map(|l| l.layer_id) {
                    Some(l) if l == u16::from(*want) => {}
                    // An untagged picture is a single-layer decode: the
                    // bitstream (its VPS) does not describe the layers
                    // the item's oinf / tols name.
                    None if *want == 0 => {}
                    None => return Err(HeifError::layered_hevc(id, *tols)),
                    Some(l) => {
                        return Err(HeifError::invalid(format!(
                            "item {id}: decoder output layer {l} where output layer set {tols} \
                             names layer {want}"
                        )));
                    }
                }
            }
        }
        frames
            .into_iter()
            .zip(layers)
            .map(|((vf, (layout, ispe)), layer_id)| {
                // A layer's own picture tags its layer when the codec
                // says so; otherwise the emission order (increasing
                // nuh_layer_id) is the oinf output-layer order.
                let layer_id = vf
                    .layer()
                    .map(|l| l.layer_id.min(u8::MAX as u16) as u8)
                    .unwrap_or(layer_id);
                Ok(LayerFrame {
                    layer_id,
                    frame: frame_from_planes(vf, layout, ispe, id)?,
                })
            })
            .collect()
    }
}

/// A predictively coded item's bitstream (HEIF §6.4.9): `None` for an
/// item without `pred` references; else the `hvcC` parameter sets, the
/// item data of every referenced item in reference order ("the list of
/// referenced items ... shall indicate the decoding order") and the
/// item's own access unit `own` (its prepared payload), Annex B, with
/// the decode-order index of that last access unit. HEVC items decode
/// this way; a predictively coded item of another coding format is a
/// typed refusal.
fn predicted_stream<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    node: &ImageNode,
    own: &[u8],
) -> Result<Option<(Vec<u8>, u64)>> {
    let ImageKind::Coded(item_type) = node.kind else {
        return Ok(None);
    };
    let Ok(meta) = file.meta() else {
        return Ok(None);
    };
    let id = node.item.id;
    let refs = meta.references_from(id, &crate::meta::reference::PRED);
    if refs.is_empty() {
        return Ok(None);
    }
    let Some(cfg) = node
        .properties
        .hvcc()
        .filter(|_| matches!(item_type, ITEM_TYPE_HVC1 | ITEM_TYPE_HEV1))
    else {
        return Err(HeifError::unsupported(format!(
            "item {id}: predictively coded '{}' items are decoded for HEVC only",
            crate::boxes::fourcc_str(&item_type)
        )));
    };
    if refs.contains(&id) || refs.len() >= MAX_ITEM_DECODES {
        return Err(HeifError::invalid(format!(
            "item {id}: pred references {refs:?} (§6.4.9: other items, in decoding order)"
        )));
    }
    let mut out = Vec::with_capacity(own.len() * (refs.len() + 1));
    let mut push = |nal: &[u8]| {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    };
    for arr in &cfg.arrays {
        for n in &arr.nal_units {
            push(n);
        }
    }
    for r in &refs {
        let data = base_layer_units(file.item_data(*r)?.into_owned(), cfg.length_size);
        for n in crate::hvcc::split_length_prefixed(&data, cfg.length_size)? {
            push(n);
        }
    }
    for n in crate::hvcc::split_length_prefixed(own, cfg.length_size)? {
        push(n);
    }
    Ok(Some((out, refs.len() as u64)))
}

/// An HEVC [`oxideav_h265::picture::Picture`] holding `frame`'s
/// samples (the F.8.1.8 external base layer picture).
fn h265_picture(frame: &HeifFrame) -> oxideav_h265::picture::Picture {
    use crate::image::Chroma;
    let cat = match frame.format.chroma {
        Chroma::Mono => 0,
        Chroma::Yuv420 => 1,
        Chroma::Yuv422 => 2,
        Chroma::Yuv444 => 3,
    };
    let plane = |i: usize| -> Vec<u16> {
        if i >= frame.format.plane_count() || (cat == 0 && i > 0) {
            return Vec::new();
        }
        let (w, h) = frame.plane_dims(i);
        let mut v = Vec::with_capacity(w as usize * h as usize);
        for y in 0..h {
            for x in 0..w {
                v.push(frame.sample(i, x, y));
            }
        }
        v
    };
    let depth = frame.format.bit_depth;
    oxideav_h265::picture::Picture::from_planes(
        frame.width as usize,
        frame.height as usize,
        cat,
        depth,
        depth,
        plane(0),
        plane(1),
        plane(2),
    )
}

/// A decoded HEVC picture as a [`HeifFrame`] of `layout` (the sample
/// structure the picture carries; its bit depth when it differs),
/// cropped to `ispe` when the picture is larger.
fn heif_frame_from_picture(
    pic: &oxideav_h265::picture::Picture,
    layout: HeifPixelFormat,
    ispe: Option<(u32, u32)>,
    item_id: u32,
) -> Result<HeifFrame> {
    use crate::image::Chroma;
    use oxideav_h265::picture::Plane as HPlane;
    let chroma = match pic.chroma_array_type() {
        0 => Chroma::Mono,
        1 => Chroma::Yuv420,
        2 => Chroma::Yuv422,
        _ => Chroma::Yuv444,
    };
    let depth = pic.bit_depth_luma().max(pic.bit_depth_chroma());
    let format = if layout.chroma == chroma && layout.bit_depth == depth {
        layout
    } else {
        HeifPixelFormat::new(chroma, depth, false)?
    };
    let (pw, ph) = (pic.width_luma() as u32, pic.height_luma() as u32);
    let (w, h) = match ispe {
        Some((w, h)) if w <= pw && h <= ph => (w, h),
        Some((w, h)) => {
            return Err(HeifError::invalid(format!(
                "item {item_id}: decoded {pw}x{ph} picture is smaller than its ispe {w}x{h}"
            )));
        }
        None => (pw, ph),
    };
    let mut frame = HeifFrame::zeroed(w, h, format)?;
    let planes = [HPlane::Luma, HPlane::Cb, HPlane::Cr];
    for (i, hp) in planes.iter().enumerate().take(format.plane_count()) {
        let (fw, fh) = frame.plane_dims(i);
        for y in 0..fh {
            for x in 0..fw {
                let v = pic.sample(*hp, x as usize, y as usize);
                frame.set_sample(i, x, y, v as u16);
            }
        }
    }
    Ok(frame)
}

/// The layout and output size decoded pictures are read at
/// ([`frame_from_planes`]).
pub(crate) type StreamShape = (HeifPixelFormat, Option<(u32, u32)>);

/// Fold a codec's `Frame::StreamParams` event (oxideav-core 0.1.43: the
/// shape of the pictures that follow) into `shape`: the announced
/// width / height and — when it is one this crate reads — pixel
/// layout replace the current ones; fields the event leaves unset keep
/// theirs.
pub(crate) fn adopt_stream_params(p: &CodecParameters, shape: &mut StreamShape) {
    if let (Some(w), Some(h)) = (p.width, p.height)
        && w > 0
        && h > 0
    {
        shape.1 = Some((w, h));
    }
    if let Some(layout) = p.pixel_format.and_then(|pf| {
        HeifPixelFormat::from_core(pf).or_else(|| HeifPixelFormat::from_core_gbr(pf))
    }) {
        shape.0 = layout;
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
    /// `lhv1` with its `lhvC` (multi-layer decode).
    LayeredHevc(&'a crate::lhvc::LhevcConfig),
    /// `vvc1` with its `vvcC` (HEIF Annex L).
    Vvc(&'a VvcConfig),
}

/// An `hvc1` item's payload without the NAL units of layers above the
/// base (HEIF B.2.2.1.2: readers shall ignore NAL units with
/// `nuh_layer_id` greater than 0 in an item of type `hvc1`). The
/// length-prefixed form is kept; a payload with no such unit — or one
/// whose framing does not parse, which the decoder then reports — is
/// returned unchanged.
pub(crate) fn base_layer_units(data: Vec<u8>, length_size: u8) -> Vec<u8> {
    let Ok(units) = crate::hvcc::split_length_prefixed(&data, length_size) else {
        return data;
    };
    let above = |n: &&[u8]| crate::lhvc::nuh_layer_id(n).is_some_and(|l| l > 0);
    if !units.iter().any(above) {
        return data;
    }
    let ls = length_size as usize;
    let mut out = Vec::with_capacity(data.len());
    for n in units.iter().filter(|n| !above(n)) {
        out.extend_from_slice(&(n.len() as u32).to_be_bytes()[4 - ls..]);
        out.extend_from_slice(n);
    }
    out
}

/// The first of `layers` (above the base) whose VCL NAL units in the
/// length-prefixed access unit `item` are not IRAP NAL unit types
/// (H.265 Table 7-1: 16..=23), with that type. `None` when every such
/// picture is IRAP, or the framing does not parse (the decoder reports
/// that).
fn first_non_irap_layer(item: &[u8], length_size: u8, layers: &[u8]) -> Option<(u8, u8)> {
    let units = crate::hvcc::split_length_prefixed(item, length_size).ok()?;
    units.iter().find_map(|n| {
        let layer = crate::lhvc::nuh_layer_id(n)?;
        let nal_type = (n[0] >> 1) & 0x3f;
        (layer > 0 && nal_type < 32 && !(16..=23).contains(&nal_type) && layers.contains(&layer))
            .then_some((layer, nal_type))
    })
}

/// A whole `lhv1` access unit as an Annex B stream: the `lhvC`
/// record's parameter sets first (VPS, SPS, PPS, then the remaining
/// arrays, each group in record order — HEIF B.2.3.2 / ISO/IEC 14496-15
/// §9.5 do not order the arrays, a decoder activates in arrival order),
/// then the item's length-prefixed NAL units of every layer.
pub fn layered_annex_b(cfg: &crate::lhvc::LhevcConfig, item: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(item.len() + cfg.raw.len() + 64);
    let rank = |nal: &&[u8]| match nal.first().map(|b| (b >> 1) & 0x3f) {
        Some(32) => 0u8,
        Some(33) => 1,
        Some(34) => 2,
        _ => 3,
    };
    let mut sets: Vec<&[u8]> = cfg.nal_units().collect();
    sets.sort_by_key(rank);
    let units = crate::hvcc::split_length_prefixed(item, cfg.length_size)?;
    if units.is_empty() {
        return Err(HeifError::invalid("lhv1 item carries no NAL units"));
    }
    for n in sets.into_iter().chain(units) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    Ok(out)
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
            ITEM_TYPE_VVC1 => {
                let cfg = node.properties.vvcc().ok_or_else(|| {
                    HeifError::invalid(format!(
                        "item {}: vvc1 item without a vvcC property (HEIF L.2.3.1)",
                        node.item.id
                    ))
                })?;
                Ok(ItemKind::Coded(CodedKind::Vvc(cfg)))
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

/// Interpret the planes a codec emitted as a tightly packed
/// [`HeifFrame`] of the announced layout, cropped to `ispe` when the
/// decoded picture is larger (codecs may emit the coded size). A plane
/// the codec already emitted tight at the output size is moved, not
/// copied.
pub(crate) fn frame_from_planes(
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
        if y.stride == 0 || !y.stride.is_multiple_of(bps) || y.data.is_empty() {
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
    // A zero output dimension (an `ispe` or a sample entry declaring
    // 0, a decoder emitting an empty picture) is hostile input, not a
    // picture: refuse before the per-plane arithmetic.
    if w == 0 || h == 0 {
        return Err(HeifError::invalid(format!(
            "item {item_id}: zero output dimension {w}x{h}"
        )));
    }
    let mut out = Vec::with_capacity(need);
    for (p, src) in vf.planes.into_iter().take(need).enumerate() {
        let (pw, ph) = layout.plane_dims(p, w, h);
        let row_bytes = pw as usize * bps;
        let needed = (ph as usize)
            .saturating_sub(1)
            .saturating_mul(src.stride)
            .saturating_add(row_bytes);
        if pw == 0 || ph == 0 || src.stride < row_bytes || src.data.len() < needed {
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
        out.push(Plane {
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
#[non_exhaustive]
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
impl DecodedImage {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        item_id: u32,
        frame: HeifFrame,
        premultiplied_alpha: bool,
        depth: Option<HeifFrame>,
        nclx: Colr,
        nclx_explicit: bool,
        icc_profile: Option<Vec<u8>>,
        exif: Option<Vec<u8>>,
        xmp: Option<String>,
        thumbnail_ids: Vec<u32>,
        properties: ItemProperties,
        gain_map: Option<GainMapAttachment>,
        layers: Vec<LayerFrame>,
    ) -> Self {
        Self {
            item_id,
            frame,
            premultiplied_alpha,
            depth,
            nclx,
            nclx_explicit,
            icc_profile,
            exif,
            xmp,
            thumbnail_ids,
            properties,
            gain_map,
            layers,
        }
    }
}

/// A decoded gain map and its metadata (see [`DecodedImage::gain_map`]).
#[derive(Clone, Debug)]
#[non_exhaustive]
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
impl GainMapAttachment {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default` where one exists, then read / assign its public fields).
    pub fn new(
        tmap_item_id: u32,
        gain_map_item_id: u32,
        metadata: crate::gainmap::GainMapMetadata,
        frame: HeifFrame,
        colr: Option<Colr>,
        alternate_colr: Option<Colr>,
    ) -> Self {
        Self {
            tmap_item_id,
            gain_map_item_id,
            metadata,
            frame,
            colr,
            alternate_colr,
        }
    }
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

    /// An HEVC tile item (HEIF B.2.5): a rectangular set of tiles of
    /// the HEVC image item its `tbas` reference names. Its
    /// reconstructed image is the region of that item's reconstructed
    /// image the `rloc` property places it at (§6.5.7: same pixel
    /// sampling, aligned grids), `ispe` wide and high — so the tile is
    /// read out of the picture it belongs to, deblocked across tile
    /// boundaries as the picture is (B.2.5 NOTE 1), rather than
    /// decoded from the item's own NAL units. An item without exactly
    /// one `tbas` reference to an HEVC image item, without the `rloc`
    /// B.2.5 requires, or whose region leaves the picture is refused.
    fn tile_item(&mut self, node: &ImageNode) -> Result<HeifFrame> {
        let id = node.item.id;
        let meta = self.file.meta()?;
        let bases = meta.references_from(id, &crate::meta::reference::TBAS);
        let [base] = bases[..] else {
            return Err(HeifError::invalid(format!(
                "hvt1 item {id}: {} 'tbas' references (HEIF §6.5.7: the one HEVC image item \
                 the tiles belong to)",
                bases.len()
            )));
        };
        let rloc = node.properties.rloc().ok_or_else(|| {
            HeifError::invalid(format!(
                "hvt1 item {id} has no rloc property (HEIF B.2.5: its position in the HEVC \
                 image item)"
            ))
        })?;
        let (w, h) = node.reconstructed_size()?;
        let base_node = build_graph(self.file, base)?;
        if !matches!(
            base_node.item.item_type,
            crate::meta::ITEM_TYPE_HVC1 | crate::meta::ITEM_TYPE_HEV1
        ) {
            return Err(HeifError::invalid(format!(
                "hvt1 item {id}: its 'tbas' reference names item {base} ('{}'), not an HEVC \
                 image item",
                crate::boxes::fourcc_str(&base_node.item.item_type)
            )));
        }
        // Several tile items of one picture (a grid of them, a burst)
        // decode it once: the picture stays cached for the session.
        *self.uses.entry(base).or_insert(1) += 1;
        let picture = self.coded(&base_node)?;
        crate::compose::region(&picture, rloc.horizontal_offset, rloc.vertical_offset, w, h)
            .map_err(|e| match e {
                HeifError::InvalidData(m) => HeifError::invalid(format!("hvt1 item {id}: {m}")),
                HeifError::Unsupported(m) => HeifError::unsupported(format!("hvt1 item {id}: {m}")),
                other => other,
            })
    }

    /// A decoded coded item: from the cache (the last use takes it
    /// out), or freshly decoded (cached when another use follows).
    fn coded(&mut self, node: &ImageNode) -> Result<HeifFrame> {
        let id = node.item.id;
        if node.item.item_type == crate::meta::ITEM_TYPE_HVT1 {
            return self.tile_item(node);
        }
        let left = self.uses.get(&id).copied().unwrap_or(1).saturating_sub(1);
        self.uses.insert(id, left);
        if let Some(f) = self.cache.remove(&id) {
            if left > 0 {
                self.cache.insert(id, f.clone());
            }
            return Ok(f);
        }
        if let Some(cexg) = node.properties.cexg()
            && cexg.tile_count() > 1
        {
            let f = self.reconstruct_constrained_extents(node, cexg)?;
            if left > 0 {
                self.cache.insert(id, f.clone());
            }
            return Ok(f);
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
            ImageKind::Grid(g) => self.reconstruct_grid(node, g, &[]),
            ImageKind::Tiled(t) => self.reconstruct_tiled(node, t, &[]),
            ImageKind::PixelTransform(t) => {
                let mut frames = Vec::with_capacity(node.inputs.len());
                for i in &node.inputs {
                    frames.push(self.output(i)?.without_alpha());
                }
                // §6.6.2.6.1: clamped to the item's pixi depth (lenient:
                // the deepest input without one).
                let depth = node
                    .properties
                    .pixi()
                    .and_then(|p| p.bits_per_channel.first().copied())
                    .unwrap_or_else(|| {
                        frames.iter().map(|f| f.format.bit_depth).max().unwrap_or(8)
                    });
                crate::pixel_transform::compose(t, &frames, depth, node.properties.nclx())
            }
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

    /// §6.6.2.3: the reconstructed image of a `grid` item, taken
    /// through `ops` — the rotations and mirrors that lead the item's
    /// transformative chain. The canvas places every tile where the
    /// operations put it (see [`GridCanvas::with_reorientation`]), so
    /// a rotated grid is composed once, in its final orientation,
    /// instead of composed and then copied turned.
    fn reconstruct_grid(
        &mut self,
        node: &ImageNode,
        g: &crate::derived::GridDescriptor,
        ops: &[crate::compose::Reorient],
    ) -> Result<HeifFrame> {
        let mut canvas = GridCanvas::with_reorientation(g, ops);
        if self.decoder.threads > 1 && self.parallel_tiles(node) {
            self.decode_tiles_parallel(&node.inputs, &mut canvas)?;
        } else {
            for (i, t) in node.inputs.iter().enumerate() {
                let tile = self.output(t)?;
                canvas.place(i, &tile)?;
            }
        }
        canvas.finish_threads(self.decoder.threads)
    }

    /// `true` when every tile of a grid is a plain coded item — no
    /// transform, auxiliary or unrecognised essential property, used
    /// once — so the tiles can decode as independent jobs.
    fn parallel_tiles(&self, grid: &ImageNode) -> bool {
        grid.inputs.len() > 1
            && grid.inputs.iter().all(|t| {
                matches!(t.kind, ImageKind::Coded(_))
                    // A tile item is read out of its `tbas` picture.
                    && t.item.item_type != crate::meta::ITEM_TYPE_HVT1
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
        ops: &[crate::compose::Reorient],
    ) -> Result<HeifFrame> {
        let id = node.item.id;
        let cfg = &t.config;
        if !cfg.extra_dimensions.is_empty() {
            return Err(HeifError::unsupported(format!(
                "tili item {id}: {} extra dimensions (only 2-D tiled images are composed)",
                cfg.extra_dimensions.len()
            )));
        }
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
        let desc = crate::derived::GridDescriptor {
            rows: rows as u16,
            columns: cols as u16,
            output_width: w,
            output_height: h,
        };
        if let Some(ext) = &deti.external {
            if deti.input_items != num_tiles {
                return Err(HeifError::invalid(format!(
                    "tili item {id}: deti no_of_input_items {} but the tilC / ispe geometry has {num_tiles} tiles",
                    deti.input_items
                )));
            }
            let handler = meta.handler.as_ref().map(|h| h.handler_type);
            return self.reconstruct_external_tiles(
                node,
                cfg,
                ext,
                (&desc, ops),
                num_tiles,
                handler,
            );
        }
        // §6.11.3.2: in-file tiles carry tile_item_type + tipa.
        let Some((tile_type, _)) = &cfg.in_file_tiles else {
            return Err(HeifError::invalid(format!(
                "tili item {id}: in-file tiles but the tilC has no tile_item_type (Amd 2 §6.11.3.2)"
            )));
        };
        let data = self.file.item_data(id)?;
        let spans = deti.tile_spans(&data, num_tiles)?;
        let mut canvas = GridCanvas::with_reorientation(&desc, ops);
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
        canvas.finish_threads(self.decoder.threads)
    }

    /// The external tiles of a `tili` item (Amd 2:2026 §6.11.2): each
    /// tile file fetched through the decoder's
    /// [`ExternalTileResolver`](crate::tiled::ExternalTileResolver),
    /// checked ([`crate::tiled::check_external_tile`]: a HEIF file, its
    /// only image item the tile, no entity grouping, the tiled item's
    /// `handler_type`), its item of the coding format every tile shares
    /// (the first tile's `item_type`, §6.11.2) with an output of
    /// `tile_width × tile_height`, decoded under the tile file's own
    /// properties and placed row-major; the padding beyond the `ispe`
    /// is cropped. Plain coded tiles run as independent jobs under the
    /// thread budget (byte-identical to serial); a tile carrying its
    /// own transformative properties decodes through its file's graph.
    /// The tile files decode without the resolver: a tile never
    /// reaches further files.
    fn reconstruct_external_tiles(
        &mut self,
        node: &ImageNode,
        cfg: &crate::props::TilC,
        ext: &crate::tiled::ExternalTiles,
        (desc, ops): (&crate::derived::GridDescriptor, &[crate::compose::Reorient]),
        num_tiles: u64,
        handler: Option<crate::boxes::FourCc>,
    ) -> Result<HeifFrame> {
        let id = node.item.id;
        let cols = desc.columns as u32;
        let Some(resolver) = self.decoder.tile_resolver else {
            let first = ext.request(id, 0, cols, num_tiles)?;
            return Err(HeifError::external_tile_unresolved(id, &first.url));
        };
        self.count_decode(num_tiles as usize)?;
        let sub = ItemDecoder {
            tile_resolver: None,
            ..self.decoder
        };
        let mut canvas = GridCanvas::with_reorientation(desc, ops);
        let mut jobs = Vec::new();
        let mut coding: Option<crate::boxes::FourCc> = None;
        for i in 0..num_tiles {
            let req = ext.request(id, i, cols, num_tiles)?;
            let context = |e: HeifError| -> HeifError {
                let msg = format!("tili item {id}: external tile {i} ({}): {e}", req.url);
                match e {
                    HeifError::Unsupported(_) => HeifError::unsupported(msg),
                    HeifError::LimitExceeded(_) => HeifError::exhausted(msg),
                    HeifError::Io(_) => HeifError::io(msg),
                    _ => HeifError::invalid(msg),
                }
            };
            let bytes = resolver.resolve(&req)?;
            let tile = HeifFile::from_vec(bytes).map_err(context)?;
            let tile_id = crate::tiled::check_external_tile(&tile, handler).map_err(context)?;
            let tnode = build_graph(&tile, tile_id).map_err(context)?;
            match coding {
                None => coding = Some(tnode.item.item_type),
                Some(t) if t != tnode.item.item_type => {
                    return Err(context(HeifError::invalid(format!(
                        "item type '{}' differs from tile 0's '{}' (the tiles share one coding format, §6.11.1)",
                        crate::boxes::fourcc_str(&tnode.item.item_type),
                        crate::boxes::fourcc_str(&t)
                    ))));
                }
                Some(_) => {}
            }
            let size = tnode.output_size().map_err(context)?;
            if size != (cfg.tile_width, cfg.tile_height) {
                return Err(context(HeifError::invalid(format!(
                    "a {}x{} tile for a tilC tile size of {}x{} (§6.11.3.3)",
                    size.0, size.1, cfg.tile_width, cfg.tile_height
                ))));
            }
            let plain = matches!(tnode.kind, ImageKind::Coded(_))
                && tnode.alpha.is_none()
                && tnode.properties.transformative().next().is_none()
                && tnode.properties.unsupported_essential().is_empty();
            if plain {
                jobs.push((i as usize, sub.prepare(&tile, &tnode).map_err(context)?));
            } else {
                let img = decode_item(&tile, tile_id, sub).map_err(context)?;
                canvas.place(i as usize, &img.frame)?;
            }
        }
        if !jobs.is_empty() {
            self.run_tile_jobs(jobs, &mut canvas)?;
        }
        canvas.finish_threads(self.decoder.threads)
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
        if cexg.extent_config.is_some()
            && !node
                .properties
                .entries
                .iter()
                .any(|e| e.property.is_decoder_config())
        {
            // `tile_config_info_present_flag`: the configuration to
            // decode an extent on its own rides in the property as an
            // ExtentDecoderConfigurationRecord, which Amd 1 §6.5.41.3
            // leaves to "the image coding format" and no coding-format
            // binding defines. The record is kept as opaque bytes
            // (`Cexg::extent_config`) and never interpreted: an item
            // that also carries its ordinary decoder configuration
            // property decodes with that one, an item without is
            // refused.
            return Err(HeifError::unsupported(format!(
                "item {id}: cexg carries its decoder configuration only as an ExtentDecoderConfigurationRecord, which no staged coding-format binding defines"
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
        use std::sync::{Mutex, mpsc};
        let workers =
            ExecutionContext::with_threads(self.decoder.threads).effective_workers(jobs.len());
        if workers == 1 {
            // One job at a time: the whole budget is the codec's.
            for (i, job) in jobs {
                let f = job.execute(&self.decoder, true)?.swap_remove(0).frame;
                canvas.place(i, &f)?;
            }
            return Ok(());
        }
        let queue = Mutex::new(jobs.into_iter());
        let abort = AtomicBool::new(false);
        let decoder = self.decoder;
        let turner = canvas.turner();
        let (tx, rx) = mpsc::sync_channel::<(usize, Result<crate::compose::CanvasTile>)>(workers);
        std::thread::scope(|scope| -> Result<()> {
            for _ in 0..workers {
                let tx = tx.clone();
                let (queue, abort) = (&queue, &abort);
                scope.spawn(move || {
                    loop {
                        if abort.load(Ordering::Relaxed) {
                            break;
                        }
                        let next = queue.lock().map(|mut q| q.next()).unwrap_or(None);
                        let Some((i, job)) = next else {
                            break;
                        };
                        // The worker turns its tile to the canvas
                        // orientation, so the placement on the
                        // collecting thread stays a copy of rows.
                        let r = job
                            .execute(&decoder, false)
                            .map(|mut f| turner.turn(f.swap_remove(0).frame));
                        if tx.send((i, r)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);
            let mut result = Ok(());
            for (i, r) in rx.iter() {
                if result.is_err() {
                    continue;
                }
                match r.and_then(|f| canvas.place_tile(i, &f)) {
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
        // The rotations and mirrors that lead the chain of a grid or a
        // tiled item are the canvas's: the tiles go straight to their
        // oriented places.
        let ops: Vec<crate::compose::Reorient> = match &node.kind {
            ImageKind::Grid(_) | ImageKind::Tiled(_) => node
                .properties
                .transformative()
                .map_while(|e| crate::compose::Reorient::of(&e.property))
                .collect(),
            _ => Vec::new(),
        };
        let rec = match &node.kind {
            ImageKind::Grid(g) if !ops.is_empty() => self.reconstruct_grid(node, g, &ops)?,
            ImageKind::Tiled(t) if !ops.is_empty() => self.reconstruct_tiled(node, t, &ops)?,
            _ => self.reconstruct(node)?,
        };
        let mut out = crate::compose::apply_transforms_owned_threads(
            rec,
            node.properties.transformative().skip(ops.len()),
            false,
            self.decoder.threads,
        )?;
        if let Some(a) = &node.alpha {
            let mut alpha = self.output(a)?;
            if let Some(info) = a.properties.alpha_information() {
                crate::compose::normalise_alpha(&mut alpha, &info);
            }
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
    if let Some((_, _, inputs)) = gain_map_target(file, &node)?
        && let Some(g) = inputs.get(1)
        && let Ok(gain_node) = build_graph(file, *g)
    {
        count_coded_uses(&gain_node, &mut uses);
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
    let (exif, xmp) = metadata_payloads(file, &node)?;
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

pub use crate::meta::inflate_metadata;

pub use crate::meta::exif_payload;

/// The Exif payload (from the TIFF header on) and the XMP packet of the
/// metadata items describing `node` (`cdsc`): the first `Exif` /
/// `dExf` item and the first XMP `mime` item, deflated ones inflated
/// (Amd 2:2026 A.2.1 / O.4.3). What [`decode_item`] puts on the
/// [`DecodedImage`] and the framework demuxer on its stream's blobs.
pub(crate) fn metadata_payloads<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    node: &ImageNode,
) -> Result<(Option<Vec<u8>>, Option<String>)> {
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
    Ok((exif, xmp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Chroma;
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
