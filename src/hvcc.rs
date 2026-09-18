//! `HEVCDecoderConfigurationRecord` (`hvcC`, ISO/IEC 14496-15 §8.3.3.1)
//! — the decoder configuration property of `hvc1` image items (HEIF
//! Annex B.2.3.1) and of `hvc1` sample entries in image sequences.
//!
//! ```text
//! unsigned int(8)  configurationVersion = 1;
//! unsigned int(2)  general_profile_space;
//! unsigned int(1)  general_tier_flag;
//! unsigned int(5)  general_profile_idc;
//! unsigned int(32) general_profile_compatibility_flags;
//! unsigned int(48) general_constraint_indicator_flags;
//! unsigned int(8)  general_level_idc;
//! bit(4) reserved = '1111'b; unsigned int(12) min_spatial_segmentation_idc;
//! bit(6) reserved = '111111'b; unsigned int(2) parallelismType;
//! bit(6) reserved = '111111'b; unsigned int(2) chroma_format_idc;
//! bit(5) reserved = '11111'b;  unsigned int(3) bit_depth_luma_minus8;
//! bit(5) reserved = '11111'b;  unsigned int(3) bit_depth_chroma_minus8;
//! unsigned int(16) avgFrameRate;
//! unsigned int(2)  constantFrameRate;
//! unsigned int(3)  numTemporalLayers;
//! unsigned int(1)  temporalIdNested;
//! unsigned int(2)  lengthSizeMinusOne;
//! unsigned int(8)  numOfArrays;
//! for (j = 0; j < numOfArrays; j++) {
//!   unsigned int(1)  array_completeness;
//!   bit(1) reserved = 0;
//!   unsigned int(6)  NAL_unit_type;
//!   unsigned int(16) numNalus;
//!   for (i = 0; i < numNalus; i++) {
//!     unsigned int(16) nalUnitLength;
//!     bit(8*nalUnitLength) nalUnit;
//!   }
//! }
//! ```
//!
//! This crate keeps the parse standalone (no dependency on the HEVC
//! decoder) so the container can be inspected without `registry`; with
//! `registry` on, the raw record bytes are handed to `oxideav-h265` as
//! the decoder's `extradata` (its documented `hvcC` contract) and the
//! item payload is passed through as length-prefixed NAL units.

use crate::boxes::Reader;
use crate::error::{HeifError, Result};

/// Fixed-size head of the record, before the NAL arrays.
pub const HEVC_CONFIG_HEAD_LEN: usize = 23;

/// One parameter-set / SEI array of the record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NalArray {
    /// `array_completeness`.
    pub complete: bool,
    /// The `bit(1) reserved` between `array_completeness` and
    /// `NAL_unit_type`; kept so a record re-serializes byte-exact even
    /// when a writer set it (some encoders emit `1`).
    pub reserved_bit: bool,
    /// `NAL_unit_type` (32 = VPS, 33 = SPS, 34 = PPS, 39 / 40 = SEI).
    pub nal_unit_type: u8,
    /// The NAL units (two-byte header + payload each, no length prefix).
    pub nal_units: Vec<Vec<u8>>,
}

/// Parsed `hvcC` record. The raw bytes are kept for the decoder
/// hand-off and for byte-exact rewriting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HevcConfig {
    /// `configurationVersion` (1).
    pub configuration_version: u8,
    /// `general_profile_space`.
    pub general_profile_space: u8,
    /// `general_tier_flag`.
    pub general_tier_flag: bool,
    /// `general_profile_idc` (1 Main, 2 Main 10, 3 Main Still Picture, 4 RExt, …).
    pub general_profile_idc: u8,
    /// `general_profile_compatibility_flags` (bit 31 ↔ profile 0).
    pub general_profile_compatibility_flags: u32,
    /// `general_constraint_indicator_flags` (48 bits).
    pub general_constraint_indicator_flags: u64,
    /// `general_level_idc` (level × 30).
    pub general_level_idc: u8,
    /// `min_spatial_segmentation_idc`.
    pub min_spatial_segmentation_idc: u16,
    /// `parallelismType`.
    pub parallelism_type: u8,
    /// `chroma_format_idc` (0 mono, 1 4:2:0, 2 4:2:2, 3 4:4:4).
    pub chroma_format_idc: u8,
    /// `bit_depth_luma_minus8`.
    pub bit_depth_luma_minus8: u8,
    /// `bit_depth_chroma_minus8`.
    pub bit_depth_chroma_minus8: u8,
    /// `avgFrameRate`.
    pub avg_frame_rate: u16,
    /// `constantFrameRate`.
    pub constant_frame_rate: u8,
    /// `numTemporalLayers`.
    pub num_temporal_layers: u8,
    /// `temporalIdNested`.
    pub temporal_id_nested: bool,
    /// `lengthSizeMinusOne + 1`: the byte width of in-band NAL length prefixes.
    pub length_size: u8,
    /// The NAL arrays.
    pub arrays: Vec<NalArray>,
    /// The record bytes as found in the file.
    pub raw: Vec<u8>,
}

impl HevcConfig {
    /// Parse a record.
    pub fn parse(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let configuration_version = r.u8("hvcC configurationVersion")?;
        if configuration_version != 1 {
            return Err(HeifError::invalid(format!(
                "hvcC configurationVersion {configuration_version} (expected 1)"
            )));
        }
        let b1 = r.u8("hvcC profile byte")?;
        let general_profile_space = b1 >> 6;
        let general_tier_flag = (b1 >> 5) & 1 == 1;
        let general_profile_idc = b1 & 0x1f;
        let general_profile_compatibility_flags =
            r.u32("hvcC general_profile_compatibility_flags")?;
        let hi = r.u16("hvcC general_constraint_indicator_flags")? as u64;
        let lo = r.u32("hvcC general_constraint_indicator_flags")? as u64;
        let general_constraint_indicator_flags = (hi << 32) | lo;
        let general_level_idc = r.u8("hvcC general_level_idc")?;
        let min_spatial_segmentation_idc = r.u16("hvcC min_spatial_segmentation_idc")? & 0x0fff;
        let parallelism_type = r.u8("hvcC parallelismType")? & 0x03;
        let chroma_format_idc = r.u8("hvcC chroma_format_idc")? & 0x03;
        let bit_depth_luma_minus8 = r.u8("hvcC bit_depth_luma_minus8")? & 0x07;
        let bit_depth_chroma_minus8 = r.u8("hvcC bit_depth_chroma_minus8")? & 0x07;
        let avg_frame_rate = r.u16("hvcC avgFrameRate")?;
        let b21 = r.u8("hvcC lengthSizeMinusOne")?;
        let constant_frame_rate = b21 >> 6;
        let num_temporal_layers = (b21 >> 3) & 0x07;
        let temporal_id_nested = (b21 >> 2) & 1 == 1;
        let length_size = (b21 & 0x03) + 1;
        let num_arrays = r.u8("hvcC numOfArrays")? as usize;
        let mut arrays = Vec::with_capacity(num_arrays);
        for _ in 0..num_arrays {
            let t = r.u8("hvcC array type")?;
            let n = r.u16("hvcC numNalus")? as usize;
            let mut nal_units = Vec::with_capacity(n);
            for _ in 0..n {
                let len = r.u16("hvcC nalUnitLength")? as usize;
                nal_units.push(r.bytes(len, "hvcC nalUnit")?.to_vec());
            }
            arrays.push(NalArray {
                complete: t & 0x80 != 0,
                reserved_bit: t & 0x40 != 0,
                nal_unit_type: t & 0x3f,
                nal_units,
            });
        }
        Ok(Self {
            configuration_version,
            general_profile_space,
            general_tier_flag,
            general_profile_idc,
            general_profile_compatibility_flags,
            general_constraint_indicator_flags,
            general_level_idc,
            min_spatial_segmentation_idc,
            parallelism_type,
            chroma_format_idc,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
            avg_frame_rate,
            constant_frame_rate,
            num_temporal_layers,
            temporal_id_nested,
            length_size,
            arrays,
            raw: b.to_vec(),
        })
    }

    /// Luma bit depth.
    pub fn bit_depth_luma(&self) -> u8 {
        self.bit_depth_luma_minus8 + 8
    }

    /// Chroma bit depth.
    pub fn bit_depth_chroma(&self) -> u8 {
        self.bit_depth_chroma_minus8 + 8
    }

    /// Total number of NAL units across the arrays.
    pub fn nal_count(&self) -> usize {
        self.arrays.iter().map(|a| a.nal_units.len()).sum()
    }

    /// NAL units of the given type, in record order.
    pub fn nal_units_of_type(&self, nal_unit_type: u8) -> Vec<&[u8]> {
        self.arrays
            .iter()
            .filter(|a| a.nal_unit_type == nal_unit_type)
            .flat_map(|a| a.nal_units.iter().map(Vec::as_slice))
            .collect()
    }

    /// `true` when `general_profile_idc` or the compatibility flags
    /// declare `profile_idc`.
    pub fn declares_profile(&self, profile_idc: u8) -> bool {
        self.general_profile_idc == profile_idc
            || (profile_idc < 32
                && self.general_profile_compatibility_flags & (1u32 << (31 - profile_idc)) != 0)
    }

    /// Serialize the record. When `raw` matches the parsed fields (the
    /// common case) the original bytes are returned unchanged.
    pub fn to_bytes(&self) -> Vec<u8> {
        if !self.raw.is_empty() {
            return self.raw.clone();
        }
        self.serialize()
    }

    /// Serialize the record from its fields (ignoring `raw`).
    pub fn serialize(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(HEVC_CONFIG_HEAD_LEN + 64);
        b.push(1);
        b.push(
            (self.general_profile_space << 6)
                | ((self.general_tier_flag as u8) << 5)
                | (self.general_profile_idc & 0x1f),
        );
        b.extend_from_slice(&self.general_profile_compatibility_flags.to_be_bytes());
        b.extend_from_slice(&self.general_constraint_indicator_flags.to_be_bytes()[2..]);
        b.push(self.general_level_idc);
        b.extend_from_slice(&(0xf000 | (self.min_spatial_segmentation_idc & 0x0fff)).to_be_bytes());
        b.push(0xfc | (self.parallelism_type & 0x03));
        b.push(0xfc | (self.chroma_format_idc & 0x03));
        b.push(0xf8 | (self.bit_depth_luma_minus8 & 0x07));
        b.push(0xf8 | (self.bit_depth_chroma_minus8 & 0x07));
        b.extend_from_slice(&self.avg_frame_rate.to_be_bytes());
        b.push(
            (self.constant_frame_rate << 6)
                | ((self.num_temporal_layers & 0x07) << 3)
                | ((self.temporal_id_nested as u8) << 2)
                | ((self.length_size.saturating_sub(1)) & 0x03),
        );
        b.push(self.arrays.len() as u8);
        for a in &self.arrays {
            b.push(
                ((a.complete as u8) << 7)
                    | ((a.reserved_bit as u8) << 6)
                    | (a.nal_unit_type & 0x3f),
            );
            b.extend_from_slice(&(a.nal_units.len() as u16).to_be_bytes());
            for n in &a.nal_units {
                b.extend_from_slice(&(n.len() as u16).to_be_bytes());
                b.extend_from_slice(n);
            }
        }
        b
    }

    /// Human-readable chroma format (`mono`, `yuv420`, `yuv422`, `yuv444`).
    pub fn chroma_name(&self) -> &'static str {
        match self.chroma_format_idc {
            0 => "mono",
            1 => "yuv420",
            2 => "yuv422",
            _ => "yuv444",
        }
    }
}

/// Split an `HEVCItemData` / sample payload into its NAL units
/// (`length_size`-byte big-endian length prefixes, HEIF Annex B.2.2.2).
pub fn split_length_prefixed(data: &[u8], length_size: u8) -> Result<Vec<&[u8]>> {
    if !(1..=4).contains(&length_size) {
        return Err(HeifError::invalid(format!(
            "NAL length prefix size {length_size} (expected 1..=4)"
        )));
    }
    let mut out = Vec::new();
    let mut r = Reader::new(data);
    while !r.is_empty() {
        let len = r.uint(length_size as usize, "NALUnitLength")? as usize;
        if len == 0 {
            return Err(HeifError::invalid("zero-length NAL unit"));
        }
        out.push(r.bytes(len, "NALUnit")?);
    }
    Ok(out)
}

/// Re-frame NAL units with `length_size`-byte length prefixes.
pub fn join_length_prefixed(nal_units: &[&[u8]], length_size: u8) -> Result<Vec<u8>> {
    if !(1..=4).contains(&length_size) {
        return Err(HeifError::invalid(format!(
            "NAL length prefix size {length_size} (expected 1..=4)"
        )));
    }
    let mut out = Vec::new();
    for n in nal_units {
        let len = n.len() as u64;
        if length_size < 8 && len >= 1u64 << (8 * length_size as u64) {
            return Err(HeifError::invalid(format!(
                "NAL unit of {len} bytes does not fit a {length_size}-byte length prefix"
            )));
        }
        out.extend_from_slice(&len.to_be_bytes()[8 - length_size as usize..]);
        out.extend_from_slice(n);
    }
    Ok(out)
}

/// Split an Annex B byte stream (3- or 4-byte start codes) into NAL
/// units. Leading zero bytes and trailing zero padding are tolerated.
pub fn split_annex_b(stream: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let n = stream.len();
    // Find the first start code.
    let find_sc = |from: usize| -> Option<(usize, usize)> {
        let mut j = from;
        while j + 3 <= n {
            if stream[j] == 0 && stream[j + 1] == 0 && stream[j + 2] == 1 {
                return Some((j, j + 3));
            }
            j += 1;
        }
        None
    };
    let Some((_, mut payload_start)) = find_sc(0) else {
        return out;
    };
    loop {
        match find_sc(payload_start) {
            Some((sc_start, next_payload)) => {
                let mut end = sc_start;
                // A 4-byte start code has a zero byte before 00 00 01;
                // trailing zero bytes belong to no NAL unit.
                while end > payload_start && stream[end - 1] == 0 {
                    end -= 1;
                }
                if end > payload_start {
                    out.push(&stream[payload_start..end]);
                }
                payload_start = next_payload;
            }
            None => {
                let mut end = n;
                while end > payload_start && stream[end - 1] == 0 {
                    end -= 1;
                }
                if end > payload_start {
                    out.push(&stream[payload_start..end]);
                }
                break;
            }
        }
        if payload_start >= n {
            break;
        }
    }
    out
}

/// `nal_unit_type` of a coded NAL unit (from its two-byte header).
pub fn nal_unit_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| (b >> 1) & 0x3f)
}

/// `nuh_layer_id` of a coded NAL unit.
pub fn nal_layer_id(nal: &[u8]) -> Option<u8> {
    if nal.len() < 2 {
        return None;
    }
    Some(((nal[0] & 1) << 5) | (nal[1] >> 3))
}

/// HEVC NAL unit type of the VPS.
pub const NAL_VPS: u8 = 32;
/// HEVC NAL unit type of the SPS.
pub const NAL_SPS: u8 = 33;
/// HEVC NAL unit type of the PPS.
pub const NAL_PPS: u8 = 34;
/// HEVC NAL unit type of a prefix SEI.
pub const NAL_PREFIX_SEI: u8 = 39;
/// HEVC NAL unit type of a suffix SEI.
pub const NAL_SUFFIX_SEI: u8 = 40;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A record with the layout the corpus uses (3 arrays, one NAL each).
    pub(crate) fn sample_record() -> Vec<u8> {
        let mut b = vec![
            0x01, // version
            0x03, // profile_space 0, tier 0, profile_idc 3
            0x70, 0x00, 0x00, 0x00, // compat flags
            0x90, 0x00, 0x00, 0x00, 0x00, 0x00, // constraint flags
            0x3c, // level 60
            0xf0, 0x00, // min_spatial_segmentation
            0xfc, // parallelism
            0xfd, // chroma 4:2:0
            0xf8, // luma 8
            0xf8, // chroma 8
            0x00, 0x00, // avg frame rate
            0x0f, // cfr 0, temporal layers 1, nested 1, lengthSizeMinusOne 3
            0x03, // arrays
        ];
        for (t, nal) in [
            (0xa0u8, vec![0x40, 0x01, 0x0c, 0x01]),
            (0xa1, vec![0x42, 0x01, 0x01, 0x03]),
            (0xa2, vec![0x44, 0x01, 0xc1]),
        ] {
            b.push(t);
            b.extend_from_slice(&1u16.to_be_bytes());
            b.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            b.extend_from_slice(&nal);
        }
        b
    }

    #[test]
    fn parses_and_reserializes_byte_exact() {
        let raw = sample_record();
        let c = HevcConfig::parse(&raw).unwrap();
        assert_eq!(c.general_profile_idc, 3);
        assert_eq!(c.general_level_idc, 60);
        assert_eq!(c.chroma_format_idc, 1);
        assert_eq!(c.chroma_name(), "yuv420");
        assert_eq!(c.bit_depth_luma(), 8);
        assert_eq!(c.length_size, 4);
        assert_eq!(c.num_temporal_layers, 1);
        assert!(c.temporal_id_nested);
        assert_eq!(c.arrays.len(), 3);
        assert_eq!(c.nal_count(), 3);
        assert!(c.arrays[0].complete);
        assert_eq!(c.arrays[0].nal_unit_type, NAL_VPS);
        assert_eq!(c.nal_units_of_type(NAL_SPS).len(), 1);
        assert!(c.declares_profile(3));
        assert!(c.declares_profile(1), "compat flag bit for profile 1");
        assert!(c.declares_profile(2), "0x70 compat nibble = profiles 1..=3");
        assert!(!c.declares_profile(4));
        assert_eq!(c.to_bytes(), raw);
        assert_eq!(c.serialize(), raw);
        assert_eq!(c.general_constraint_indicator_flags, 0x9000_0000_0000);
    }

    #[test]
    fn rejects_bad_version_and_truncation() {
        let mut raw = sample_record();
        raw[0] = 2;
        assert!(HevcConfig::parse(&raw).is_err());
        let raw = sample_record();
        assert!(HevcConfig::parse(&raw[..raw.len() - 1]).is_err());
        assert!(HevcConfig::parse(&raw[..10]).is_err());
    }

    #[test]
    fn length_prefix_split_and_join() {
        let nals: Vec<&[u8]> = vec![&[0x26, 0x01, 0xaf], &[0x28, 0x01]];
        let joined = join_length_prefixed(&nals, 4).unwrap();
        assert_eq!(joined.len(), 4 + 3 + 4 + 2);
        let split = split_length_prefixed(&joined, 4).unwrap();
        assert_eq!(split, nals);
        assert!(split_length_prefixed(&joined[..6], 4).is_err());
        assert!(split_length_prefixed(&[0, 0, 0, 0], 4).is_err());
        let two = join_length_prefixed(&nals, 2).unwrap();
        assert_eq!(split_length_prefixed(&two, 2).unwrap(), nals);
        assert!(join_length_prefixed(&[&[0u8; 300]], 1).is_err());
        assert_eq!(nal_unit_type(&[0x26, 0x01]), Some(19));
        assert_eq!(nal_layer_id(&[0x26, 0x01]), Some(0));
    }

    #[test]
    fn annex_b_split() {
        let s = [
            0, 0, 0, 1, 0x40, 0x01, 0x0c, 0, 0, 1, 0x42, 0x01, 0xaa, 0, 0, 0, 0, 1, 0x26, 0x01,
            0xff, 0, 0,
        ];
        let nals = split_annex_b(&s);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x40, 0x01, 0x0c]);
        assert_eq!(nals[1], &[0x42, 0x01, 0xaa]);
        assert_eq!(nals[2], &[0x26, 0x01, 0xff]);
        assert!(split_annex_b(&[1, 2, 3]).is_empty());
    }
}
