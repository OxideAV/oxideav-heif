//! Image sequences (ISO/IEC 23008-12 §7): the `moov` / `trak` / `stbl`
//! sample tables of `pict` (and `vide` / `auxv`) tracks, their visual
//! sample entries (`hvc1` + `hvcC`, `av01` + `av1C`, `ccst`, `auxi`,
//! `colr`, `clap`, `pasp`) and the §7.2.1 track-header matrix.
//!
//! The walk is container-only and part of the standalone surface: it
//! produces a flat [`Sample`] table per track (file offsets, sizes,
//! decode / composition times, sync flags) that the framework demuxer
//! turns into packets.

use crate::av1c::Av1Config;
use crate::boxes::{find_box, fourcc_str, iter_boxes, parse_full_box, payload, FourCc, Reader};
use crate::error::{HeifError, Result};
use crate::file::HeifFile;
use crate::hvcc::HevcConfig;
use crate::meta::RawProperty;
use crate::props::{Amve, Cclv, Clap, Clli, Colr, Mdcv, Pasp, Property};

/// Upper bound on the samples one track may expand to.
pub const MAX_SAMPLES: usize = 1 << 24;
/// Upper bound on the tracks a movie may declare.
pub const MAX_TRACKS: usize = 1 << 12;

/// `ccst` — coding constraints (§7.2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodingConstraints {
    /// `all_ref_pics_intra`.
    pub all_ref_pics_intra: bool,
    /// `intra_pred_used`.
    pub intra_pred_used: bool,
    /// `max_ref_per_pic` (15 = unconstrained).
    pub max_ref_per_pic: u8,
}

/// One visual sample entry of a track's `stsd`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleEntry {
    /// Entry type (`hvc1`, `hev1`, `av01`, …).
    pub entry_type: FourCc,
    /// `data_reference_index`.
    pub data_reference_index: u16,
    /// `width` from the VisualSampleEntry.
    pub width: u16,
    /// `height` from the VisualSampleEntry.
    pub height: u16,
    /// `hvcC` child, when present.
    pub hvcc: Option<HevcConfig>,
    /// `av1C` child, when present.
    pub av1c: Option<Av1Config>,
    /// `avcC` child, when present (`avc1` / `avc3` entries).
    pub avcc: Option<crate::avcc::AvcConfig>,
    /// `lhvC` child, when present (`lhv1` / `hvc2` entries).
    pub lhvc: Option<crate::lhvc::LhevcConfig>,
    /// `ccst` child (mandatory for `pict` tracks).
    pub ccst: Option<CodingConstraints>,
    /// `auxi` `aux_track_type` URN (auxiliary tracks).
    pub aux_track_type: Option<String>,
    /// `colr` children, in order.
    pub colr: Vec<Colr>,
    /// `clap` child.
    pub clap: Option<Clap>,
    /// `pasp` child.
    pub pasp: Option<Pasp>,
    /// `clli` child (ISO/IEC 14496-12 §12.1.6).
    pub clli: Option<Clli>,
    /// `mdcv` child (§12.1.7).
    pub mdcv: Option<Mdcv>,
    /// `cclv` child (§12.1.8).
    pub cclv: Option<Cclv>,
    /// `amve` child (§12.1.9).
    pub amve: Option<Amve>,
    /// Every child box, raw, in order.
    pub children: Vec<RawProperty>,
}

/// One sample of a track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Absolute file offset.
    pub offset: u64,
    /// Size in bytes.
    pub size: u32,
    /// Decode time in the media timescale.
    pub dts: u64,
    /// Composition offset (`ctts`) in the media timescale.
    pub cts_offset: i64,
    /// Duration (`stts` delta) in the media timescale.
    pub duration: u32,
    /// Sync sample (`stss`; every sample when the box is absent).
    pub is_sync: bool,
    /// 1-based `stsd` entry index.
    pub description_index: u32,
}

impl Sample {
    /// Presentation time (`dts + cts_offset`), saturating at 0.
    pub fn pts(&self) -> u64 {
        (self.dts as i128 + self.cts_offset as i128).max(0) as u64
    }
}

/// One `elst` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    /// `segment_duration` (movie timescale).
    pub segment_duration: u64,
    /// `media_time` (media timescale; −1 = empty edit).
    pub media_time: i64,
    /// `media_rate` as a 16.16 fixed-point value.
    pub media_rate: i32,
}

/// Orientation derived from the §7.2.1 track-header matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackOrientation {
    /// Anti-clockwise rotation in units of 90°.
    pub rotation: u8,
    /// Horizontal mirror (left/right exchanged) before rotation.
    pub mirror: bool,
}

/// A sample-to-group mapping (`sbgp`, ISO/IEC 14496-12 §8.9.2, or the
/// compact `csgp`, §8.9.5, expanded to the same run list).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleGroup {
    /// `grouping_type`.
    pub grouping_type: FourCc,
    /// `grouping_type_parameter` (`sbgp` v1 / `csgp` with the presence flag).
    pub grouping_type_parameter: Option<u32>,
    /// `(sample_count, group_description_index)` runs in sample order;
    /// index 0 = no group, 1-based into the matching `sgpd` (a set
    /// `csgp` msb marks a fragment-local description: bit 31 kept).
    pub entries: Vec<(u32, u32)>,
    /// `true` when parsed from a `csgp` box.
    pub compact: bool,
}

impl SampleGroup {
    /// The `group_description_index` of sample `index` (0-based), or
    /// `None` past the described samples.
    pub fn index_of(&self, index: usize) -> Option<u32> {
        let mut at = 0usize;
        for (count, gdi) in &self.entries {
            let next = at.saturating_add(*count as usize);
            if index < next {
                return Some(*gdi);
            }
            at = next;
        }
        None
    }
}

/// One `sgpd` box (§8.9.3): the group descriptions of a grouping type;
/// entries stay raw (their layout is per grouping type).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleGroupDescription {
    /// `grouping_type`.
    pub grouping_type: FourCc,
    /// Box version.
    pub version: u8,
    /// `default_length` (v1+; 0 = per-entry `description_length`).
    pub default_length: u32,
    /// `default_group_description_index` (v2+; 0 = no default).
    pub default_group_description_index: u32,
    /// The `SampleGroupDescriptionEntry` bodies, 1-based in the file.
    pub entries: Vec<Vec<u8>>,
}

/// A `prft` box (§8.16.5): producer reference time for one track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProducerReferenceTime {
    /// Box flags (0 / 1 / 2 / 4 / 8 / 16 / 24, §8.16.5.3).
    pub flags: u32,
    /// `reference_track_ID`.
    pub reference_track_id: u32,
    /// `ntp_timestamp` (NTP 64-bit format).
    pub ntp_timestamp: u64,
    /// `media_time` (32-bit in v0, 64-bit in v1).
    pub media_time: u64,
}

/// An `ssix` box (§8.16.4): per subsegment, `(level, range_size)` runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubsegmentIndex {
    /// One `Vec<(level, range_size)>` per subsegment.
    pub subsegments: Vec<Vec<(u8, u32)>>,
}

/// Upper bound on sample-group runs / description entries per box.
pub const MAX_GROUP_ENTRIES: usize = 1 << 20;

/// One track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    /// `track_ID`.
    pub track_id: u32,
    /// `track_enabled` (tkhd flag 1).
    pub enabled: bool,
    /// `track_in_movie` (tkhd flag 2).
    pub in_movie: bool,
    /// `handler_type` (`pict`, `vide`, `auxv`, …).
    pub handler: FourCc,
    /// Media timescale (`mdhd`).
    pub timescale: u32,
    /// Media duration (`mdhd`).
    pub duration: u64,
    /// Presentation width (`tkhd`, 16.16 → integer part).
    pub width: u32,
    /// Presentation height (`tkhd`, 16.16 → integer part).
    pub height: u32,
    /// `tkhd` matrix, file order (`a b u c d v x y w`).
    pub matrix: [i32; 9],
    /// `stsd` entries.
    pub sample_entries: Vec<SampleEntry>,
    /// The samples in decode order.
    pub samples: Vec<Sample>,
    /// `tref` references `(type, track ids)`.
    pub references: Vec<(FourCc, Vec<u32>)>,
    /// `elst` entries.
    pub edits: Vec<Edit>,
    /// `sbgp` / `csgp` sample-to-group mappings of the `stbl`, in file order.
    pub sample_groups: Vec<SampleGroup>,
    /// `sgpd` group descriptions of the `stbl`, in file order.
    pub sample_group_descriptions: Vec<SampleGroupDescription>,
}

impl Track {
    /// The 1-based `sgpd` entry index that applies to sample `index`
    /// for `grouping_type` (and `parameter`, when the mapping carries
    /// one): the `sbgp` / `csgp` mapping, else the `sgpd` default;
    /// `None` when neither maps the sample (or maps it to 0).
    pub fn group_description_index(
        &self,
        grouping_type: &FourCc,
        parameter: Option<u32>,
        index: usize,
    ) -> Option<u32> {
        let mapped = self
            .sample_groups
            .iter()
            .filter(|g| {
                &g.grouping_type == grouping_type
                    && (parameter.is_none() || g.grouping_type_parameter == parameter)
            })
            .find_map(|g| g.index_of(index));
        let idx = match mapped {
            Some(i) => i,
            None => self
                .sample_group_descriptions
                .iter()
                .find(|d| &d.grouping_type == grouping_type)
                .map(|d| d.default_group_description_index)
                .unwrap_or(0),
        };
        (idx & 0x7fff_ffff != 0).then_some(idx)
    }

    /// The raw `sgpd` entry that applies to sample `index` for
    /// `grouping_type` (see [`Track::group_description_index`]).
    pub fn group_description_of(
        &self,
        grouping_type: &FourCc,
        parameter: Option<u32>,
        index: usize,
    ) -> Option<&[u8]> {
        let idx = self.group_description_index(grouping_type, parameter, index)? & 0x7fff_ffff;
        self.sample_group_descriptions
            .iter()
            .find(|d| &d.grouping_type == grouping_type)?
            .entries
            .get(idx as usize - 1)
            .map(Vec::as_slice)
    }

    /// `true` for image-sequence / video / auxiliary-video tracks.
    pub fn is_visual(&self) -> bool {
        matches!(&self.handler, b"pict" | b"vide" | b"auxv")
    }

    /// The first sample entry.
    pub fn primary_entry(&self) -> Option<&SampleEntry> {
        self.sample_entries.first()
    }

    /// The auxiliary kind announced by the first sample entry's `auxi`
    /// URN (alpha / depth / other), `None` for a non-auxiliary track.
    pub fn aux_kind(&self) -> Option<crate::props::AuxKind> {
        let urn = self.primary_entry()?.aux_track_type.as_deref()?;
        Some(
            crate::props::AuxC {
                aux_type: urn.to_string(),
                aux_subtype: Vec::new(),
            }
            .kind(),
        )
    }

    /// Index of the sample that is time-parallel to decode time `dts`
    /// of another track with `timescale` (HEIF §7.5.3.1: the sample of
    /// this track whose time span covers that instant — the last one
    /// starting at or before it).
    pub fn sample_index_at(&self, dts: u64, timescale: u32) -> Option<usize> {
        let here = self.timescale.max(1) as u128;
        let there = timescale.max(1) as u128;
        let target = dts as u128 * here;
        let mut best = None;
        for (i, s) in self.samples.iter().enumerate() {
            if s.dts as u128 * there <= target {
                best = Some(i);
            } else {
                break;
            }
        }
        best
    }

    /// Tracks this one references with `reference_type`.
    pub fn references_of(&self, reference_type: &FourCc) -> Vec<u32> {
        self.references
            .iter()
            .filter(|(t, _)| t == reference_type)
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect()
    }

    /// §7.2.1: decode the matrix into rotation / mirror when it is one
    /// of the permitted combinations; `None` for any other matrix.
    pub fn orientation(&self) -> Option<TrackOrientation> {
        const ONE: i32 = 0x0001_0000;
        let [a, b, _u, c, d, _v, _x, _y, w] = self.matrix;
        if w != 0x4000_0000 {
            return None;
        }
        // Rotation matrices (a b; c d) for 0/90/180/270 anti-clockwise
        // in the ISOBMFF (x, y) → (x·a + y·c, x·b + y·d) convention, and
        // their horizontally mirrored variants.
        let table = [
            ([ONE, 0, 0, ONE], 0u8, false),
            ([0, -ONE, ONE, 0], 1, false),
            ([-ONE, 0, 0, -ONE], 2, false),
            ([0, ONE, -ONE, 0], 3, false),
            ([-ONE, 0, 0, ONE], 0, true),
            ([0, ONE, ONE, 0], 1, true),
            ([ONE, 0, 0, -ONE], 2, true),
            ([0, -ONE, -ONE, 0], 3, true),
        ];
        table
            .iter()
            .find(|(m, _, _)| *m == [a, b, c, d])
            .map(|(_, rotation, mirror)| TrackOrientation {
                rotation: *rotation,
                mirror: *mirror,
            })
    }

    /// Total duration in the media timescale from the sample table.
    pub fn sample_duration_total(&self) -> u64 {
        self.samples
            .iter()
            .fold(0u64, |acc, s| acc.saturating_add(s.duration as u64))
    }
}

/// The `moov` box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Movie {
    /// `mvhd` timescale.
    pub timescale: u32,
    /// `mvhd` duration.
    pub duration: u64,
    /// The tracks, in file order.
    pub tracks: Vec<Track>,
    /// Top-level `prft` boxes, in file order.
    pub producer_reference_times: Vec<ProducerReferenceTime>,
    /// Top-level `ssix` boxes, in file order.
    pub subsegment_indexes: Vec<SubsegmentIndex>,
}

impl Movie {
    /// Tracks with a visual handler.
    pub fn visual_tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| t.is_visual())
    }

    /// Look up a track by id.
    pub fn track(&self, id: u32) -> Option<&Track> {
        self.tracks.iter().find(|t| t.track_id == id)
    }

    /// The auxiliary tracks of `track_id` (HEIF §7.5.3.1: linked by an
    /// `auxl` track reference from the auxiliary track), in file order.
    pub fn auxiliary_tracks_of(&self, track_id: u32) -> Vec<&Track> {
        self.tracks
            .iter()
            .filter(|t| t.track_id != track_id && t.references_of(b"auxl").contains(&track_id))
            .collect()
    }

    /// The alpha auxiliary track of `track_id`, when one exists.
    pub fn alpha_track_of(&self, track_id: u32) -> Option<&Track> {
        self.auxiliary_tracks_of(track_id)
            .into_iter()
            .find(|t| t.aux_kind() == Some(crate::props::AuxKind::Alpha))
    }
}

/// Parse the movie box of a file, `Ok(None)` when there is none.
pub fn parse_movie(file: &HeifFile) -> Result<Option<Movie>> {
    let Some(moov) = file.top_level_payload(b"moov") else {
        return Ok(None);
    };
    let file_len = file.bytes().len() as u64;
    let (mvhd_h, mvhd) =
        find_box(moov, b"mvhd")?.ok_or_else(|| HeifError::invalid("moov without mvhd"))?;
    let _ = mvhd_h;
    let (timescale, duration) = parse_mvhd(mvhd)?;
    let mut tracks = Vec::new();
    for h in iter_boxes(moov) {
        let h = h?;
        if &h.box_type != b"trak" {
            continue;
        }
        if tracks.len() >= MAX_TRACKS {
            return Err(HeifError::exhausted(format!(
                "more than {MAX_TRACKS} tracks"
            )));
        }
        tracks.push(parse_trak(payload(moov, &h), file_len)?);
    }
    let mut producer_reference_times = Vec::new();
    let mut subsegment_indexes = Vec::new();
    for h in &file.top_level {
        let p = crate::boxes::payload(file.bytes(), h);
        match &h.box_type {
            b"prft" => producer_reference_times.push(parse_prft(p)?),
            b"ssix" => subsegment_indexes.push(parse_ssix(p)?),
            _ => {}
        }
    }
    Ok(Some(Movie {
        timescale,
        duration,
        tracks,
        producer_reference_times,
        subsegment_indexes,
    }))
}

/// `prft` (§8.16.5.2).
pub fn parse_prft(p: &[u8]) -> Result<ProducerReferenceTime> {
    let (v, flags, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    let reference_track_id = r.u32("prft reference_track_ID")?;
    let ntp_timestamp = r.u64("prft ntp_timestamp")?;
    let media_time = if v == 0 {
        r.u32("prft media_time")? as u64
    } else {
        r.u64("prft media_time")?
    };
    Ok(ProducerReferenceTime {
        flags,
        reference_track_id,
        ntp_timestamp,
        media_time,
    })
}

/// `ssix` (§8.16.4.2).
pub fn parse_ssix(p: &[u8]) -> Result<SubsegmentIndex> {
    let (_v, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    let n = r.u32("ssix subsegment_count")? as usize;
    if n > r.remaining() / 4 {
        return Err(HeifError::invalid("ssix subsegment_count exceeds the box"));
    }
    let mut subsegments = Vec::with_capacity(n);
    for _ in 0..n {
        let rc = r.u32("ssix range_count")? as usize;
        if rc > r.remaining() / 4 {
            return Err(HeifError::invalid("ssix range_count exceeds the box"));
        }
        let mut ranges = Vec::with_capacity(rc);
        for _ in 0..rc {
            let w = r.u32("ssix level / range_size")?;
            ranges.push(((w >> 24) as u8, w & 0x00ff_ffff));
        }
        subsegments.push(ranges);
    }
    Ok(SubsegmentIndex { subsegments })
}

/// `sbgp` (§8.9.2.2).
pub fn parse_sbgp(p: &[u8]) -> Result<SampleGroup> {
    let (v, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    let grouping_type = r.fourcc("sbgp grouping_type")?;
    let grouping_type_parameter = if v == 1 {
        Some(r.u32("sbgp grouping_type_parameter")?)
    } else {
        None
    };
    let n = r.u32("sbgp entry_count")? as usize;
    if n > r.remaining() / 8 || n > MAX_GROUP_ENTRIES {
        return Err(HeifError::invalid("sbgp entry_count exceeds the box"));
    }
    let mut entries = Vec::with_capacity(n);
    for _ in 0..n {
        let c = r.u32("sbgp sample_count")?;
        let g = r.u32("sbgp group_description_index")?;
        entries.push((c, g));
    }
    Ok(SampleGroup {
        grouping_type,
        grouping_type_parameter,
        entries,
        compact: false,
    })
}

/// `csgp` (§8.9.5): the pattern table is expanded to `sbgp`-shaped
/// runs (one run per pattern element, repeated `sample_count /
/// pattern_length` times, the remainder taken from the pattern head).
pub fn parse_csgp(p: &[u8]) -> Result<SampleGroup> {
    let (_v, flags, body) = parse_full_box(p)?;
    let fragment_local_msb = flags & 0x80 != 0;
    let parameter_present = flags & 0x40 != 0;
    let width = |code: u32| 4u32 << (code & 3);
    let pattern_bits = width(flags >> 4);
    let count_bits = width(flags >> 2);
    let index_bits = width(flags);
    let mut r = Reader::new(body);
    let grouping_type = r.fourcc("csgp grouping_type")?;
    let grouping_type_parameter = if parameter_present {
        Some(r.u32("csgp grouping_type_parameter")?)
    } else {
        None
    };
    let pattern_count = r.u32("csgp pattern_count")? as usize;
    let rest = r.rest();
    let mut bits = BitCursor { data: rest, pos: 0 };
    if pattern_count as u64 * (pattern_bits + count_bits) as u64 > rest.len() as u64 * 8
        || pattern_count > MAX_GROUP_ENTRIES
    {
        return Err(HeifError::invalid("csgp pattern_count exceeds the box"));
    }
    let mut patterns = Vec::with_capacity(pattern_count);
    let mut total_len = 0u64;
    for _ in 0..pattern_count {
        let len = bits.read(pattern_bits, "csgp pattern_length")?;
        let count = bits.read(count_bits, "csgp sample_count")?;
        total_len += len as u64;
        patterns.push((len, count));
    }
    if total_len * index_bits as u64 > (rest.len() as u64 * 8).saturating_sub(bits.pos as u64)
        || total_len > MAX_GROUP_ENTRIES as u64
    {
        return Err(HeifError::invalid("csgp pattern indices exceed the box"));
    }
    let mut entries = Vec::new();
    for (len, count) in patterns {
        let mut indices = Vec::with_capacity(len as usize);
        for _ in 0..len {
            let mut idx = bits.read(index_bits, "csgp sample_group_description_index")?;
            if fragment_local_msb && index_bits < 32 && idx & (1 << (index_bits - 1)) != 0 {
                idx = (idx & !(1 << (index_bits - 1))) | 0x8000_0000;
            }
            indices.push(idx);
        }
        if len == 0 {
            continue;
        }
        // `sample_count` samples follow the pattern cyclically.
        let mut remaining = count;
        while remaining > 0 {
            for &idx in &indices {
                if remaining == 0 {
                    break;
                }
                match entries.last_mut() {
                    Some((c, g)) if *g == idx => *c += 1,
                    _ => entries.push((1u32, idx)),
                }
                remaining -= 1;
            }
            if entries.len() > MAX_GROUP_ENTRIES {
                return Err(HeifError::exhausted("csgp expands past the run cap"));
            }
        }
    }
    Ok(SampleGroup {
        grouping_type,
        grouping_type_parameter,
        entries,
        compact: true,
    })
}

/// `sgpd` (§8.9.3.2).
pub fn parse_sgpd(p: &[u8]) -> Result<SampleGroupDescription> {
    let (v, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    let grouping_type = r.fourcc("sgpd grouping_type")?;
    let default_length = if v >= 1 {
        r.u32("sgpd default_length")?
    } else {
        0
    };
    let default_group_description_index = if v >= 2 {
        r.u32("sgpd default_group_description_index")?
    } else {
        0
    };
    let n = r.u32("sgpd entry_count")? as usize;
    if n > r.remaining() || n > MAX_GROUP_ENTRIES {
        return Err(HeifError::invalid("sgpd entry_count exceeds the box"));
    }
    let mut entries = Vec::with_capacity(n);
    for _ in 0..n {
        let len = if v >= 1 {
            if default_length == 0 {
                r.u32("sgpd description_length")? as usize
            } else {
                default_length as usize
            }
        } else {
            // v0 entries have no length field: the layout is per
            // grouping type; the rest of the box is kept as one entry.
            r.remaining()
        };
        entries.push(r.bytes(len, "sgpd entry")?.to_vec());
        if v == 0 {
            break;
        }
    }
    Ok(SampleGroupDescription {
        grouping_type,
        version: v,
        default_length,
        default_group_description_index,
        entries,
    })
}

/// MSB-first bit reader for the `csgp` packed fields.
struct BitCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitCursor<'_> {
    fn read(&mut self, bits: u32, what: &str) -> Result<u32> {
        let mut v = 0u64;
        for _ in 0..bits {
            let byte = self
                .data
                .get(self.pos / 8)
                .ok_or_else(|| HeifError::invalid(format!("{what}: truncated")))?;
            v = (v << 1) | ((byte >> (7 - self.pos % 8)) & 1) as u64;
            self.pos += 1;
        }
        Ok(v as u32)
    }
}

fn parse_mvhd(p: &[u8]) -> Result<(u32, u64)> {
    let (v, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    if v == 1 {
        r.skip(16, "mvhd times")?;
        let ts = r.u32("mvhd timescale")?;
        let d = r.u64("mvhd duration")?;
        Ok((ts, d))
    } else {
        r.skip(8, "mvhd times")?;
        let ts = r.u32("mvhd timescale")?;
        let d = r.u32("mvhd duration")? as u64;
        Ok((ts, d))
    }
}

struct Tkhd {
    track_id: u32,
    flags: u32,
    width: u32,
    height: u32,
    matrix: [i32; 9],
}

fn parse_tkhd(p: &[u8]) -> Result<Tkhd> {
    let (v, flags, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    let track_id = if v == 1 {
        r.skip(16, "tkhd times")?;
        let id = r.u32("tkhd track_ID")?;
        r.skip(4, "tkhd reserved")?;
        r.skip(8, "tkhd duration")?;
        id
    } else {
        r.skip(8, "tkhd times")?;
        let id = r.u32("tkhd track_ID")?;
        r.skip(4, "tkhd reserved")?;
        r.skip(4, "tkhd duration")?;
        id
    };
    r.skip(8, "tkhd reserved")?;
    r.skip(2, "tkhd layer")?;
    r.skip(2, "tkhd alternate_group")?;
    r.skip(2, "tkhd volume")?;
    r.skip(2, "tkhd reserved")?;
    let mut matrix = [0i32; 9];
    for m in matrix.iter_mut() {
        *m = r.i32("tkhd matrix")?;
    }
    let width = r.u32("tkhd width")? >> 16;
    let height = r.u32("tkhd height")? >> 16;
    Ok(Tkhd {
        track_id,
        flags,
        width,
        height,
        matrix,
    })
}

fn parse_mdhd(p: &[u8]) -> Result<(u32, u64)> {
    let (v, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    if v == 1 {
        r.skip(16, "mdhd times")?;
        let ts = r.u32("mdhd timescale")?;
        let d = r.u64("mdhd duration")?;
        Ok((ts, d))
    } else {
        r.skip(8, "mdhd times")?;
        let ts = r.u32("mdhd timescale")?;
        let d = r.u32("mdhd duration")? as u64;
        Ok((ts, d))
    }
}

fn parse_hdlr_type(p: &[u8]) -> Result<FourCc> {
    let (_v, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    r.skip(4, "hdlr pre_defined")?;
    r.fourcc("hdlr handler_type")
}

fn parse_trak(trak: &[u8], file_len: u64) -> Result<Track> {
    let (_, tkhd) =
        find_box(trak, b"tkhd")?.ok_or_else(|| HeifError::invalid("trak without tkhd"))?;
    let tk = parse_tkhd(tkhd)?;
    let (_, mdia) =
        find_box(trak, b"mdia")?.ok_or_else(|| HeifError::invalid("trak without mdia"))?;
    let (_, mdhd) =
        find_box(mdia, b"mdhd")?.ok_or_else(|| HeifError::invalid("mdia without mdhd"))?;
    let (timescale, duration) = parse_mdhd(mdhd)?;
    let (_, hdlr) =
        find_box(mdia, b"hdlr")?.ok_or_else(|| HeifError::invalid("mdia without hdlr"))?;
    let handler = parse_hdlr_type(hdlr)?;
    let (_, minf) =
        find_box(mdia, b"minf")?.ok_or_else(|| HeifError::invalid("mdia without minf"))?;
    let (_, stbl) =
        find_box(minf, b"stbl")?.ok_or_else(|| HeifError::invalid("minf without stbl"))?;
    let sample_entries = match find_box(stbl, b"stsd")? {
        Some((_, stsd)) => parse_stsd(stsd)?,
        None => Vec::new(),
    };
    let samples = parse_sample_table(stbl, file_len)?;
    let mut sample_groups = Vec::new();
    let mut sample_group_descriptions = Vec::new();
    for h in iter_boxes(stbl) {
        let h = h?;
        let p = payload(stbl, &h);
        match &h.box_type {
            b"sbgp" => sample_groups.push(parse_sbgp(p)?),
            b"csgp" => sample_groups.push(parse_csgp(p)?),
            b"sgpd" => sample_group_descriptions.push(parse_sgpd(p)?),
            _ => {}
        }
        if sample_groups.len() + sample_group_descriptions.len() > 4096 {
            return Err(HeifError::exhausted("more than 4096 sample-group boxes"));
        }
    }
    let mut references = Vec::new();
    if let Some((_, tref)) = find_box(trak, b"tref")? {
        for h in iter_boxes(tref) {
            let h = h?;
            let mut r = Reader::new(payload(tref, &h));
            let mut ids = Vec::with_capacity(r.remaining() / 4);
            while r.remaining() >= 4 {
                ids.push(r.u32("tref track_ID")?);
            }
            references.push((h.box_type, ids));
        }
    }
    let mut edits = Vec::new();
    if let Some((_, edts)) = find_box(trak, b"edts")? {
        if let Some((_, elst)) = find_box(edts, b"elst")? {
            let (v, _f, body) = parse_full_box(elst)?;
            let mut r = Reader::new(body);
            let n = r.u32("elst entry_count")? as usize;
            for _ in 0..n {
                let (segment_duration, media_time) = if v == 1 {
                    (
                        r.u64("elst segment_duration")?,
                        r.u64("elst media_time")? as i64,
                    )
                } else {
                    (
                        r.u32("elst segment_duration")? as u64,
                        r.u32("elst media_time")? as i32 as i64,
                    )
                };
                let media_rate = r.i32("elst media_rate")?;
                edits.push(Edit {
                    segment_duration,
                    media_time,
                    media_rate,
                });
            }
        }
    }
    Ok(Track {
        track_id: tk.track_id,
        enabled: tk.flags & 1 == 1,
        in_movie: tk.flags & 2 == 2,
        handler,
        timescale,
        duration,
        width: tk.width,
        height: tk.height,
        matrix: tk.matrix,
        sample_entries,
        samples,
        references,
        edits,
        sample_groups,
        sample_group_descriptions,
    })
}

fn parse_stsd(p: &[u8]) -> Result<Vec<SampleEntry>> {
    let (_v, _f, body) = parse_full_box(p)?;
    let mut r = Reader::new(body);
    let n = r.u32("stsd entry_count")? as usize;
    let entries = r.rest();
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while out.len() < n && cursor < entries.len() {
        let h = crate::boxes::parse_box_header(entries, cursor)?;
        out.push(parse_visual_entry(h.box_type, payload(entries, &h))?);
        cursor = h.end();
    }
    Ok(out)
}

/// VisualSampleEntry: SampleEntry (6 reserved + data_reference_index)
/// then pre_defined(2) reserved(2) pre_defined(12) width(2) height(2)
/// horizresolution(4) vertresolution(4) reserved(4) frame_count(2)
/// compressorname(32) depth(2) pre_defined(2) — 78 bytes, then children.
fn parse_visual_entry(entry_type: FourCc, p: &[u8]) -> Result<SampleEntry> {
    let mut r = Reader::new(p);
    r.skip(6, "SampleEntry reserved")?;
    let data_reference_index = r.u16("SampleEntry data_reference_index")?;
    r.skip(16, "VisualSampleEntry pre_defined")?;
    let width = r.u16("VisualSampleEntry width")?;
    let height = r.u16("VisualSampleEntry height")?;
    r.skip(8, "VisualSampleEntry resolution")?;
    r.skip(4, "VisualSampleEntry reserved")?;
    r.skip(2, "VisualSampleEntry frame_count")?;
    r.skip(32, "VisualSampleEntry compressorname")?;
    r.skip(2, "VisualSampleEntry depth")?;
    r.skip(2, "VisualSampleEntry pre_defined")?;
    let rest = r.rest();
    let mut entry = SampleEntry {
        entry_type,
        data_reference_index,
        width,
        height,
        hvcc: None,
        av1c: None,
        avcc: None,
        lhvc: None,
        ccst: None,
        aux_track_type: None,
        colr: Vec::new(),
        clap: None,
        pasp: None,
        clli: None,
        mdcv: None,
        cclv: None,
        amve: None,
        children: Vec::new(),
    };
    for h in iter_boxes(rest) {
        let h = h?;
        let body = payload(rest, &h);
        let raw = RawProperty {
            box_type: h.box_type,
            user_type: h.user_type,
            body: body.to_vec(),
            box_size: h.total_len(),
        };
        match &h.box_type {
            b"hvcC" => entry.hvcc = Some(HevcConfig::parse(body)?),
            b"av1C" => entry.av1c = Some(Av1Config::parse(body)?),
            b"avcC" => entry.avcc = Some(crate::avcc::AvcConfig::parse(body)?),
            b"lhvC" => entry.lhvc = Some(crate::lhvc::LhevcConfig::parse(body)?),
            b"ccst" => {
                let (_v, _f, b) = parse_full_box(body)?;
                let mut cr = Reader::new(b);
                let w = cr.u32("ccst")?;
                entry.ccst = Some(CodingConstraints {
                    all_ref_pics_intra: w >> 31 == 1,
                    intra_pred_used: (w >> 30) & 1 == 1,
                    max_ref_per_pic: ((w >> 26) & 0x0f) as u8,
                });
            }
            b"auxi" => {
                // HEIF §7.5.3.3: a FullBox holding the URN. Apple ImageIO
                // writes the string directly after the box header (no
                // version / flags); a body whose first byte is printable
                // is read that way.
                let b = match body.first() {
                    Some(c) if c.is_ascii_graphic() => body,
                    _ => parse_full_box(body)?.2,
                };
                let mut cr = Reader::new(b);
                entry.aux_track_type = Some(cr.cstr("auxi aux_track_type")?);
            }
            b"colr" => {
                if let Property::Colr(c) = Property::parse(&raw)? {
                    entry.colr.push(c);
                }
            }
            b"clap" => {
                if let Property::Clap(c) = Property::parse(&raw)? {
                    entry.clap = Some(c);
                }
            }
            b"pasp" => {
                if let Property::Pasp(c) = Property::parse(&raw)? {
                    entry.pasp = Some(c);
                }
            }
            b"clli" | b"mdcv" | b"cclv" | b"amve" => match Property::parse(&raw)? {
                Property::Clli(v) => entry.clli = Some(v),
                Property::Mdcv(v) => entry.mdcv = Some(v),
                Property::Cclv(v) => entry.cclv = Some(v),
                Property::Amve(v) => entry.amve = Some(v),
                _ => {}
            },
            _ => {}
        }
        entry.children.push(raw);
    }
    Ok(entry)
}

fn parse_sample_table(stbl: &[u8], file_len: u64) -> Result<Vec<Sample>> {
    let mut stts = None;
    let mut ctts = None;
    let mut stsc = None;
    let mut stsz = None;
    let mut stz2 = None;
    let mut stco = None;
    let mut co64 = None;
    let mut stss = None;
    for h in iter_boxes(stbl) {
        let h = h?;
        let p = payload(stbl, &h);
        match &h.box_type {
            b"stts" => stts = Some(p),
            b"ctts" => ctts = Some(p),
            b"stsc" => stsc = Some(p),
            b"stsz" => stsz = Some(p),
            b"stz2" => stz2 = Some(p),
            b"stco" => stco = Some(p),
            b"co64" => co64 = Some(p),
            b"stss" => stss = Some(p),
            _ => {}
        }
    }
    let stts = stts.ok_or_else(|| HeifError::invalid("stbl without stts"))?;
    let stsc = stsc.ok_or_else(|| HeifError::invalid("stbl without stsc"))?;
    // Sizes.
    let sizes: SampleSizes = if let Some(p) = stsz {
        let (_v, _f, body) = parse_full_box(p)?;
        let mut r = Reader::new(body);
        let sample_size = r.u32("stsz sample_size")?;
        let count = r.u32("stsz sample_count")? as usize;
        if count > MAX_SAMPLES {
            return Err(HeifError::exhausted(format!(
                "stsz declares {count} samples"
            )));
        }
        if sample_size != 0 {
            SampleSizes::Fixed(sample_size, count)
        } else {
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                v.push(r.u32("stsz entry_size")?);
            }
            SampleSizes::Table(v)
        }
    } else if let Some(p) = stz2 {
        let (_v, _f, body) = parse_full_box(p)?;
        let mut r = Reader::new(body);
        r.skip(3, "stz2 reserved")?;
        let field_size = r.u8("stz2 field_size")?;
        let count = r.u32("stz2 sample_count")? as usize;
        if count > MAX_SAMPLES {
            return Err(HeifError::exhausted(format!(
                "stz2 declares {count} samples"
            )));
        }
        let mut v = Vec::with_capacity(count);
        match field_size {
            4 => {
                let bytes = r.bytes(count.div_ceil(2), "stz2 entries")?;
                for i in 0..count {
                    let b = bytes[i / 2];
                    v.push(if i % 2 == 0 {
                        (b >> 4) as u32
                    } else {
                        (b & 0x0f) as u32
                    });
                }
            }
            8 => {
                for _ in 0..count {
                    v.push(r.u8("stz2 entry")? as u32);
                }
            }
            16 => {
                for _ in 0..count {
                    v.push(r.u16("stz2 entry")? as u32);
                }
            }
            f => return Err(HeifError::invalid(format!("stz2 field_size {f}"))),
        }
        SampleSizes::Table(v)
    } else {
        return Err(HeifError::invalid("stbl without stsz / stz2"));
    };
    let sample_count = sizes.count();
    // Chunk offsets.
    let chunk_offsets: Vec<u64> = if let Some(p) = stco {
        let (_v, _f, body) = parse_full_box(p)?;
        let mut r = Reader::new(body);
        let n = r.u32("stco entry_count")? as usize;
        if n > r.remaining() / 4 {
            return Err(HeifError::invalid("stco entry_count exceeds the box"));
        }
        (0..n)
            .map(|_| r.u32("stco chunk_offset").map(u64::from))
            .collect::<Result<_>>()?
    } else if let Some(p) = co64 {
        let (_v, _f, body) = parse_full_box(p)?;
        let mut r = Reader::new(body);
        let n = r.u32("co64 entry_count")? as usize;
        if n > r.remaining() / 8 {
            return Err(HeifError::invalid("co64 entry_count exceeds the box"));
        }
        (0..n)
            .map(|_| r.u64("co64 chunk_offset"))
            .collect::<Result<_>>()?
    } else {
        return Err(HeifError::invalid("stbl without stco / co64"));
    };
    // Sample-to-chunk.
    let (_v, _f, body) = parse_full_box(stsc)?;
    let mut r = Reader::new(body);
    let n = r.u32("stsc entry_count")? as usize;
    if n > r.remaining() / 12 {
        return Err(HeifError::invalid("stsc entry_count exceeds the box"));
    }
    let mut stsc_entries = Vec::with_capacity(n);
    for _ in 0..n {
        let first_chunk = r.u32("stsc first_chunk")?;
        let samples_per_chunk = r.u32("stsc samples_per_chunk")?;
        let desc = r.u32("stsc sample_description_index")?;
        stsc_entries.push((first_chunk, samples_per_chunk, desc));
    }
    // Decode-time deltas.
    let (_v, _f, body) = parse_full_box(stts)?;
    let mut r = Reader::new(body);
    let n = r.u32("stts entry_count")? as usize;
    if n > r.remaining() / 8 {
        return Err(HeifError::invalid("stts entry_count exceeds the box"));
    }
    let mut durations: Vec<u32> = Vec::with_capacity(sample_count);
    for _ in 0..n {
        let count = r.u32("stts sample_count")? as usize;
        let delta = r.u32("stts sample_delta")?;
        if durations.len() + count > MAX_SAMPLES {
            return Err(HeifError::exhausted("stts expands past the sample cap"));
        }
        durations.extend(std::iter::repeat(delta).take(count));
    }
    // Composition offsets.
    let mut cts_offsets: Vec<i64> = Vec::new();
    if let Some(p) = ctts {
        let (v, _f, body) = parse_full_box(p)?;
        let mut r = Reader::new(body);
        let n = r.u32("ctts entry_count")? as usize;
        if n > r.remaining() / 8 {
            return Err(HeifError::invalid("ctts entry_count exceeds the box"));
        }
        for _ in 0..n {
            let count = r.u32("ctts sample_count")? as usize;
            let off = if v == 1 {
                r.i32("ctts sample_offset")? as i64
            } else {
                r.u32("ctts sample_offset")? as i64
            };
            if cts_offsets.len() + count > MAX_SAMPLES {
                return Err(HeifError::exhausted("ctts expands past the sample cap"));
            }
            cts_offsets.extend(std::iter::repeat(off).take(count));
        }
    }
    // Sync samples.
    let sync: Option<Vec<u32>> = match stss {
        Some(p) => {
            let (_v, _f, body) = parse_full_box(p)?;
            let mut r = Reader::new(body);
            let n = r.u32("stss entry_count")? as usize;
            if n > r.remaining() / 4 {
                return Err(HeifError::invalid("stss entry_count exceeds the box"));
            }
            let mut v: Vec<u32> = (0..n)
                .map(|_| r.u32("stss sample_number"))
                .collect::<Result<_>>()?;
            v.sort_unstable();
            Some(v)
        }
        None => None,
    };
    // Expand chunks.
    let mut out = Vec::with_capacity(sample_count);
    let mut dts: u64 = 0;
    let mut sample_idx = 0usize;
    for (ci, e) in stsc_entries.iter().enumerate() {
        let first = e.0 as usize;
        if first == 0 || first > chunk_offsets.len() {
            return Err(HeifError::invalid(format!(
                "stsc entry {ci}: first_chunk {first} outside the {} chunks",
                chunk_offsets.len()
            )));
        }
        let last = match stsc_entries.get(ci + 1) {
            Some(next) => {
                let nf = next.0 as usize;
                if nf <= first {
                    return Err(HeifError::invalid("stsc first_chunk not increasing"));
                }
                nf - 1
            }
            None => chunk_offsets.len(),
        };
        for chunk in first..=last {
            let mut off = chunk_offsets[chunk - 1];
            for _ in 0..e.1 {
                if sample_idx >= sample_count {
                    break;
                }
                let size = sizes.get(sample_idx);
                let end = off
                    .checked_add(size as u64)
                    .ok_or_else(|| HeifError::invalid("sample offset overflow"))?;
                if end > file_len {
                    return Err(HeifError::invalid(format!(
                        "sample {sample_idx} [{off}, {end}) past the {file_len}-byte file"
                    )));
                }
                let duration = durations.get(sample_idx).copied().unwrap_or(0);
                out.push(Sample {
                    offset: off,
                    size,
                    dts,
                    cts_offset: cts_offsets.get(sample_idx).copied().unwrap_or(0),
                    duration,
                    is_sync: sync
                        .as_ref()
                        .map(|s| s.binary_search(&(sample_idx as u32 + 1)).is_ok())
                        .unwrap_or(true),
                    description_index: e.2,
                });
                dts = dts.saturating_add(duration as u64);
                off = end;
                sample_idx += 1;
            }
        }
    }
    if sample_idx != sample_count {
        return Err(HeifError::invalid(format!(
            "sample table describes {sample_idx} samples but stsz declares {sample_count}"
        )));
    }
    Ok(out)
}

enum SampleSizes {
    Fixed(u32, usize),
    Table(Vec<u32>),
}

impl SampleSizes {
    fn count(&self) -> usize {
        match self {
            SampleSizes::Fixed(_, n) => *n,
            SampleSizes::Table(v) => v.len(),
        }
    }

    fn get(&self, i: usize) -> u32 {
        match self {
            SampleSizes::Fixed(s, _) => *s,
            SampleSizes::Table(v) => v[i],
        }
    }
}

/// Bytes of a sample.
pub fn sample_bytes<'a>(file: &'a HeifFile, s: &Sample) -> Result<&'a [u8]> {
    let b = file.bytes();
    let end = s.offset as usize + s.size as usize;
    if end > b.len() {
        return Err(HeifError::invalid("sample outside the file"));
    }
    Ok(&b[s.offset as usize..end])
}

#[doc(hidden)]
/// Diagnostics helper: the entry type of a track's first sample entry.
pub fn entry_type_str(t: &Track) -> String {
    t.primary_entry()
        .map(|e| fourcc_str(&e.entry_type))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orientation_table() {
        let mut t = Track {
            track_id: 1,
            enabled: true,
            in_movie: true,
            handler: *b"pict",
            timescale: 1,
            duration: 0,
            width: 1,
            height: 1,
            matrix: [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000],
            sample_entries: vec![],
            samples: vec![],
            references: vec![],
            edits: vec![],
            sample_groups: vec![],
            sample_group_descriptions: vec![],
        };
        assert_eq!(
            t.orientation(),
            Some(TrackOrientation {
                rotation: 0,
                mirror: false
            })
        );
        t.matrix = [0, -0x10000, 0, 0x10000, 0, 0, 0, 0, 0x40000000];
        assert_eq!(t.orientation().unwrap().rotation, 1);
        t.matrix = [-0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000];
        assert!(t.orientation().unwrap().mirror);
        t.matrix = [0x20000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000];
        assert_eq!(t.orientation(), None);
        assert!(t.is_visual());
    }

    #[test]
    fn sample_groups_sbgp_csgp_sgpd() {
        use crate::boxes::write::full_boxed;
        // sbgp v1: 2 samples → 1, 3 samples → 0, 1 sample → 2.
        let mut b = b"eqiv".to_vec();
        b.extend_from_slice(&7u32.to_be_bytes());
        b.extend_from_slice(&3u32.to_be_bytes());
        for (c, g) in [(2u32, 1u32), (3, 0), (1, 2)] {
            b.extend_from_slice(&c.to_be_bytes());
            b.extend_from_slice(&g.to_be_bytes());
        }
        let sbgp = full_boxed(b"sbgp", 1, 0, &b);
        let g = parse_sbgp(&sbgp[8..]).unwrap();
        assert_eq!(g.grouping_type_parameter, Some(7));
        assert_eq!(g.entries, vec![(2, 1), (3, 0), (1, 2)]);
        assert_eq!(g.index_of(0), Some(1));
        assert_eq!(g.index_of(2), Some(0));
        assert_eq!(g.index_of(5), Some(2));
        assert_eq!(g.index_of(6), None);
        // csgp: index 4-bit, count 8-bit, pattern-length 4-bit
        // (flags = 0 | 1<<2 | 0<<4 = 0x04), no parameter. Pattern
        // [1, 2] of length 2 applied to 5 samples, then pattern [3]
        // (length 1) to 2 samples.
        let mut c = b"eqiv".to_vec();
        c.extend_from_slice(&2u32.to_be_bytes());
        // pattern_length[1]=2 (4 bits), sample_count[1]=5 (8 bits),
        // pattern_length[2]=1, sample_count[2]=2 → bits:
        // 0010 00000101 0001 00000010 → 0x20 0x51 0x02 then indices
        // 0001 0010 0011 → 0x12 0x30
        c.extend_from_slice(&[0x20, 0x51, 0x02, 0x12, 0x30]);
        let csgp = full_boxed(b"csgp", 0, 0x04, &c);
        let g = parse_csgp(&csgp[8..]).unwrap();
        assert!(g.compact);
        assert_eq!(
            g.entries,
            vec![(1, 1), (1, 2), (1, 1), (1, 2), (1, 1), (2, 3)]
        );
        assert_eq!(g.index_of(4), Some(1));
        assert_eq!(g.index_of(6), Some(3));
        // sgpd v1 with default_length 0: two entries of 2 and 3 bytes;
        // v2 with a default index.
        let mut d = b"eqiv".to_vec();
        d.extend_from_slice(&0u32.to_be_bytes());
        d.extend_from_slice(&2u32.to_be_bytes());
        d.extend_from_slice(&2u32.to_be_bytes());
        d.extend_from_slice(&[0xaa, 0xbb]);
        d.extend_from_slice(&3u32.to_be_bytes());
        d.extend_from_slice(&[1, 2, 3]);
        let sgpd = full_boxed(b"sgpd", 1, 0, &d);
        let desc = parse_sgpd(&sgpd[8..]).unwrap();
        assert_eq!(desc.entries, vec![vec![0xaa, 0xbb], vec![1, 2, 3]]);
        let mut d2 = b"eqiv".to_vec();
        d2.extend_from_slice(&1u32.to_be_bytes()); // default_length 1
        d2.extend_from_slice(&2u32.to_be_bytes()); // default index 2
        d2.extend_from_slice(&2u32.to_be_bytes());
        d2.extend_from_slice(&[0x11, 0x22]);
        let desc2 = parse_sgpd(&full_boxed(b"sgpd", 2, 0, &d2)[8..]).unwrap();
        assert_eq!(desc2.default_group_description_index, 2);
        assert_eq!(desc2.entries, vec![vec![0x11], vec![0x22]]);
        // Track-level resolution: mapped samples use the mapping, the
        // rest fall back to the sgpd default.
        let t = Track {
            track_id: 1,
            enabled: true,
            in_movie: true,
            handler: *b"pict",
            timescale: 1,
            duration: 0,
            width: 1,
            height: 1,
            matrix: [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000],
            sample_entries: vec![],
            samples: vec![],
            references: vec![],
            edits: vec![],
            sample_groups: vec![parse_sbgp(&sbgp[8..]).unwrap()],
            sample_group_descriptions: vec![desc2],
        };
        assert_eq!(t.group_description_index(b"eqiv", None, 0), Some(1));
        assert_eq!(t.group_description_index(b"eqiv", Some(7), 0), Some(1));
        assert_eq!(t.group_description_index(b"eqiv", Some(8), 0), Some(2));
        assert_eq!(t.group_description_index(b"eqiv", None, 3), None);
        assert_eq!(t.group_description_index(b"eqiv", None, 9), Some(2));
        assert_eq!(t.group_description_of(b"eqiv", None, 9), Some(&[0x22][..]));
        assert_eq!(t.group_description_index(b"rap ", None, 0), None);
    }

    #[test]
    fn prft_and_ssix() {
        use crate::boxes::write::full_boxed;
        let mut b = 3u32.to_be_bytes().to_vec();
        b.extend_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
        b.extend_from_slice(&90_000u32.to_be_bytes());
        let p = parse_prft(&full_boxed(b"prft", 0, 24, &b)[8..]).unwrap();
        assert_eq!(
            p,
            ProducerReferenceTime {
                flags: 24,
                reference_track_id: 3,
                ntp_timestamp: 0x1122_3344_5566_7788,
                media_time: 90_000
            }
        );
        let mut b1 = 3u32.to_be_bytes().to_vec();
        b1.extend_from_slice(&1u64.to_be_bytes());
        b1.extend_from_slice(&(1u64 << 40).to_be_bytes());
        assert_eq!(
            parse_prft(&full_boxed(b"prft", 1, 0, &b1)[8..])
                .unwrap()
                .media_time,
            1 << 40
        );
        let mut s = 2u32.to_be_bytes().to_vec();
        s.extend_from_slice(&2u32.to_be_bytes());
        s.extend_from_slice(&[0, 0x00, 0x10, 0x00]);
        s.extend_from_slice(&[1, 0, 0, 0]);
        s.extend_from_slice(&1u32.to_be_bytes());
        s.extend_from_slice(&[7, 0xff, 0xff, 0xff]);
        let x = parse_ssix(&full_boxed(b"ssix", 0, 0, &s)[8..]).unwrap();
        assert_eq!(
            x.subsegments,
            vec![vec![(0, 0x1000), (1, 0)], vec![(7, 0xff_ffff)]]
        );
        assert!(parse_ssix(&full_boxed(b"ssix", 0, 0, &[0, 0, 0, 9])[8..]).is_err());
    }

    #[test]
    fn sample_pts_saturates() {
        let s = Sample {
            offset: 0,
            size: 0,
            dts: 2,
            cts_offset: -5,
            duration: 1,
            is_sync: true,
            description_index: 1,
        };
        assert_eq!(s.pts(), 0);
    }
}
