//! Framework [`Muxer`] (`registry` feature): coded video packets in, an
//! `msf1` image-sequence file out (via [`SequenceWriter`]).
//!
//! The muxer takes the first video stream it is given. HEVC packets
//! may arrive either Annex B framed (what `oxideav-h265`'s encoder
//! emits, parameter sets in band) or `hvcC` + length-prefixed (a
//! stream demuxed from another ISOBMFF file); the parameter sets are
//! lifted into the sample entry's `hvcC` and every sample is re-framed
//! with 4-byte length prefixes (ISO/IEC 14496-15). AV1 packets are
//! temporal units and need an `av1C` record in the stream's
//! `extradata`. The first packet also becomes the file-level `meta`
//! still (cover image, HEIF §7.1) when it is a sync sample of a
//! self-contained codec configuration.
//!
//! A `"heif"` still stream (the `"heif"` encoder's output: every packet
//! is a complete file) is passed through: exactly one packet, written
//! as the file; a second packet is refused.

use std::io::Write;

use oxideav_core::{
    Error as CoreError, Muxer, Packet, Result as CoreResult, StreamInfo, WriteSeek,
};

use crate::av1c::Av1Config;
use crate::avcc::AvcConfig;
use crate::encode::{avc_item_from_annex_b, avc_sample_from_annex_b, hevc_item_from_annex_b};
use crate::error::{HeifError, Result};
use crate::hvcc::{join_length_prefixed, split_length_prefixed, HevcConfig};
use crate::meta::{ITEM_TYPE_AV01, ITEM_TYPE_AVC1, ITEM_TYPE_HVC1};
use crate::props::{Colr, Ispe, Pixi, Property};
use crate::sequence::MetadataTrackKind;
use crate::vvcc::VvcConfig;
use crate::vvcdec::vvc_item_from_annex_b;
use crate::writer::{HeifWriter, SequenceMetadataTrack, SequenceWriter};

/// Container name the muxer is registered under.
pub const MUXER_NAME: &str = "heif";

enum Codec {
    Hevc,
    Av1,
    /// VVC (`"h266"`): Annex B access units or length-prefixed samples
    /// with a `vvcC` extradata (HEIF Annex L.3).
    Vvc,
    /// AVC (`"h264"`): Annex B access units or length-prefixed samples
    /// with an `avcC` extradata (HEIF Annex E.3).
    Avc,
    /// A `"heif"` / `"avif"` still stream: one whole-file packet.
    Still,
}

/// A timed metadata stream on its way to a metadata track: stream
/// index, kind, media timescale, `(sample, duration)` so far.
struct MetadataStream {
    index: u32,
    kind: MetadataTrackKind,
    timescale: u32,
    samples: Vec<(Vec<u8>, u32)>,
}

/// The image-sequence muxer.
pub struct HeifSequenceMuxer {
    output: Box<dyn WriteSeek>,
    stream: StreamInfo,
    codec: Codec,
    hvcc: Option<HevcConfig>,
    av1c: Option<Av1Config>,
    vvcc: Option<VvcConfig>,
    avcc: Option<AvcConfig>,
    coded_size: Option<(u32, u32)>,
    samples: Vec<(Vec<u8>, u32, bool)>,
    /// The whole-file packets of a still stream (one picture each).
    still: Vec<Vec<u8>>,
    /// The `Data` streams of codec id `exif` / `xmp` (HEIF §8.3 timed
    /// metadata).
    metadata_streams: Vec<MetadataStream>,
    /// The `copyright` / `copyright:<lang>` pairs kept by
    /// [`Muxer::set_metadata`], written as `cprt` boxes.
    copyrights: Vec<crate::sequence::Copyright>,
    header_written: bool,
    finished: bool,
}

/// The `cprt` language a metadata key names: `copyright` is the
/// undetermined language `und`, `copyright:<lang>` a three-letter
/// lower-case ISO 639-2/T code; any other key is not carried.
fn copyright_language(key: &str) -> Option<&str> {
    if key == "copyright" {
        return Some("und");
    }
    let lang = key.strip_prefix("copyright:")?;
    (lang.len() == 3 && lang.bytes().all(|b| b.is_ascii_lowercase())).then_some(lang)
}

impl HeifSequenceMuxer {
    /// Create over `streams` (the first video stream is muxed).
    pub fn new(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Self> {
        let stream = streams
            .iter()
            .find(|s| s.params.media_type == oxideav_core::MediaType::Video)
            .cloned()
            .ok_or_else(|| HeifError::invalid("heif muxer: no video stream"))?;
        let codec = match stream.params.codec_id.as_str() {
            "h265" | "hevc" => Codec::Hevc,
            "av1" => Codec::Av1,
            "h266" | "vvc" => Codec::Vvc,
            "h264" | "avc" => Codec::Avc,
            // The `"heif"` and `"avif"` still encoders emit the same
            // packet shape — one composed HEIF-family file per packet,
            // already branded — written through unchanged.
            crate::demux::CODEC_ID | crate::demux::CODEC_ID_AVIF => Codec::Still,
            other => {
                return Err(HeifError::unsupported(format!(
                    "heif muxer: codec '{other}' (heif / avif still, h265 / av1 / h266 / h264 sequences supported)"
                )))
            }
        };
        let mut hvcc = None;
        let mut av1c = None;
        let mut vvcc = None;
        let mut avcc = None;
        if !stream.params.extradata.is_empty() {
            match codec {
                Codec::Hevc => {
                    if let Ok(c) = HevcConfig::parse(&stream.params.extradata) {
                        hvcc = Some(c);
                    }
                }
                Codec::Av1 => av1c = Some(Av1Config::parse(&stream.params.extradata)?),
                Codec::Vvc => {
                    if let Ok(c) = VvcConfig::parse(&stream.params.extradata) {
                        vvcc = Some(c);
                    }
                }
                Codec::Avc => {
                    if let Ok(c) = AvcConfig::parse(&stream.params.extradata) {
                        avcc = Some(c);
                    }
                }
                Codec::Still => {}
            }
        }
        let coded_size = match (stream.params.width, stream.params.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
            _ => None,
        };
        // Timed metadata streams describing the pictures (the demuxer's
        // `exif` / `xmp` `Data` streams, or a caller's): each becomes a
        // metadata track with a `cdsc` reference to the picture track.
        let metadata_streams = streams
            .iter()
            .filter(|s| s.params.media_type == oxideav_core::MediaType::Data)
            .filter_map(|s| {
                let kind = match s.params.codec_id.as_str() {
                    crate::demux::CODEC_ID_EXIF => MetadataTrackKind::Exif,
                    crate::demux::CODEC_ID_XMP => MetadataTrackKind::Xmp,
                    _ => return None,
                };
                Some(MetadataStream {
                    index: s.index,
                    kind,
                    timescale: s.time_base.den().max(1) as u32,
                    samples: Vec::new(),
                })
            })
            .collect();
        Ok(Self {
            output,
            stream,
            codec,
            hvcc,
            av1c,
            vvcc,
            avcc,
            coded_size,
            samples: Vec::new(),
            still: Vec::new(),
            metadata_streams,
            copyrights: Vec::new(),
            header_written: false,
            finished: false,
        })
    }

    fn push(&mut self, packet: &Packet) -> Result<()> {
        let duration = packet.duration.unwrap_or(1).max(1) as u32;
        match self.codec {
            Codec::Still => {
                // The packet must be a HEIF file this crate can read.
                // One packet is the file; several (a burst: the still
                // encoder's packets for the frames of one stream, or
                // the demuxer's per-item packets) are merged into one
                // image collection at `finish`.
                crate::file::HeifFile::parse_borrowed(&packet.data).map_err(|e| {
                    HeifError::invalid(format!("heif muxer: still packet is not a HEIF file: {e}"))
                })?;
                self.still.push(packet.data.clone());
            }
            Codec::Hevc => {
                // Length-prefixed input when an hvcC was supplied and the
                // payload does not start with an Annex B start code.
                let annex_b =
                    packet.data.starts_with(&[0, 0, 1]) || packet.data.starts_with(&[0, 0, 0, 1]);
                if annex_b {
                    let (cfg, data, w, h, _) = hevc_item_from_annex_b(&packet.data)?;
                    if self.hvcc.is_none() {
                        self.hvcc = Some(cfg);
                        self.coded_size.get_or_insert((w, h));
                    }
                    self.samples.push((data, duration, packet.is_keyframe()));
                } else {
                    let cfg = self.hvcc.as_ref().ok_or_else(|| {
                        HeifError::invalid(
                            "heif muxer: length-prefixed HEVC packets need an hvcC extradata",
                        )
                    })?;
                    let nals = split_length_prefixed(&packet.data, cfg.length_size)?;
                    let data = if cfg.length_size == 4 {
                        packet.data.clone()
                    } else {
                        join_length_prefixed(&nals, 4)?
                    };
                    self.samples.push((data, duration, packet.is_keyframe()));
                }
            }
            Codec::Av1 => {
                if self.av1c.is_none() {
                    return Err(HeifError::invalid(
                        "heif muxer: AV1 streams need an av1C extradata",
                    ));
                }
                self.samples
                    .push((packet.data.clone(), duration, packet.is_keyframe()));
            }
            Codec::Vvc => {
                let annex_b =
                    packet.data.starts_with(&[0, 0, 1]) || packet.data.starts_with(&[0, 0, 0, 1]);
                if annex_b {
                    let (cfg, data, w, h, _) = vvc_item_from_annex_b(&packet.data)?;
                    if self.vvcc.is_none() {
                        self.vvcc = Some(cfg);
                        self.coded_size.get_or_insert((w, h));
                    }
                    self.samples.push((data, duration, packet.is_keyframe()));
                } else {
                    let cfg = self.vvcc.as_ref().ok_or_else(|| {
                        HeifError::invalid(
                            "heif muxer: length-prefixed VVC packets need a vvcC extradata",
                        )
                    })?;
                    let nals = split_length_prefixed(&packet.data, cfg.length_size)?;
                    let data = if cfg.length_size == 4 {
                        packet.data.clone()
                    } else {
                        join_length_prefixed(&nals, 4)?
                    };
                    self.samples.push((data, duration, packet.is_keyframe()));
                }
            }
            Codec::Avc => {
                let annex_b =
                    packet.data.starts_with(&[0, 0, 1]) || packet.data.starts_with(&[0, 0, 0, 1]);
                if annex_b {
                    // An access unit with an SPS yields the record; the
                    // others are length-prefixed as they are.
                    let has_sps = oxideav_h264::nal::AnnexBSplitter::new(&packet.data)
                        .any(|n| n.first().map(|h| h & 0x1f) == Some(7));
                    let data = if has_sps {
                        let (cfg, data, w, h, _) = avc_item_from_annex_b(&packet.data)?;
                        if self.avcc.is_none() {
                            self.avcc = Some(cfg);
                            self.coded_size.get_or_insert((w, h));
                        }
                        data
                    } else {
                        avc_sample_from_annex_b(&packet.data)?
                    };
                    self.samples.push((data, duration, packet.is_keyframe()));
                } else {
                    let cfg = self.avcc.as_ref().ok_or_else(|| {
                        HeifError::invalid(
                            "heif muxer: length-prefixed AVC packets need an avcC extradata",
                        )
                    })?;
                    let nals = split_length_prefixed(&packet.data, cfg.length_size)?;
                    let data = if cfg.length_size == 4 {
                        packet.data.clone()
                    } else {
                        join_length_prefixed(&nals, 4)?
                    };
                    self.samples.push((data, duration, packet.is_keyframe()));
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if let Codec::Still = self.codec {
            let stills = std::mem::take(&mut self.still);
            let bytes = match stills.as_slice() {
                [] => {
                    return Err(HeifError::invalid(
                        "heif muxer: no still packet was written",
                    ))
                }
                // One picture: the encoder's file, through unchanged.
                [one] => one.clone(),
                // A burst: every packet's primary image (with its own
                // derivations, auxiliaries, thumbnails and metadata
                // items) becomes an item of one collection, the first
                // the primary — what the demuxer reads back as this
                // stream's packets, each frame with its own blobs.
                many => HeifWriter::from_files(many)?.write_to_vec()?,
            };
            self.output
                .write_all(&bytes)
                .map_err(|e| HeifError::invalid(format!("heif muxer: write: {e}")))?;
            self.output
                .flush()
                .map_err(|e| HeifError::invalid(format!("heif muxer: flush: {e}")))?;
            return Ok(());
        }
        let (w, h) = self
            .coded_size
            .ok_or_else(|| HeifError::invalid("heif muxer: unknown picture size"))?;
        let (entry_type, config, layout) =
            match self.codec {
                Codec::Hevc => {
                    let mut c = self.hvcc.clone().ok_or_else(|| {
                        HeifError::invalid("heif muxer: no HEVC parameter sets seen")
                    })?;
                    if c.length_size != 4 {
                        c.length_size = 4;
                        c.raw = c.serialize();
                    }
                    let layout = crate::decode::hevc_layout(&c)?;
                    (ITEM_TYPE_HVC1, Property::HvcC(c), layout)
                }
                Codec::Av1 => {
                    let c = self.av1c.clone().expect("checked in push");
                    let layout = crate::decode::av1_layout(&c)?;
                    (ITEM_TYPE_AV01, Property::Av1C(c), layout)
                }
                Codec::Vvc => {
                    let c = self.vvcc.clone().ok_or_else(|| {
                        HeifError::invalid("heif muxer: VVC stream without a vvcC record")
                    })?;
                    let layout = crate::decode::vvc_layout(&c)?;
                    (crate::meta::ITEM_TYPE_VVC1, Property::VvcC(c), layout)
                }
                Codec::Avc => {
                    let mut c = self.avcc.clone().ok_or_else(|| {
                        HeifError::invalid("heif muxer: no AVC parameter sets seen")
                    })?;
                    if c.length_size != 4 {
                        c.length_size = 4;
                        c.raw = c.serialize();
                    }
                    let layout = c.layout()?;
                    (ITEM_TYPE_AVC1, Property::AvcC(c), layout)
                }
                Codec::Still => unreachable!("handled above"),
            };
        let timescale = self.stream.time_base.den().max(1) as u32;
        let mut sw = SequenceWriter::new(entry_type, config.clone(), w as u16, h as u16, timescale);
        sw.copyrights = self.copyrights.clone();
        // The stream's metadata blobs (r476, core 0.1.40): the ICC
        // profile as the sample entry's `colr` (next to the nclx one,
        // as the still encoder writes them), Exif / XMP as items
        // describing the cover image (the file-level `meta` the still
        // path reads them from).
        let metadata = crate::api_registry::metadata_of_blobs(&[], self.stream.params.blobs());
        // Sequence-level Exif / XMP: the master track's MetaBox (HEIF
        // §8.3, Annex A.2.2 / A.3) — what the demuxer reads back onto
        // the track's stream parameters.
        sw.exif = metadata.exif.clone();
        sw.xmp = metadata.xmp.clone();
        for m in &self.metadata_streams {
            if m.samples.is_empty() {
                continue;
            }
            sw.push_metadata_track(SequenceMetadataTrack::new(
                m.kind,
                m.timescale,
                m.samples
                    .iter()
                    .map(|(d, dur)| crate::writer::SequenceSample::new(d.clone(), *dur, true))
                    .collect(),
            ));
        }
        // An ICC profile next to the nclx: HEIF §6.5.5 wants that nclx
        // to signal unspecified primaries / transfer (2 / 2) — the
        // still encoder's pairing.
        let nclx = crate::encode::nclx_for_icc(&Colr::MIAF_DEFAULT, metadata.icc.is_some());
        sw.entry_properties.push(Property::Colr(nclx.clone()));
        if let Some(icc) = &metadata.icc {
            sw.entry_properties.push(Property::Colr(Colr::Icc {
                restricted: false,
                profile: icc.clone(),
            }));
        }
        for (data, dur, sync) in &self.samples {
            sw.push_sample(data.clone(), *dur, *sync);
        }
        // Cover image: the first sync sample as a still item.
        if let Some((data, _, true)) = self.samples.first() {
            let mut still = HeifWriter::new();
            let mut props = vec![
                (config, true),
                (
                    Property::Ispe(Ispe {
                        width: w,
                        height: h,
                    }),
                    false,
                ),
                (
                    Property::Pixi(Pixi {
                        bits_per_channel: vec![layout.bit_depth; layout.chroma.colour_planes()],
                    }),
                    false,
                ),
                (Property::Colr(nclx), false),
            ];
            if let Some(icc) = &metadata.icc {
                props.push((
                    Property::Colr(Colr::Icc {
                        restricted: false,
                        profile: icc.clone(),
                    }),
                    false,
                ));
            }
            let id = still.add_coded_item(entry_type, data.clone(), props);
            if let Some(exif) = &metadata.exif {
                still.add_exif(id, exif);
            }
            if let Some(xmp) = &metadata.xmp {
                still.add_xmp_bytes(id, xmp.clone());
            }
            still.set_primary(id);
            sw.still = Some(still);
            // The cover aliases sample 0 (one copy of the bytes).
            sw.cover_sample = Some(0);
        }
        let bytes = sw.write_to_vec()?;
        self.output
            .write_all(&bytes)
            .map_err(|e| HeifError::invalid(format!("heif muxer: write: {e}")))?;
        self.output
            .flush()
            .map_err(|e| HeifError::invalid(format!("heif muxer: flush: {e}")))?;
        Ok(())
    }
}

impl Muxer for HeifSequenceMuxer {
    fn format_name(&self) -> &str {
        MUXER_NAME
    }

    fn write_header(&mut self) -> CoreResult<()> {
        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        if !self.header_written {
            return Err(CoreError::invalid("heif muxer: write_header first"));
        }
        if packet.stream_index != self.stream.index {
            if let Some(m) = self
                .metadata_streams
                .iter_mut()
                .find(|m| m.index == packet.stream_index)
            {
                let duration = u32::try_from(packet.duration.unwrap_or(1).max(1)).unwrap_or(1);
                m.samples.push((packet.data.clone(), duration));
            }
            return Ok(());
        }
        self.push(packet)?;
        Ok(())
    }

    /// The sequence file's text metadata: `copyright` (and
    /// `copyright:<lang>` for a notice in a given ISO 639-2/T language)
    /// is stored as a `cprt` box of the movie-level `udta` (ISO/IEC
    /// 14496-12 §8.10.2), one per language, in the given order; every
    /// other key is dropped — neither 14496-12 nor 23008-12 defines a
    /// carriage for free-form string pairs. A `"heif"` / `"avif"` still
    /// stream is written through as the file the encoder produced and
    /// has no movie box: nothing is stored and the call returns
    /// `Unsupported`. Must precede `write_header`.
    fn set_metadata(&mut self, metadata: &[(String, String)]) -> CoreResult<()> {
        if self.header_written {
            return Err(CoreError::invalid(
                "heif muxer: set_metadata must precede write_header",
            ));
        }
        if matches!(self.codec, Codec::Still) {
            return Err(CoreError::unsupported(
                "heif muxer: a still stream is written through as the encoder's file; it carries no cprt",
            ));
        }
        self.copyrights = metadata
            .iter()
            .filter_map(|(k, v)| {
                copyright_language(k).map(|lang| crate::sequence::Copyright::new(lang, v.as_str()))
            })
            .collect();
        Ok(())
    }

    fn carries_metadata_key(&self, key: &str) -> bool {
        !matches!(self.codec, Codec::Still) && copyright_language(key).is_some()
    }

    fn write_trailer(&mut self) -> CoreResult<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if self.samples.is_empty() && self.still.is_empty() {
            return Err(CoreError::invalid("heif muxer: no packets were written"));
        }
        self.finish()?;
        Ok(())
    }
}

/// [`oxideav_core::OpenMuxerFn`] for the registry.
pub fn open_muxer(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
) -> CoreResult<Box<dyn Muxer>> {
    Ok(Box::new(HeifSequenceMuxer::new(output, streams)?))
}
