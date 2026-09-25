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
use crate::encode::hevc_item_from_annex_b;
use crate::error::{HeifError, Result};
use crate::hvcc::{join_length_prefixed, split_length_prefixed, HevcConfig};
use crate::meta::{ITEM_TYPE_AV01, ITEM_TYPE_HVC1};
use crate::props::{Colr, Ispe, Pixi, Property};
use crate::writer::{HeifWriter, SequenceWriter};

/// Container name the muxer is registered under.
pub const MUXER_NAME: &str = "heif";

enum Codec {
    Hevc,
    Av1,
    /// A `"heif"` still stream: one whole-file packet.
    Still,
}

/// The image-sequence muxer.
pub struct HeifSequenceMuxer {
    output: Box<dyn WriteSeek>,
    stream: StreamInfo,
    codec: Codec,
    hvcc: Option<HevcConfig>,
    av1c: Option<Av1Config>,
    coded_size: Option<(u32, u32)>,
    samples: Vec<(Vec<u8>, u32, bool)>,
    /// The whole-file packet of a still stream.
    still: Option<Vec<u8>>,
    header_written: bool,
    finished: bool,
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
            crate::demux::CODEC_ID => Codec::Still,
            other => {
                return Err(HeifError::unsupported(format!(
                    "heif muxer: codec '{other}' (heif still, h265 / av1 sequences supported)"
                )))
            }
        };
        let mut hvcc = None;
        let mut av1c = None;
        if !stream.params.extradata.is_empty() {
            match codec {
                Codec::Hevc => {
                    if let Ok(c) = HevcConfig::parse(&stream.params.extradata) {
                        hvcc = Some(c);
                    }
                }
                Codec::Av1 => av1c = Some(Av1Config::parse(&stream.params.extradata)?),
                Codec::Still => {}
            }
        }
        let coded_size = match (stream.params.width, stream.params.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
            _ => None,
        };
        Ok(Self {
            output,
            stream,
            codec,
            hvcc,
            av1c,
            coded_size,
            samples: Vec::new(),
            still: None,
            header_written: false,
            finished: false,
        })
    }

    fn push(&mut self, packet: &Packet) -> Result<()> {
        let duration = packet.duration.unwrap_or(1).max(1) as u32;
        match self.codec {
            Codec::Still => {
                if self.still.is_some() {
                    return Err(HeifError::invalid(
                        "heif muxer: a still stream carries exactly one packet (one file); \
                         a second packet cannot be appended",
                    ));
                }
                // The packet must be a HEIF file this crate can read.
                crate::file::HeifFile::parse(&packet.data).map_err(|e| {
                    HeifError::invalid(format!("heif muxer: still packet is not a HEIF file: {e}"))
                })?;
                self.still = Some(packet.data.clone());
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
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if let Codec::Still = self.codec {
            let bytes = self
                .still
                .take()
                .ok_or_else(|| HeifError::invalid("heif muxer: no still packet was written"))?;
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
        let (entry_type, config, layout) = match self.codec {
            Codec::Hevc => {
                let mut c = self
                    .hvcc
                    .clone()
                    .ok_or_else(|| HeifError::invalid("heif muxer: no HEVC parameter sets seen"))?;
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
            Codec::Still => unreachable!("handled above"),
        };
        let timescale = self.stream.time_base.den().max(1) as u32;
        let mut sw = SequenceWriter::new(entry_type, config.clone(), w as u16, h as u16, timescale);
        sw.entry_properties.push(Property::Colr(Colr::MIAF_DEFAULT));
        for (data, dur, sync) in &self.samples {
            sw.push_sample(data.clone(), *dur, *sync);
        }
        // Cover image: the first sync sample as a still item.
        if let Some((data, _, true)) = self.samples.first() {
            let mut still = HeifWriter::new();
            let id = still.add_coded_item(
                entry_type,
                data.clone(),
                vec![
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
                    (Property::Colr(Colr::MIAF_DEFAULT), false),
                ],
            );
            still.set_primary(id);
            sw.still = Some(still);
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
            return Ok(());
        }
        self.push(packet)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> CoreResult<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if self.samples.is_empty() && self.still.is_none() {
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
