//! Framework integration (`registry` feature): the HEIF container as an
//! [`oxideav_core::Demuxer`] and the `"heif"` still-image codec.
//!
//! A `.heic` / `.heif` / `.avif` file opens as a demuxer with:
//!
//! * **stream 0 — the still image** (when the file-level `meta` box has
//!   a `pict` handler and a primary item): codec id `"heif"`, one
//!   keyframe packet carrying the whole file. The `"heif"` decoder
//!   ([`HeifCodec`], registered by [`crate::registry::register`])
//!   reconstructs the primary item — derivations, transformative
//!   properties, alpha — and emits one `VideoFrame`. The stream's
//!   [`CodecParameters`] announce the predicted output geometry and
//!   pixel format so pipelines can allocate before decoding.
//! * **one stream per visual track** (`pict` / `vide` / `auxv`
//!   handlers of the `moov` box, image sequences): codec id resolved
//!   from the sample-entry type (`hvc1` → `"h265"`, `av01` → `"av1"`,
//!   …) through the [`CodecResolver`], `extradata` = the `hvcC` /
//!   `av1C` record, packets = the samples in decode order with `pts`
//!   / `dts` / `duration` in the media time base and the `stss` sync
//!   flags; `seek_to` lands on sync samples.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, CodecTag, Decoder, Error as CoreError, Frame, Packet,
    PixelFormat, ProbeContext, ProbeData, ReadSeek, Result as CoreResult, StreamInfo, TimeBase,
};

use crate::compose::needs_444;
use crate::decode::{self, decode_primary, ItemDecoder};
use crate::derived::{build_primary_graph, ImageKind, ImageNode};
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

#[doc(hidden)]
/// Predict the output layout and size of an image item without
/// decoding, by replaying the composition layer's promotion rules on
/// the container metadata. Used for the still stream's parameters.
pub fn predict_output(node: &ImageNode) -> Result<(HeifPixelFormat, (u32, u32))> {
    let (mut fmt, (mut w, mut h)) = match &node.kind {
        ImageKind::Coded(_) => {
            let f = decode::layout_of(&node.properties).ok_or_else(|| {
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
            let any_alpha = layouts.iter().any(|(f, _)| f.has_alpha);
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
                    if needs_444(fmt.chroma, w % 2 == 1, h % 2 == 1) {
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

/// The sample range an item's colour information declares (the MIAF
/// default — full — when it carries no `nclx`).
fn item_full_range(node: &ImageNode) -> bool {
    match node.properties.nclx() {
        Some(crate::props::Colr::Nclx { full_range, .. }) => *full_range,
        _ => true,
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
        b"hvc1" | b"hev1" | b"hvc2" | b"hev2" => Some(CodecId::new(decode::CODEC_ID_HEVC)),
        b"av01" => Some(CodecId::new(decode::CODEC_ID_AV1)),
        b"avc1" | b"avc3" => Some(CodecId::new("h264")),
        _ => None,
    }
}

struct TrackStream {
    track_index: usize,
    cursor: usize,
    time_base: TimeBase,
}

/// The HEIF demuxer.
pub struct HeifDemuxer {
    file: HeifFile,
    movie: Option<Movie>,
    streams: Vec<StreamInfo>,
    still_pending: bool,
    still_stream: Option<u32>,
    tracks: Vec<(u32, TrackStream)>,
    active: Option<Vec<u32>>,
    metadata: Vec<(String, String)>,
}

impl HeifDemuxer {
    /// Open a demuxer over a file held in memory.
    pub fn from_bytes(bytes: Vec<u8>, codecs: &dyn CodecResolver) -> Result<Self> {
        let file = HeifFile::from_vec(bytes)?;
        let movie = parse_movie(&file)?;
        let mut streams = Vec::new();
        let mut still_stream = None;
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
                match predict_output(&node) {
                    Ok((fmt, (w, h))) => {
                        params.width = Some(w);
                        params.height = Some(h);
                        params.pixel_format = core_pixel_format(fmt, item_full_range(&node));
                    }
                    Err(_) => {
                        if let Ok((w, h)) = node.output_size() {
                            params.width = Some(w);
                            params.height = Some(h);
                        }
                    }
                }
                metadata.push(("primary_item_id".into(), node.item.id.to_string()));
                metadata.push((
                    "primary_item_type".into(),
                    crate::boxes::fourcc_str(&node.item.item_type),
                ));
                still_stream = Some(streams.len() as u32);
                streams.push(StreamInfo {
                    index: streams.len() as u32,
                    time_base: TimeBase::new(1, 1),
                    duration: Some(1),
                    start_time: Some(0),
                    params,
                });
            }
        }
        // Track streams.
        let mut tracks = Vec::new();
        if let Some(mv) = &movie {
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
            still_pending: still_stream.is_some(),
            still_stream,
            tracks,
            active: None,
            metadata,
        })
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
        if self.still_pending {
            self.still_pending = false;
            if let Some(idx) = self.still_stream {
                if self.is_active(idx) {
                    return Ok(
                        Packet::new(idx, TimeBase::new(1, 1), self.file.bytes().to_vec())
                            .with_pts(0)
                            .with_dts(0)
                            .with_duration(1)
                            .with_keyframe(true),
                    );
                }
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
        let data = sample_bytes(&self.file, &s)?.to_vec();
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
            self.still_pending = true;
            return Ok(0);
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
}

impl HeifCodec {
    /// Construct with the id the decoder reports.
    pub fn new(codec_id: CodecId) -> Self {
        Self {
            codec_id,
            queue: VecDeque::new(),
            flushed: false,
            last: None,
        }
    }

    /// The last image decoded by [`Decoder::send_packet`].
    pub fn last_image(&self) -> Option<&decode::DecodedImage> {
        self.last.as_ref()
    }
}

impl Decoder for HeifCodec {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        let file = HeifFile::parse(&packet.data)?;
        let img = decode_primary(&file, ItemDecoder::direct())?;
        let full = matches!(
            img.nclx,
            crate::props::Colr::Nclx {
                full_range: true,
                ..
            }
        );
        let (mut vf, _pf) = img.frame.to_core_ranged(full)?;
        vf.pts = packet.pts;
        self.queue.push_back(Frame::Video(vf));
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
