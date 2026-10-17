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
//!   whose predicted output (layout, geometry, colour description)
//!   differs from the primary's form **further still streams**, one
//!   per distinct output in first-appearance order (as a multi-part
//!   EXR declares one stream per part layout), so no displayable
//!   image is lost (`metadata` key `stream:<n>:item_ids` lists each
//!   stream's items). Alpha / depth auxiliaries, thumbnails, hidden
//!   items and the gain map are composed into or attached to their
//!   master, never frames of their own.
//! * **one stream per visual track** (`pict` / `vide` / `auxv`
//!   handlers of the `moov` box, image sequences), after the still
//!   streams — the workspace convention: the composed primary still is
//!   video stream 0 (what the gateway's `primary()` / `max_frames(1)`
//!   rely on), the sequence tracks follow it, whatever the major
//!   brand: codec id resolved
//!   from the sample-entry type (`hvc1` → `"h265"`, `av01` → `"av1"`,
//!   `vvc1` → `"h266"`, …) through the [`CodecResolver`], `extradata`
//!   = the `hvcC` / `av1C` / `avcC` / `vvcC` record, packets = the samples in decode order with `pts`
//!   / `dts` / `duration` in the media time base and the `stss` sync
//!   flags; `seek_to` lands on sync samples.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, CodecTag, Decoder, Error as CoreError, Frame,
    MetadataBlob, Packet, PixelFormat, ProbeContext, ProbeData, ReadSeek, Result as CoreResult,
    StreamInfo, TimeBase,
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
/// The codec id oxideav-avif registers its still encoder under (one id,
/// one owner — this crate registers nothing under it): its packets are
/// whole `avif`-branded files, which the `"heif"` muxer writes through
/// exactly like a `"heif"` still stream's.
pub const CODEC_ID_AVIF: &str = "avif";
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

/// The colour information a decoded item carries: a `tmap` decodes to
/// its base rendition by default, with the base's colour information
/// ([`decode::ToneMapOutput`]); every other item its own.
fn output_colour(node: &ImageNode) -> crate::props::Colr {
    match (&node.kind, node.inputs.first()) {
        (ImageKind::ToneMap(_), Some(base)) => item_colour(base),
        _ => item_colour(node),
    }
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

/// One still stream: its index, its items (one packet each) and the
/// next packet to emit.
struct StillStream {
    index: u32,
    items: Vec<u32>,
    cursor: usize,
}

/// The displayable items of one predicted output: the items in
/// packet order, the layout / geometry / colour they share, and the
/// first item's derivation graph (the stream description's source).
struct StillGroup {
    items: Vec<u32>,
    layout: Option<HeifPixelFormat>,
    size: Option<(u32, u32)>,
    colour: crate::props::Colr,
    first: ImageNode,
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

/// The metadata blobs of an image item (r476, core 0.1.40): its ICC
/// profile (`colr` `prof` / `rICC`), the Exif payload and the XMP
/// packet of the metadata items describing it — the
/// [`crate::Metadata`] the contract decode yields, as
/// [`crate::api_registry::blobs_of_metadata`] forms it. An unreadable
/// metadata item leaves its blob out (the decode reports it).
fn item_blobs<D: AsRef<[u8]>>(file: &HeifFile<D>, node: &ImageNode) -> Vec<MetadataBlob> {
    let (exif, xmp) = decode::metadata_payloads(file, node).unwrap_or((None, None));
    crate::api_registry::blobs_of_metadata(&crate::api::Metadata::new(
        node.properties.icc_profile().map(<[u8]>::to_vec),
        exif,
        xmp.map(String::into_bytes),
        None,
    ))
}

/// The metadata blobs of a visual sample entry: its ICC profile
/// (`colr` `prof` / `rICC`) — what the contract `decode_all` puts on
/// every frame of the track.
fn entry_blobs(entry: &crate::sequence::SampleEntry) -> Vec<MetadataBlob> {
    crate::api_registry::blobs_of_metadata(&crate::api::Metadata::new(
        entry.colr.iter().find_map(|c| match c {
            crate::props::Colr::Icc { profile, .. } => Some(profile.clone()),
            _ => None,
        }),
        None,
        None,
        None,
    ))
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
    /// The still streams — one per distinct predicted output among the
    /// displayable image items, the primary's first — each with its
    /// pictures, one packet each, in [`crate::decode_all`]'s order.
    stills: Vec<StillStream>,
    /// Absolute offset and width (2 / 4 bytes) of the `pitm` item id
    /// field, patched to name the item a burst packet carries.
    pitm_field: Option<(usize, usize)>,
    tracks: Vec<(u32, TrackStream)>,
    active: Option<Vec<u32>>,
    metadata: Vec<(String, String)>,
}

/// The displayable image items of `file` in [`crate::decode_all`]'s
/// order — the entries of the viewer's display order (MIAF Annex A:
/// non-hidden masters, thumbnails and auxiliaries excluded, an `altr`
/// group counting once), the entry holding the primary first, then by
/// ascending first id; each entry contributes its first member whose
/// derivation graph builds and whose output can be predicted (a `tmap`
/// listed before its base, as `decode_all` picks it) — grouped by
/// predicted output (layout, geometry and colour description) so one
/// stream description fits every packet of a group, groups in
/// first-appearance order. A file whose display order yields nothing
/// (the primary a thumbnail of nothing displayable, …) carries the
/// primary alone.
fn still_groups<D: AsRef<[u8]>>(file: &HeifFile<D>, primary: &ImageNode) -> Vec<StillGroup> {
    let pid = primary.item.id;
    let mut groups: Vec<StillGroup> = Vec::new();
    if let Ok(meta) = file.meta() {
        let mut entries = meta.display_order();
        entries.sort_by_key(|e| (!e.contains(&pid), e.first().copied().unwrap_or(u32::MAX)));
        // The primary comes first even when the display order leaves
        // it out (a thumbnail named by `pitm`), as `decode_all` does.
        if !entries.iter().any(|e| e.contains(&pid)) {
            entries.insert(0, vec![pid]);
        }
        // The other view of a `ster` pair rides the pair's packet (the
        // decoder emits both views, tagged), never a packet of its own.
        let mut partners: Vec<u32> = Vec::new();
        for alternatives in entries {
            if alternatives.iter().any(|id| partners.contains(id)) {
                continue;
            }
            let pick = alternatives.iter().copied().find_map(|id| {
                let node = build_graph(file, id).ok()?;
                let (fmt, size) = predict_output(&node).ok()?;
                Some((id, fmt, size, node))
            });
            let Some((id, fmt, size, node)) = pick else {
                continue;
            };
            if let Some((l, r)) = stereo_pair_of(file, id) {
                partners.push(l);
                partners.push(r);
            }
            let colour = output_colour(&node);
            match groups
                .iter_mut()
                .find(|g| g.layout == Some(fmt) && g.size == Some(size) && g.colour == colour)
            {
                Some(g) => g.items.push(id),
                None => groups.push(StillGroup {
                    items: vec![id],
                    layout: Some(fmt),
                    size: Some(size),
                    colour,
                    first: node,
                }),
            }
        }
    }
    if groups.is_empty() {
        let predicted = predict_output(primary).ok();
        groups.push(StillGroup {
            items: vec![pid],
            layout: predicted.map(|p| p.0),
            size: predicted.map(|p| p.1),
            colour: output_colour(primary),
            first: primary.clone(),
        });
    }
    groups
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
        let mut stills: Vec<StillStream> = Vec::new();
        // The still streams (always first), built before the track
        // streams and spliced in at the front.
        let mut pending_stills: Vec<(CodecParameters, Vec<u32>)> = Vec::new();
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
                let mut groups = still_groups(&file, &node);
                match pitm_field {
                    Some((_, width)) => {
                        for g in &mut groups {
                            g.items.retain(|&id| {
                                id == node.item.id || width == 4 || u16::try_from(id).is_ok()
                            });
                        }
                        groups.retain(|g| !g.items.is_empty());
                    }
                    None => {
                        // Only the pitm itself can be named.
                        groups.retain(|g| g.items.contains(&node.item.id));
                        for g in &mut groups {
                            g.items.retain(|&id| id == node.item.id);
                        }
                    }
                }
                if groups.is_empty() {
                    let predicted = predict_output(&node).ok();
                    groups.push(StillGroup {
                        items: vec![node.item.id],
                        layout: predicted.map(|p| p.0),
                        size: predicted.map(|p| p.1),
                        colour: output_colour(&node),
                        first: node.clone(),
                    });
                }
                for (gi, g) in groups.iter().enumerate() {
                    let mut params = CodecParameters::video(CodecId::new(CODEC_ID))
                        .with_color_signal(crate::image::core_bridge::color_signal_of(&g.colour));
                    match (g.layout, g.size) {
                        (Some(fmt), Some((w, h))) => {
                            params.width = Some(w);
                            params.height = Some(h);
                            params.pixel_format = core_pixel_format_for(fmt, &g.colour);
                        }
                        _ => {
                            if let Ok((w, h)) = g.first.output_size() {
                                params.width = Some(w);
                                params.height = Some(h);
                            }
                        }
                    }
                    if gi == 0 {
                        let layers = still_layers(&file, &g.first);
                        if !layers.is_empty() {
                            params = params.with_layers(layers);
                        }
                    }
                    // The stream's metadata blobs (r476, core 0.1.40):
                    // the first item's ICC profile / Exif / XMP — what
                    // `info()` reports for the primary; every packet's
                    // frame carries its own item's as per-frame blobs.
                    params = params.with_blobs(item_blobs(&file, &g.first));
                    pending_stills.push((params, g.items.clone()));
                }
            }
        }
        // Track streams take the indices after the stills.
        let track_base = pending_stills.len();
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
                    .with_color_signal(crate::image::core_bridge::color_signal_of(&colour))
                    .with_blobs(entry_blobs(entry));
                params.width = Some(entry.width as u32);
                params.height = Some(entry.height as u32);
                params.pixel_format = core_pixel_format_for(layout.with_alpha(), &colour);
                let time_base = TimeBase::new(1, t.timescale.max(1) as i64);
                let index = (track_base + streams.len()) as u32;
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
                params = params
                    .with_tag(CodecTag::fourcc(&entry.entry_type))
                    .with_blobs(entry_blobs(entry));
                let time_base = TimeBase::new(1, t.timescale.max(1) as i64);
                let index = (track_base + streams.len()) as u32;
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
                metadata.push((format!("stream:{index}:track_id"), t.track_id.to_string()));
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
        // The still streams at their indices (0..).
        let mut still_infos = Vec::with_capacity(pending_stills.len());
        for (index, (params, items)) in pending_stills.into_iter().enumerate() {
            let index = index as u32;
            metadata.push((
                format!("stream:{index}:item_ids"),
                items
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ));
            still_infos.push(StreamInfo {
                index,
                time_base: TimeBase::new(1, 1),
                duration: Some(items.len() as i64),
                start_time: Some(0),
                params,
            });
            stills.push(StillStream {
                index,
                items,
                cursor: 0,
            });
        }
        streams.splice(0..0, still_infos);
        if streams.is_empty() {
            return Err(HeifError::unsupported(
                "file carries neither a pict meta box with a primary item nor a visual track",
            ));
        }
        Ok(Self {
            file,
            movie,
            streams,
            stills,
            pitm_field,
            tracks,
            active: None,
            metadata,
        })
    }

    /// The image items the first still stream carries, one packet
    /// each, in packet order (the primary first; see
    /// [`crate::decode_all`]). Empty for a file without a still.
    pub fn still_items(&self) -> &[u32] {
        self.stills
            .first()
            .map(|s| s.items.as_slice())
            .unwrap_or(&[])
    }

    /// The image items of still stream `stream` (`None` for a track
    /// stream or an unknown index).
    pub fn still_stream_items(&self, stream: u32) -> Option<&[u32]> {
        self.stills
            .iter()
            .find(|s| s.index == stream)
            .map(|s| s.items.as_slice())
    }

    /// Packet `index` of a still stream — item `id`: the whole file,
    /// with the `pitm` naming the picture's item so the `"heif"`
    /// decoder reconstructs that item (its own derivations, transforms
    /// and auxiliaries) exactly as the primary. Untimed: `pts` is the
    /// index in a 1/1 time base, duration 1.
    fn still_packet(&self, stream: u32, id: u32, index: usize) -> CoreResult<Packet> {
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
        // The still streams first, in stream order.
        for si in 0..self.stills.len() {
            let (index, len, cursor) = {
                let s = &self.stills[si];
                (s.index, s.items.len(), s.cursor)
            };
            if cursor < len {
                if self.is_active(index) {
                    let id = self.stills[si].items[cursor];
                    self.stills[si].cursor += 1;
                    return self.still_packet(index, id, cursor);
                }
                // An inactive still stream is skipped altogether.
                self.stills[si].cursor = len;
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
        if let Some(s) = self.stills.iter_mut().find(|s| s.index == stream_index) {
            // Still packets are the item index (1/1 time base).
            let last = s.items.len().saturating_sub(1);
            s.cursor = usize::try_from(pts.max(0)).unwrap_or(last).min(last);
            return Ok(s.cursor as i64);
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

/// Options of the `"heif"` decoder (`CodecParameters::options`; the
/// declared schema of the registry entry): the contract's
/// [`DecodeOptions`](crate::api::DecodeOptions) limits and switches, so
/// a pipeline bounds a decode before any allocation. `0` on a limit
/// keeps the contract default.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct HeifDecoderOptions {
    /// `max_width`: widest output accepted (0 = 2²⁰).
    pub max_width: u32,
    /// `max_height`: tallest output accepted (0 = 2²⁰).
    pub max_height: u32,
    /// `max_pixels`: largest output area accepted (0 = 2³⁰).
    pub max_pixels: u32,
    /// `max_bytes`: largest file accepted (0 = 4 GiB).
    pub max_bytes: u32,
    /// `strict`: require a HEIF-family brand and MIAF conformance of
    /// a `miaf`-branded file.
    pub strict: bool,
    /// `tone_mapped`: decode a `tmap` to its HDR reconstruction rather
    /// than the base rendition.
    pub tone_mapped: bool,
    /// `item_id`: decode this item instead of the `pitm` (0 = `pitm`;
    /// a burst's packets still name their own item).
    pub item_id: u32,
    /// `base_layer_fallback`: decode the base layer of a layered item
    /// whose enhancement layers cannot be decoded.
    pub base_layer_fallback: bool,
    /// `reference_white`: the SDR reference white in nits of a
    /// tone-mapped decode (0 = the contract default).
    pub reference_white: f32,
}

impl HeifDecoderOptions {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default`, then read / assign its public fields).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        max_width: u32,
        max_height: u32,
        max_pixels: u32,
        max_bytes: u32,
        strict: bool,
        tone_mapped: bool,
        item_id: u32,
        base_layer_fallback: bool,
        reference_white: f32,
    ) -> Self {
        Self {
            max_width,
            max_height,
            max_pixels,
            max_bytes,
            strict,
            tone_mapped,
            item_id,
            base_layer_fallback,
            reference_white,
        }
    }

    /// Set `max_width`.
    pub fn with_max_width(mut self, v: u32) -> Self {
        self.max_width = v;
        self
    }

    /// Set `max_height`.
    pub fn with_max_height(mut self, v: u32) -> Self {
        self.max_height = v;
        self
    }

    /// Set `max_pixels`.
    pub fn with_max_pixels(mut self, v: u32) -> Self {
        self.max_pixels = v;
        self
    }

    /// Set `max_bytes`.
    pub fn with_max_bytes(mut self, v: u32) -> Self {
        self.max_bytes = v;
        self
    }

    /// Set `strict`.
    pub fn with_strict(mut self, v: bool) -> Self {
        self.strict = v;
        self
    }

    /// Set `tone_mapped`.
    pub fn with_tone_mapped(mut self, v: bool) -> Self {
        self.tone_mapped = v;
        self
    }

    /// Set `item_id`.
    pub fn with_item_id(mut self, v: u32) -> Self {
        self.item_id = v;
        self
    }

    /// Set `base_layer_fallback`.
    pub fn with_base_layer_fallback(mut self, v: bool) -> Self {
        self.base_layer_fallback = v;
        self
    }

    /// Set `reference_white`.
    pub fn with_reference_white(mut self, v: f32) -> Self {
        self.reference_white = v;
        self
    }

    /// The contract [`DecodeOptions`](crate::api::DecodeOptions) these
    /// settings describe (the thread budget is the execution
    /// context's, set by the decoder).
    pub fn to_decode_options(&self) -> crate::api::DecodeOptions {
        let mut o = crate::api::DecodeOptions::default();
        if self.max_width > 0 {
            o = o.with_max_width(Some(self.max_width));
        }
        if self.max_height > 0 {
            o = o.with_max_height(Some(self.max_height));
        }
        if self.max_pixels > 0 {
            o = o.with_max_pixels(Some(u64::from(self.max_pixels)));
        }
        if self.max_bytes > 0 {
            o = o.with_max_bytes(Some(u64::from(self.max_bytes)));
        }
        if self.item_id > 0 {
            o = o.with_item_id(Some(self.item_id));
        }
        if self.reference_white > 0.0 {
            o = o.with_reference_white_nits(Some(f64::from(self.reference_white)));
        }
        o.with_strict(self.strict)
            .with_tone_mapped(self.tone_mapped)
            .with_base_layer_fallback(self.base_layer_fallback)
    }
}

impl Default for HeifDecoderOptions {
    fn default() -> Self {
        Self::new(0, 0, 0, 0, false, false, 0, false, 0.0)
    }
}

impl oxideav_core::CodecOptionsStruct for HeifDecoderOptions {
    const SCHEMA: &'static [oxideav_core::OptionField] = &[
        oxideav_core::OptionField {
            name: "max_width",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(0),
            help: "Widest output accepted (0 = the contract default, 2^20)",
        },
        oxideav_core::OptionField {
            name: "max_height",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(0),
            help: "Tallest output accepted (0 = the contract default, 2^20)",
        },
        oxideav_core::OptionField {
            name: "max_pixels",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(0),
            help: "Largest output area accepted (0 = the contract default, 2^30)",
        },
        oxideav_core::OptionField {
            name: "max_bytes",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(0),
            help: "Largest file accepted (0 = the contract default, 4 GiB)",
        },
        oxideav_core::OptionField {
            name: "strict",
            kind: oxideav_core::OptionKind::Bool,
            default: oxideav_core::OptionValue::Bool(false),
            help: "Require a HEIF-family brand and MIAF conformance of a miaf-branded file",
        },
        oxideav_core::OptionField {
            name: "tone_mapped",
            kind: oxideav_core::OptionKind::Bool,
            default: oxideav_core::OptionValue::Bool(false),
            help: "Decode a tmap item to its HDR reconstruction instead of the base rendition",
        },
        oxideav_core::OptionField {
            name: "item_id",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(0),
            help: "Decode this item instead of the primary (0 = the pitm)",
        },
        oxideav_core::OptionField {
            name: "base_layer_fallback",
            kind: oxideav_core::OptionKind::Bool,
            default: oxideav_core::OptionValue::Bool(false),
            help:
                "Decode the base layer of a layered item whose enhancement layers cannot be decoded",
        },
        oxideav_core::OptionField {
            name: "reference_white",
            kind: oxideav_core::OptionKind::F32,
            default: oxideav_core::OptionValue::F32(0.0),
            help: "SDR reference white in nits for a tone-mapped decode (0 = the contract default)",
        },
    ];

    fn apply(&mut self, key: &str, value: &oxideav_core::OptionValue) -> CoreResult<()> {
        match key {
            "max_width" => self.max_width = value.as_u32()?,
            "max_height" => self.max_height = value.as_u32()?,
            "max_pixels" => self.max_pixels = value.as_u32()?,
            "max_bytes" => self.max_bytes = value.as_u32()?,
            "strict" => self.strict = value.as_bool()?,
            "tone_mapped" => self.tone_mapped = value.as_bool()?,
            "item_id" => self.item_id = value.as_u32()?,
            "base_layer_fallback" => self.base_layer_fallback = value.as_bool()?,
            "reference_white" => self.reference_white = value.as_f32()?,
            other => {
                return Err(CoreError::invalid(format!(
                    "heif: unknown decoder option '{other}'"
                )))
            }
        }
        Ok(())
    }
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
    /// The contract options (limits, strictness, tone mapping) this
    /// decoder runs with.
    opts: crate::api::DecodeOptions,
}

impl HeifCodec {
    /// Construct with the id the decoder reports, at the contract's
    /// default options.
    pub fn new(codec_id: CodecId) -> Self {
        Self {
            codec_id,
            queue: VecDeque::new(),
            flushed: false,
            last: None,
            exec: oxideav_core::ExecutionContext::serial(),
            opts: crate::api::DecodeOptions::default(),
        }
    }

    /// Run with these contract options (limits, strictness, item,
    /// tone mapping; a thread budget set here wins over the execution
    /// context's).
    pub fn with_decode_options(mut self, opts: crate::api::DecodeOptions) -> Self {
        self.opts = opts;
        self
    }

    /// The last image decoded by [`Decoder::send_packet`].
    pub fn last_image(&self) -> Option<&decode::DecodedImage> {
        self.last.as_ref()
    }

    /// The contract options this decoder runs with: the configured
    /// options, with the thread budget of `set_execution_context`
    /// unless they carry one.
    fn decode_options(&self) -> crate::api::DecodeOptions {
        let mut o = self.opts.clone();
        if o.threads.is_none() {
            o = o.with_threads((self.exec.threads > 1).then_some(self.exec.threads));
        }
        o
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
        // The item's metadata (ICC / Exif / XMP) rides the frame as its
        // blobs (r476, core 0.1.40) — what the contract `HeifImage`
        // carries in `metadata`.
        let metadata_of = |d: &decode::DecodedImage| {
            crate::api::Metadata::new(
                d.icc_profile.clone(),
                d.exif.clone(),
                d.xmp.as_ref().map(|x| x.as_bytes().to_vec()),
                None,
            )
        };
        let signal_frame =
            |f: &crate::HeifFrame, nclx: &crate::props::Colr, metadata: crate::api::Metadata| {
                let image = crate::api::HeifImage::from_frame(
                    f.clone(),
                    crate::api::ColorInfo::from_colr(nclx),
                    metadata,
                );
                let (mut vf, _pf) = image.into_video_frame();
                vf.pts = packet.pts;
                Ok::<_, CoreError>(vf)
            };
        if !img.layers.is_empty() {
            // A layered item without lsel: every output layer, tagged.
            for l in &img.layers {
                let vf = signal_frame(&l.frame, &img.nclx, metadata_of(&img))?
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
                let vf = signal_frame(&image.frame, &image.nclx, metadata_of(image))?
                    .with_layer(oxideav_core::LayerIdentity::new(layer).with_view_id(view));
                self.queue.push_back(Frame::Video(vf));
            }
        } else {
            let vf = signal_frame(&img.frame, &img.nclx, metadata_of(&img))?;
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

/// Direct factory endpoint for the `"heif"` decoder: `params.options`
/// are the declared [`HeifDecoderOptions`] (unknown keys are refused).
pub fn make_decoder(params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
    let typed = oxideav_core::parse_options::<HeifDecoderOptions>(&params.options)?;
    Ok(Box::new(
        HeifCodec::new(params.codec_id.clone()).with_decode_options(typed.to_decode_options()),
    ))
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
