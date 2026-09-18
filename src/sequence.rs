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
use crate::props::{Clap, Colr, Pasp, Property};

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
}

impl Track {
    /// `true` for image-sequence / video / auxiliary-video tracks.
    pub fn is_visual(&self) -> bool {
        matches!(&self.handler, b"pict" | b"vide" | b"auxv")
    }

    /// The first sample entry.
    pub fn primary_entry(&self) -> Option<&SampleEntry> {
        self.sample_entries.first()
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
    Ok(Some(Movie {
        timescale,
        duration,
        tracks,
    }))
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
        ccst: None,
        aux_track_type: None,
        colr: Vec::new(),
        clap: None,
        pasp: None,
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
                let (_v, _f, b) = parse_full_box(body)?;
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
