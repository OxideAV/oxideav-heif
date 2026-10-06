//! Framework integration (`registry` feature): the HEIF container as an
//! [`oxideav_core::Demuxer`] and the `"heif"` still-image codec.
//!
//! A `.heic` / `.heif` / `.avif` file opens as a demuxer with:
//!
//! * **stream 0 — the still images** (when the file-level `meta` box has
//!   a `pict` handler and a primary item): codec id `"heif"`, one
//!   keyframe packet per displayable image item — the primary first,
//!   then the rest of a burst in [`crate::decode_all`]'s order — each
//!   carrying the whole file with its `pitm` naming that item. The
//!   `"heif"` decoder ([`HeifCodec`], registered by
//!   [`crate::registry::register`]) reconstructs the item —
//!   derivations, transformative properties, alpha — and emits one
//!   `VideoFrame` per packet. Item packets are untimed: `pts` is the
//!   item index in a 1/1 time base, duration 1. The stream's
//!   [`CodecParameters`] announce the predicted output geometry and
//!   pixel format so pipelines can allocate before decoding; items
//!   whose output differs from the primary's are not part of it
//!   (`metadata` key `stream:<n>:item_ids` lists the carried items).
//!   Alpha / depth auxiliaries, thumbnails, hidden items and the gain
//!   map are composed into or attached to their master, never frames
//!   of their own.
//! * **one stream per visual track** (`pict` / `vide` / `auxv`
//!   handlers of the `moov` box, image sequences): codec id resolved
//!   from the sample-entry type (`hvc1` → `"h265"`, `av01` → `"av1"`,
//!   `vvc1` → `"h266"`, …) through the [`CodecResolver`], `extradata`
//!   = the `hvcC` / `av1C` / `avcC` / `vvcC` record, packets = the samples in decode order with `pts`
//!   / `dts` / `duration` in the media time base and the `stss` sync
//!   flags; `seek_to` lands on sync samples.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, CodecTag, Decoder, Error as CoreError, Frame, Packet,
    PixelFormat, ProbeContext, ProbeData, ReadSeek, Result as CoreResult, StreamInfo, TimeBase,
};

use crate::decode;
use crate::derived::{build_graph, build_primary_graph, ImageKind, ImageNode};
use crate::error::{HeifError, Result};
use crate::file::HeifFile;
use crate::image::{Chroma, HeifPixelFormat};
use crate::props::Property;
use crate::sequence::{parse_movie, sample_bytes, Movie, Track};

/// Container name the demuxer is registered under.
pub const CONTAINER_NAME: &str = "heif";
/// Codec id of the still-image decoder.
pub const CODEC_ID: &str = "heif";
/// Maximum file size the demuxer reads into memory.
pub const MAX_FILE_BYTES: u64 = 1 << 32;

pub use crate::layout::predict_output;

#[doc(hidden)]
/// The framework pixel format the `"heif"` decoder will emit for a
/// predicted layout (applies the same 4:4:4 promotion as
/// [`crate::HeifFrame::to_core`]).
pub fn core_pixel_format(fmt: HeifPixelFormat, full_range: bool) -> Option<PixelFormat> {
    fmt.to_core()
        .or_else(|| {
            HeifPixelFormat {
                chroma: Chroma::Yuv444,
                ..fmt
            }
            .to_core()
        })
        .map(|pf| {
            if full_range {
                crate::image::core_bridge::full_range_variant(pf)
            } else {
                pf
            }
        })
}

#[doc(hidden)]
/// [`core_pixel_format`] for an item whose effective colour information
/// is `colr`: a 4:4:4 layout with `matrix_coefficients = 0` (H.273
/// identity) is labelled planar RGB (`Gbrp*` / `Gbrap*`), as
/// [`crate::HeifFrame::to_core_signalled`] emits it.
pub fn core_pixel_format_for(
    fmt: HeifPixelFormat,
    colr: &crate::props::Colr,
) -> Option<PixelFormat> {
    if crate::image::core_bridge::identity_matrix(colr) {
        if let Some(gbr) = fmt.to_core_gbr() {
            return Some(gbr);
        }
    }
    let full = match colr {
        crate::props::Colr::Nclx { full_range, .. } => *full_range,
        _ => true,
    };
    core_pixel_format(fmt, full)
}

/// The `ster` stereo pair (`left`, `right`) an image item belongs to
/// (HEIF §6.8.5), when any.
fn stereo_pair_of<D: AsRef<[u8]>>(file: &HeifFile<D>, item_id: u32) -> Option<(u32, u32)> {
    let meta = file.meta.as_ref()?;
    meta.groups_containing(item_id, b"ster")
        .into_iter()
        .chain(meta.groups_containing(item_id, b"stem"))
        .find_map(|g| g.stereo_pair())
        .filter(|(l, r)| l != r)
}

/// The layer an image item selects (`lsel`), for tagging a view.
fn view_layer_id<D: AsRef<[u8]>>(file: &HeifFile<D>, item_id: u32) -> Option<u16> {
    let meta = file.meta.as_ref()?;
    crate::props::ItemProperties::resolve(meta, item_id)
        .ok()?
        .lsel()
        .map(|l| l.layer_id)
}

/// The stream-level layer list of a still: the output layers of a
/// layered primary without `lsel`, or the two views of a `ster` pair.
fn still_layers<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    node: &ImageNode,
) -> Vec<oxideav_core::LayerInfo> {
    use oxideav_core::LayerInfo;
    if node.item.item_type == crate::meta::ITEM_TYPE_LHV1 && node.properties.lsel().is_none() {
        if let Some(o) = node.properties.oinf() {
            let mut out = o.output_layers(node.properties.tols().unwrap_or(0));
            out.sort_unstable();
            out.dedup();
            if out.len() > 1 {
                return out
                    .into_iter()
                    .map(|l| {
                        let mut info = LayerInfo::new(l as u16);
                        if l != 0 {
                            info = info.with_depends_on(vec![0u16]);
                        }
                        info
                    })
                    .collect();
            }
        }
    }
    if let Some((left, right)) = stereo_pair_of(file, node.item.id) {
        return [(0u16, left), (1u16, right)]
            .into_iter()
            .map(|(view, id)| {
                LayerInfo::new(view_layer_id(file, id).unwrap_or(view)).with_view_id(view)
            })
            .collect();
    }
    Vec::new()
}

/// The effective colour information of an item (its `nclx`, else the
/// MIAF §7.3.6.4 default).
fn item_colour(node: &ImageNode) -> crate::props::Colr {
    node.properties
        .nclx()
        .cloned()
        .unwrap_or(crate::props::Colr::MIAF_DEFAULT)
}

/// Codec id for a sample-entry type, through the resolver first and
/// by the well-known types otherwise.
fn codec_id_for_entry(entry_type: &[u8; 4], codecs: &dyn CodecResolver) -> Option<CodecId> {
    let tag = CodecTag::fourcc(entry_type);
    if let Some(id) = codecs.resolve_tag(&ProbeContext::new(&tag)) {
        return Some(id);
    }
    match entry_type {
        b"hvc1" | b"hev1" | b"hvc2" | b"hev2" | b"lhv1" | b"lhe1" => {
            Some(CodecId::new(decode::CODEC_ID_HEVC))
        }
        b"av01" => Some(CodecId::new(decode::CODEC_ID_AV1)),
        b"avc1" | b"avc3" => Some(CodecId::new(decode::CODEC_ID_AVC)),
        b"vvc1" | b"vvi1" => Some(CodecId::new(decode::CODEC_ID_VVC)),
        _ => None,
    }
}

struct TrackStream {
    track_index: usize,
    cursor: usize,
    time_base: TimeBase,
    /// Alpha auxiliary track (HEIF §7.5.3) composed into this stream:
    /// each packet is a synthesized single-image HEIF file (master
    /// sample + time-parallel alpha sample) for the `"heif"` codec.
    alpha_track_index: Option<usize>,
}

/// Sample layout a decoder-configuration property announces.
fn config_layout(config: &Property) -> Option<HeifPixelFormat> {
    match config {
        Property::HvcC(h) => decode::hevc_layout(h).ok(),
        Property::Av1C(a) => decode::av1_layout(a).ok(),
        Property::AvcC(a) => a.layout().ok(),
        _ => None,
    }
}

/// A visual sample entry's coded-item description for a synthesized
/// still: item type + decoder configuration + descriptive properties.
fn entry_item(
    entry: &crate::sequence::SampleEntry,
) -> Option<(crate::boxes::FourCc, Property, Vec<Property>)> {
    let (item_type, config) = match &entry.entry_type {
        b"hvc1" | b"hev1" => (*b"hvc1", Property::HvcC(entry.hvcc.clone()?)),
        b"av01" => (*b"av01", Property::Av1C(entry.av1c.clone()?)),
        b"avc1" | b"avc3" => (*b"avc1", Property::AvcC(entry.avcc.clone()?)),
        b"vvc1" | b"vvi1" => (*b"vvc1", Property::VvcC(entry.vvcc.clone()?)),
        _ => return None,
    };
    let mut extra = Vec::new();
    if let Some(c) = entry.colr.first() {
        extra.push(Property::Colr(c.clone()));
    }
    if let Some(c) = entry.clap {
        extra.push(Property::Clap(c));
    }
    if let Some(p) = entry.pasp {
        extra.push(Property::Pasp(p));
    }
    Some((item_type, config, extra))
}

/// One `"heif"` still file holding `master` (with its alpha auxiliary
/// when `alpha` is given), built from the tracks' sample entries: the
/// still-image alpha rules (`auxl` + `auxC`, resize / depth match at
/// composition) then apply unchanged.
#[doc(hidden)]
pub fn synthesize_still(
    master: (&Track, &[u8]),
    alpha: Option<(&Track, &[u8])>,
) -> Result<Vec<u8>> {
    let (mt, mdata) = master;
    let me = mt
        .primary_entry()
        .ok_or_else(|| HeifError::invalid("track without a sample entry"))?;
    let (item_type, config, extra) = entry_item(me)
        .ok_or_else(|| HeifError::unsupported("sample entry without a decoder configuration"))?;
    let layout =
        config_layout(&config).ok_or_else(|| HeifError::unsupported("sample entry layout"))?;
    let mut w = crate::writer::HeifWriter::new();
    let mut props = vec![
        (config, true),
        (
            Property::Ispe(crate::props::Ispe {
                width: me.width as u32,
                height: me.height as u32,
            }),
            false,
        ),
        (
            Property::Pixi(crate::props::Pixi {
                bits_per_channel: vec![layout.bit_depth; layout.chroma.colour_planes()],
            }),
            false,
        ),
    ];
    if !extra.iter().any(|p| matches!(p, Property::Colr(_))) {
        props.push((Property::Colr(crate::props::Colr::MIAF_DEFAULT), false));
    }
    for p in extra {
        let essential = matches!(p, Property::Clap(_));
        props.push((p, essential));
    }
    let id = w.add_coded_item(item_type, mdata.to_vec(), props);
    if let Some((at, adata)) = alpha {
        let ae = at
            .primary_entry()
            .ok_or_else(|| HeifError::invalid("alpha track without a sample entry"))?;
        let (a_type, a_config, a_extra) = entry_item(ae).ok_or_else(|| {
            HeifError::unsupported("alpha sample entry without a decoder configuration")
        })?;
        let a_layout = config_layout(&a_config)
            .ok_or_else(|| HeifError::unsupported("alpha sample entry layout"))?;
        let mut aprops = vec![
            (a_config, true),
            (
                Property::Ispe(crate::props::Ispe {
                    width: ae.width as u32,
                    height: ae.height as u32,
                }),
                false,
            ),
            (
                Property::Pixi(crate::props::Pixi {
                    bits_per_channel: vec![a_layout.bit_depth],
                }),
                false,
            ),
            (
                Property::AuxC(crate::props::AuxC {
                    aux_type: ae
                        .aux_track_type
                        .clone()
                        .unwrap_or_else(|| crate::props::AUX_URN_ALPHA.to_string()),
                    aux_subtype: Vec::new(),
                }),
                true,
            ),
        ];
        for p in a_extra {
            if let Property::Clap(_) = p {
                aprops.push((p, true));
            }
        }
        w.add_alpha(id, a_type, adata.to_vec(), aprops, false);
    }
    w.set_primary(id);
    w.write_to_vec()
}

/// The HEIF demuxer.
pub struct HeifDemuxer {
    file: HeifFile,
    movie: Option<Movie>,
    streams: Vec<StreamInfo>,
    /// The pictures of the still stream, one packet each: the primary
    /// item first, then the other displayable image items (a burst) in
    /// [`crate::decode_all`]'s order.
    still_items: Vec<u32>,
    /// Next still packet to emit (an index into `still_items`).
    still_cursor: usize,
    /// Absolute offset and width (2 / 4 bytes) of the `pitm` item id
    /// field, patched to name the item a burst packet carries.
    pitm_field: Option<(usize, usize)>,
    still_stream: Option<u32>,
    tracks: Vec<(u32, TrackStream)>,
    active: Option<Vec<u32>>,
    metadata: Vec<(String, String)>,
}

/// The displayable image items of `file` in [`crate::decode_all`]'s
/// order — the primary first, then every other entry of the viewer's
/// display order (MIAF Annex A: non-hidden masters, thumbnails and
/// auxiliaries excluded, an `altr` group counting once) by ascending
/// first id — restricted to the items whose predicted output (layout
/// and geometry) equals the primary's, so one stream description fits
/// every packet. An `altr` group contributes its first member whose
/// derivation graph builds.
fn still_item_order<D: AsRef<[u8]>>(file: &HeifFile<D>, primary: &ImageNode) -> Vec<u32> {
    let pid = primary.item.id;
    let mut out = vec![pid];
    let Ok(meta) = file.meta() else {
        return out;
    };
    let Ok(want) = predict_output(primary) else {
        return out;
    };
    let mut entries = meta.display_order();
    entries.sort_by_key(|e| (!e.contains(&pid), e.first().copied().unwrap_or(u32::MAX)));
    for alternatives in entries {
        if alternatives.contains(&pid) {
            continue;
        }
        let pick = alternatives.iter().copied().find_map(|id| {
            let node = build_graph(file, id).ok()?;
            (predict_output(&node).ok()? == want).then_some(id)
        });
        if let Some(id) = pick {
            out.push(id);
        }
    }
    out
}

/// Absolute offset and byte width of the `pitm` item id inside the
/// file bytes every reader works on, when the `meta` box carries one.
fn pitm_field_of<D: AsRef<[u8]>>(file: &HeifFile<D>) -> Option<(usize, usize)> {
    use crate::boxes::{iter_boxes, parse_full_box, payload};
    let bytes = file.bytes();
    let meta = file.top_level.iter().find(|h| &h.box_type == b"meta")?;
    let meta_payload = payload(bytes, meta);
    let (_, _, inner) = parse_full_box(meta_payload).ok()?;
    let inner_start = meta.payload_start + (meta_payload.len() - inner.len());
    for h in iter_boxes(inner) {
        let h = h.ok()?;
        if &h.box_type != b"pitm" {
            continue;
        }
        let p = payload(inner, &h);
        let (version, _, body) = parse_full_box(p).ok()?;
        let width = match version {
            0 => 2,
            1 => 4,
            _ => return None,
        };
        if body.len() < width {
            return None;
        }
        return Some((
            inner_start + h.payload_start + (p.len() - body.len()),
            width,
        ));
    }
    None
}

impl HeifDemuxer {
    /// Open a demuxer over a file held in memory.
    pub fn from_bytes(bytes: Vec<u8>, codecs: &dyn CodecResolver) -> Result<Self> {
        let file = HeifFile::from_vec(bytes)?;
        let movie = parse_movie(&file)?;
        let mut streams = Vec::new();
        let mut still_stream = None;
        let mut still_items: Vec<u32> = Vec::new();
        let mut pitm_field: Option<(usize, usize)> = None;
        let mut metadata = vec![
            (
                "major_brand".to_string(),
                crate::boxes::fourcc_str(&file.file_type.major_brand),
            ),
            (
                "compatible_brands".to_string(),
                file.file_type
                    .compatible_brands
                    .iter()
                    .map(crate::boxes::fourcc_str)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        ];
        // Still image stream.
        let has_pict_meta = file
            .meta
            .as_ref()
            .and_then(|m| m.handler.as_ref())
            .map(|h| &h.handler_type == b"pict")
            .unwrap_or(false);
        if has_pict_meta {
            if let Ok(node) = build_primary_graph(&file) {
                let mut params = CodecParameters::video(CodecId::new(CODEC_ID));
                // A tmap decodes to its base rendition by default, with
                // the base's colour information (decode::ToneMapOutput).
                let colour_node = match (&node.kind, node.inputs.first()) {
                    (ImageKind::ToneMap(_), Some(base)) => base,
                    _ => &node,
                };
                let colour = item_colour(colour_node);
                params =
                    params.with_color_signal(crate::image::core_bridge::color_signal_of(&colour));
                match predict_output(&node) {
                    Ok((fmt, (w, h))) => {
                        params.width = Some(w);
                        params.height = Some(h);
                        params.pixel_format = core_pixel_format_for(fmt, &colour);
                    }
                    Err(_) => {
                        if let Ok((w, h)) = node.output_size() {
                            params.width = Some(w);
                            params.height = Some(h);
                        }
                    }
                }
                let layers = still_layers(&file, &node);
                if !layers.is_empty() {
                    params = params.with_layers(layers);
                }
                metadata.push(("primary_item_id".into(), node.item.id.to_string()));
                metadata.push((
                    "primary_item_type".into(),
                    crate::boxes::fourcc_str(&node.item.item_type),
                ));
                // Every displayable item is a packet; the ones after the
                // primary need the `pitm` patched to name them, so
                // without a patchable `pitm` (or an id too wide for
                // it) the stream is the primary alone.
                pitm_field = pitm_field_of(&file);
                still_items = still_item_order(&file, &node);
                match pitm_field {
                    Some((_, width)) => still_items.retain(|&id| {
                        id == node.item.id || width == 4 || u16::try_from(id).is_ok()
                    }),
                    None => still_items.truncate(1),
                }
                let index = streams.len() as u32;
                metadata.push((
                    format!("stream:{index}:item_ids"),
                    still_items
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                ));
                still_stream = Some(index);
                streams.push(StreamInfo {
                    index,
                    time_base: TimeBase::new(1, 1),
                    duration: Some(still_items.len() as i64),
                    start_time: Some(0),
                    params,
                });
            }
        }
        // Composed streams: a master track with an alpha auxiliary
        // track yields a `"heif"` stream whose packets are synthesized
        // stills (master + time-parallel alpha) — frames with alpha
        // through the ordinary decoder path. The raw tracks follow.
        let mut tracks = Vec::new();
        if let Some(mv) = &movie {
            for (ti, t) in mv.tracks.iter().enumerate() {
                if !t.is_visual() || &t.handler == b"auxv" {
                    continue;
                }
                let Some(alpha) = mv.alpha_track_of(t.track_id) else {
                    continue;
                };
                let Some(ai) = mv.tracks.iter().position(|x| x.track_id == alpha.track_id) else {
                    continue;
                };
                let Some(entry) = t.primary_entry() else {
                    continue;
                };
                let Some(layout) = entry_item(entry).and_then(|(_, c, _)| config_layout(&c)) else {
                    continue;
                };
                let colour = entry
                    .colr
                    .iter()
                    .find(|c| matches!(c, crate::props::Colr::Nclx { .. }))
                    .cloned()
                    .unwrap_or(crate::props::Colr::MIAF_DEFAULT);
                let mut params = CodecParameters::video(CodecId::new(CODEC_ID))
                    .with_color_signal(crate::image::core_bridge::color_signal_of(&colour));
                params.width = Some(entry.width as u32);
                params.height = Some(entry.height as u32);
                params.pixel_format = core_pixel_format_for(layout.with_alpha(), &colour);
                let time_base = TimeBase::new(1, t.timescale.max(1) as i64);
                let index = streams.len() as u32;
                streams.push(StreamInfo {
                    index,
                    time_base,
                    duration: Some(t.duration as i64),
                    start_time: t.samples.first().map(|s| s.pts() as i64),
                    params,
                });
                tracks.push((
                    index,
                    TrackStream {
                        track_index: ti,
                        cursor: 0,
                        time_base,
                        alpha_track_index: Some(ai),
                    },
                ));
                metadata.push((
                    format!("track:{}:alpha_track", t.track_id),
                    alpha.track_id.to_string(),
                ));
                metadata.push((
                    format!("stream:{index}:composed_from_track"),
                    t.track_id.to_string(),
                ));
            }
            for (ti, t) in mv.tracks.iter().enumerate() {
                if !t.is_visual() {
                    continue;
                }
                let Some(entry) = t.primary_entry() else {
                    continue;
                };
                let Some(id) = codec_id_for_entry(&entry.entry_type, codecs) else {
                    continue;
                };
                let mut params = CodecParameters::video(id);
                params.width = Some(entry.width as u32);
                params.height = Some(entry.height as u32);
                if let Some(h) = &entry.hvcc {
                    params.extradata = h.raw.clone();
                    params.pixel_format = decode::hevc_layout(h).ok().and_then(|f| f.to_core());
                } else if let Some(a) = &entry.av1c {
                    params.extradata = a.raw.clone();
                    params.pixel_format = decode::av1_layout(a).ok().and_then(|f| f.to_core());
                } else if let Some(a) = &entry.avcc {
                    params.extradata = a.raw.clone();
                    params.pixel_format = a.layout().ok().and_then(|f| f.to_core());
                } else if let Some(l) = &entry.lhvc {
                    params.extradata = l.raw.clone();
                } else if let Some(v) = &entry.vvcc {
                    params.extradata = v.to_bytes();
                    params.pixel_format = decode::vvc_layout(v).ok().and_then(|f| f.to_core());
                }
                if let Some(c) = entry
                    .colr
                    .iter()
                    .find(|c| matches!(c, crate::props::Colr::Nclx { .. }))
                {
                    params =
                        params.with_color_signal(crate::image::core_bridge::color_signal_of(c));
                }
                params = params.with_tag(CodecTag::fourcc(&entry.entry_type));
                let time_base = TimeBase::new(1, t.timescale.max(1) as i64);
                let index = streams.len() as u32;
                streams.push(StreamInfo {
                    index,
                    time_base,
                    duration: Some(t.duration as i64),
                    start_time: t.samples.first().map(|s| s.pts() as i64),
                    params,
                });
                tracks.push((
                    index,
                    TrackStream {
                        track_index: ti,
                        cursor: 0,
                        time_base,
                        alpha_track_index: None,
                    },
                ));
                metadata.push((
                    format!("track:{}:handler", t.track_id),
                    crate::boxes::fourcc_str(&t.handler),
                ));
                if let Some(a) = &entry.aux_track_type {
                    metadata.push((format!("track:{}:aux_track_type", t.track_id), a.clone()));
                }
                for (rt, ids) in &t.references {
                    metadata.push((
                        format!("track:{}:tref:{}", t.track_id, crate::boxes::fourcc_str(rt)),
                        ids.iter().map(u32::to_string).collect::<Vec<_>>().join(","),
                    ));
                }
            }
        }
        if streams.is_empty() {
            return Err(HeifError::unsupported(
                "file carries neither a pict meta box with a primary item nor a visual track",
            ));
        }
        Ok(Self {
            file,
            movie,
            streams,
            still_items,
            still_cursor: 0,
            pitm_field,
            still_stream,
            tracks,
            active: None,
            metadata,
        })
    }

    /// The image items the still stream carries, one packet each, in
    /// packet order (the primary first; see [`crate::decode_all`]).
    pub fn still_items(&self) -> &[u32] {
        &self.still_items
    }

    /// Packet `index` of the still stream: the whole file, with the
    /// `pitm` naming the picture's item so the `"heif"` decoder
    /// reconstructs that item (its own derivations, transforms and
    /// auxiliaries) exactly as the primary. Untimed: `pts` is the
    /// index in a 1/1 time base, duration 1.
    fn still_packet(&self, stream: u32, index: usize) -> CoreResult<Packet> {
        let id = self.still_items[index];
        let mut data = self.file.bytes().to_vec();
        let primary = self.file.primary_item().map(|i| i.id).ok();
        if Some(id) != primary {
            let (off, width) = self
                .pitm_field
                .ok_or_else(|| CoreError::invalid("heif: burst item without a pitm to patch"))?;
            match width {
                2 => data[off..off + 2].copy_from_slice(&(id as u16).to_be_bytes()),
                _ => data[off..off + 4].copy_from_slice(&id.to_be_bytes()),
            }
        }
        Ok(Packet::new(stream, TimeBase::new(1, 1), data)
            .with_pts(index as i64)
            .with_dts(index as i64)
            .with_duration(1)
            .with_keyframe(true))
    }

    /// The parsed file.
    pub fn file(&self) -> &HeifFile {
        &self.file
    }

    /// The parsed movie box, when present.
    pub fn movie(&self) -> Option<&Movie> {
        self.movie.as_ref()
    }

    fn track(&self, ts: &TrackStream) -> &Track {
        &self
            .movie
            .as_ref()
            .expect("track stream implies movie")
            .tracks[ts.track_index]
    }

    fn is_active(&self, stream: u32) -> bool {
        self.active
            .as_ref()
            .map(|a| a.contains(&stream))
            .unwrap_or(true)
    }
}

impl oxideav_core::Demuxer for HeifDemuxer {
    fn format_name(&self) -> &str {
        CONTAINER_NAME
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> CoreResult<Packet> {
        if let Some(idx) = self.still_stream {
            if self.still_cursor < self.still_items.len() {
                if self.is_active(idx) {
                    let i = self.still_cursor;
                    self.still_cursor += 1;
                    return self.still_packet(idx, i);
                }
                // An inactive still stream is skipped altogether.
                self.still_cursor = self.still_items.len();
            }
        }
        // Pick the active track whose next sample has the earliest
        // decode time (in seconds).
        let mut best: Option<(usize, f64)> = None;
        for (i, (idx, ts)) in self.tracks.iter().enumerate() {
            if !self.is_active(*idx) {
                continue;
            }
            let t = self.track(ts);
            if let Some(s) = t.samples.get(ts.cursor) {
                let secs = s.dts as f64 / t.timescale.max(1) as f64;
                if best.map(|(_, b)| secs < b).unwrap_or(true) {
                    best = Some((i, secs));
                }
            }
        }
        let Some((i, _)) = best else {
            return Err(CoreError::Eof);
        };
        let (idx, ts) = &self.tracks[i];
        let idx = *idx;
        let t = self.track(ts);
        let s = t.samples[ts.cursor];
        let data = match ts.alpha_track_index {
            None => sample_bytes(&self.file, &s)?.to_vec(),
            Some(ai) => {
                let at = &self
                    .movie
                    .as_ref()
                    .expect("track stream implies movie")
                    .tracks[ai];
                let alpha = at
                    .sample_index_at(s.dts, t.timescale)
                    .and_then(|i| at.samples.get(i))
                    .map(|a| sample_bytes(&self.file, a))
                    .transpose()?
                    .map(|bytes| (at, bytes));
                synthesize_still((t, sample_bytes(&self.file, &s)?), alpha)?
            }
        };
        let tb = ts.time_base;
        self.tracks[i].1.cursor += 1;
        Ok(Packet::new(idx, tb, data)
            .with_pts(s.pts() as i64)
            .with_dts(s.dts as i64)
            .with_duration(s.duration as i64)
            .with_keyframe(s.is_sync))
    }

    fn set_active_streams(&mut self, indices: &[u32]) {
        self.active = Some(indices.to_vec());
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> CoreResult<i64> {
        if Some(stream_index) == self.still_stream {
            // Still packets are the item index (1/1 time base).
            let last = self.still_items.len().saturating_sub(1);
            self.still_cursor = usize::try_from(pts.max(0)).unwrap_or(last).min(last);
            return Ok(self.still_cursor as i64);
        }
        let Some(pos) = self.tracks.iter().position(|(i, _)| *i == stream_index) else {
            return Err(CoreError::invalid(format!("no stream {stream_index}")));
        };
        let t = self.track(&self.tracks[pos].1);
        let target = pts.max(0) as u64;
        let mut chosen = 0usize;
        for (i, s) in t.samples.iter().enumerate() {
            if s.is_sync && s.pts() <= target {
                chosen = i;
            }
        }
        let landed = t.samples.get(chosen).map(|s| s.pts() as i64).unwrap_or(0);
        self.tracks[pos].1.cursor = chosen;
        Ok(landed)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        let mv = self.movie.as_ref()?;
        if mv.timescale == 0 {
            return None;
        }
        Some((mv.duration as i128 * 1_000_000 / mv.timescale as i128) as i64)
    }
}

/// Container probe: HEIF-family brands in an `ftyp` at offset 0.
pub fn probe(p: &ProbeData) -> u8 {
    crate::ftyp::probe_score(p.buf)
}

/// [`oxideav_core::OpenDemuxerFn`] for the registry.
pub fn open(
    mut input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
) -> CoreResult<Box<dyn oxideav_core::Demuxer>> {
    let len = input.seek(SeekFrom::End(0))?;
    if len > MAX_FILE_BYTES {
        return Err(CoreError::resource_exhausted(format!(
            "heif: {len}-byte input exceeds the {MAX_FILE_BYTES}-byte in-memory limit"
        )));
    }
    input.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(len as usize);
    input.read_to_end(&mut bytes)?;
    Ok(Box::new(HeifDemuxer::from_bytes(bytes, codecs)?))
}

/// The `"heif"` still-image decoder: one packet = one file, one frame
/// = the primary item's output image.
pub struct HeifCodec {
    codec_id: CodecId,
    queue: VecDeque<Frame>,
    flushed: bool,
    /// The most recently decoded image's descriptive side (colour,
    /// metadata) for callers holding the concrete type.
    last: Option<decode::DecodedImage>,
    /// Thread budget from `set_execution_context` (serial by default).
    exec: oxideav_core::ExecutionContext,
}

impl HeifCodec {
    /// Construct with the id the decoder reports.
    pub fn new(codec_id: CodecId) -> Self {
        Self {
            codec_id,
            queue: VecDeque::new(),
            flushed: false,
            last: None,
            exec: oxideav_core::ExecutionContext::serial(),
        }
    }

    /// The last image decoded by [`Decoder::send_packet`].
    pub fn last_image(&self) -> Option<&decode::DecodedImage> {
        self.last.as_ref()
    }

    /// The contract options this decoder runs with: the thread budget
    /// of `set_execution_context`, everything else default.
    fn decode_options(&self) -> crate::api::DecodeOptions {
        crate::api::DecodeOptions::default()
            .with_threads((self.exec.threads > 1).then_some(self.exec.threads))
    }
}

impl Decoder for HeifCodec {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        let file = HeifFile::parse_borrowed(&packet.data)?;
        // The contract path: one implementation (limits, strictness and
        // the thread budget through DecodeOptions).
        let opts = self.decode_options();
        let img = crate::api_registry::decode_file_item(&file, &opts)?;
        // The colour information rides on the frame (H.273 code points +
        // range; identity-matrix items as planar RGB), the way
        // HeifImage::into_video_frame labels it.
        let signal_frame = |f: &crate::HeifFrame, nclx: &crate::props::Colr| {
            let image = crate::api::HeifImage::from_frame(
                f.clone(),
                crate::api::ColorInfo::from_colr(nclx),
                crate::api::Metadata::default(),
            );
            let (mut vf, _pf) = image.into_video_frame();
            vf.pts = packet.pts;
            Ok::<_, CoreError>(vf)
        };
        if !img.layers.is_empty() {
            // A layered item without lsel: every output layer, tagged.
            for l in &img.layers {
                let vf = signal_frame(&l.frame, &img.nclx)?
                    .with_layer(oxideav_core::LayerIdentity::new(l.layer_id as u16));
                self.queue.push_back(Frame::Video(vf));
            }
        } else if let Some((left, right)) = stereo_pair_of(&file, img.item_id) {
            // HEIF §6.8.5 'ster': the primary is one view of a stereo
            // pair — both views out, tagged view 0 (left) / 1 (right).
            let other = if img.item_id == left { right } else { left };
            let partner = crate::api_registry::decode_file_item(
                &file,
                &opts.clone().with_item_id(Some(other)),
            )?;
            let (first, second) = if img.item_id == left {
                (&img, &partner)
            } else {
                (&partner, &img)
            };
            for (view, image) in [(0u16, first), (1u16, second)] {
                let layer = view_layer_id(&file, image.item_id).unwrap_or(view);
                let vf = signal_frame(&image.frame, &image.nclx)?
                    .with_layer(oxideav_core::LayerIdentity::new(layer).with_view_id(view));
                self.queue.push_back(Frame::Video(vf));
            }
        } else {
            let vf = signal_frame(&img.frame, &img.nclx)?;
            self.queue.push_back(Frame::Video(vf));
        }
        self.last = Some(img);
        Ok(())
    }

    fn receive_frame(&mut self) -> CoreResult<Frame> {
        match self.queue.pop_front() {
            Some(f) => Ok(f),
            None if self.flushed => Err(CoreError::Eof),
            None => Err(CoreError::NeedMore),
        }
    }

    fn flush(&mut self) -> CoreResult<()> {
        self.flushed = true;
        Ok(())
    }

    fn reset(&mut self) -> CoreResult<()> {
        self.queue.clear();
        self.flushed = false;
        self.last = None;
        Ok(())
    }

    fn set_execution_context(&mut self, ctx: &oxideav_core::ExecutionContext) {
        // Grid tiles decode in parallel under this budget (byte-identical
        // output for every budget).
        self.exec = ctx.clone();
    }
}

/// Direct factory endpoint for the `"heif"` decoder.
pub fn make_decoder(params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
    Ok(Box::new(HeifCodec::new(params.codec_id.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_pixel_format_promotes_unmapped_layouts() {
        let f = HeifPixelFormat::new(Chroma::Mono, 10, true).unwrap();
        assert_eq!(core_pixel_format(f, true), Some(PixelFormat::Yuva444P10Le));
        let g = HeifPixelFormat::new(Chroma::Yuv420, 8, false).unwrap();
        assert_eq!(core_pixel_format(g, false), Some(PixelFormat::Yuv420P));
        assert_eq!(core_pixel_format(g, true), Some(PixelFormat::YuvJ420P));
    }

    #[test]
    fn decoder_lifecycle_without_input() {
        let mut d = HeifCodec::new(CodecId::new(CODEC_ID));
        assert!(matches!(d.receive_frame(), Err(CoreError::NeedMore)));
        d.flush().unwrap();
        assert!(matches!(d.receive_frame(), Err(CoreError::Eof)));
        let bad = Packet::new(0, TimeBase::new(1, 1), vec![0, 0, 0, 0]);
        assert!(d.send_packet(&bad).is_err());
        d.reset().unwrap();
        assert!(matches!(d.receive_frame(), Err(CoreError::NeedMore)));
    }
}
