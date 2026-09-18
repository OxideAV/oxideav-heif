//! `AV1CodecConfigurationRecord` (`av1C`, AV1 ISOBMFF Binding §2.3) —
//! the decoder configuration property of `av01` image items (AVIF
//! §4.2) and of `av01` sample entries.
//!
//! ```text
//! unsigned int(1) marker = 1;
//! unsigned int(7) version = 1;
//! unsigned int(3) seq_profile;
//! unsigned int(5) seq_level_idx_0;
//! unsigned int(1) seq_tier_0;
//! unsigned int(1) high_bitdepth;
//! unsigned int(1) twelve_bit;
//! unsigned int(1) monochrome;
//! unsigned int(1) chroma_subsampling_x;
//! unsigned int(1) chroma_subsampling_y;
//! unsigned int(2) chroma_sample_position;
//! unsigned int(3) reserved = 0;
//! unsigned int(1) initial_presentation_delay_present;
//! if (initial_presentation_delay_present) {
//!   unsigned int(4) initial_presentation_delay_minus_one;
//! } else {
//!   unsigned int(4) reserved = 0;
//! }
//! unsigned int(8) configOBUs[];
//! ```

use crate::error::{HeifError, Result};

/// Parsed `av1C` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Av1Config {
    /// `seq_profile`.
    pub seq_profile: u8,
    /// `seq_level_idx_0`.
    pub seq_level_idx_0: u8,
    /// `seq_tier_0`.
    pub seq_tier_0: bool,
    /// `high_bitdepth`.
    pub high_bitdepth: bool,
    /// `twelve_bit`.
    pub twelve_bit: bool,
    /// `monochrome`.
    pub monochrome: bool,
    /// `chroma_subsampling_x`.
    pub chroma_subsampling_x: bool,
    /// `chroma_subsampling_y`.
    pub chroma_subsampling_y: bool,
    /// `chroma_sample_position`.
    pub chroma_sample_position: u8,
    /// `initial_presentation_delay_minus_one` when present.
    pub initial_presentation_delay_minus_one: Option<u8>,
    /// `configOBUs` (typically the Sequence Header OBU).
    pub config_obus: Vec<u8>,
    /// The record bytes as found in the file.
    pub raw: Vec<u8>,
}

impl Av1Config {
    /// Parse a record.
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < 4 {
            return Err(HeifError::invalid(format!(
                "av1C: record is {} bytes (need at least 4)",
                b.len()
            )));
        }
        if b[0] >> 7 != 1 {
            return Err(HeifError::invalid("av1C: marker bit is 0"));
        }
        let version = b[0] & 0x7f;
        if version != 1 {
            return Err(HeifError::invalid(format!(
                "av1C: version {version} (expected 1)"
            )));
        }
        let ipd_present = (b[3] >> 4) & 1 == 1;
        Ok(Self {
            seq_profile: b[1] >> 5,
            seq_level_idx_0: b[1] & 0x1f,
            seq_tier_0: b[2] >> 7 == 1,
            high_bitdepth: (b[2] >> 6) & 1 == 1,
            twelve_bit: (b[2] >> 5) & 1 == 1,
            monochrome: (b[2] >> 4) & 1 == 1,
            chroma_subsampling_x: (b[2] >> 3) & 1 == 1,
            chroma_subsampling_y: (b[2] >> 2) & 1 == 1,
            chroma_sample_position: b[2] & 0x03,
            initial_presentation_delay_minus_one: if ipd_present {
                Some(b[3] & 0x0f)
            } else {
                None
            },
            config_obus: b[4..].to_vec(),
            raw: b.to_vec(),
        })
    }

    /// Bit depth (8 / 10 / 12).
    pub fn bit_depth(&self) -> u8 {
        match (self.high_bitdepth, self.twelve_bit) {
            (false, _) => 8,
            (true, false) => 10,
            (true, true) => 12,
        }
    }

    /// Equivalent HEVC-style `chroma_format_idc` (0 mono, 1 4:2:0,
    /// 2 4:2:2, 3 4:4:4).
    pub fn chroma_format_idc(&self) -> u8 {
        if self.monochrome {
            0
        } else {
            match (self.chroma_subsampling_x, self.chroma_subsampling_y) {
                (true, true) => 1,
                (true, false) => 2,
                _ => 3,
            }
        }
    }

    /// Serialize the record (original bytes when available).
    pub fn to_bytes(&self) -> Vec<u8> {
        if !self.raw.is_empty() {
            return self.raw.clone();
        }
        self.serialize()
    }

    /// Serialize the record from its fields.
    pub fn serialize(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(4 + self.config_obus.len());
        b.push(0x81);
        b.push(((self.seq_profile & 0x07) << 5) | (self.seq_level_idx_0 & 0x1f));
        b.push(
            ((self.seq_tier_0 as u8) << 7)
                | ((self.high_bitdepth as u8) << 6)
                | ((self.twelve_bit as u8) << 5)
                | ((self.monochrome as u8) << 4)
                | ((self.chroma_subsampling_x as u8) << 3)
                | ((self.chroma_subsampling_y as u8) << 2)
                | (self.chroma_sample_position & 0x03),
        );
        b.push(match self.initial_presentation_delay_minus_one {
            Some(d) => 0x10 | (d & 0x0f),
            None => 0,
        });
        b.extend_from_slice(&self.config_obus);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_serialize() {
        let raw = [0x81, 0x0c, 0x0c, 0x00, 0x0a, 0x0b];
        let c = Av1Config::parse(&raw).unwrap();
        assert_eq!(c.seq_profile, 0);
        assert_eq!(c.seq_level_idx_0, 12);
        assert!(c.chroma_subsampling_x && c.chroma_subsampling_y);
        assert_eq!(c.bit_depth(), 8);
        assert_eq!(c.chroma_format_idc(), 1);
        assert_eq!(c.config_obus, vec![0x0a, 0x0b]);
        assert_eq!(c.serialize(), raw);
        assert_eq!(c.to_bytes(), raw);
        let hi = Av1Config::parse(&[0x81, 0x4d, 0x70, 0x1f]).unwrap();
        assert_eq!(hi.seq_profile, 2);
        assert_eq!(hi.bit_depth(), 12);
        assert!(hi.monochrome);
        assert_eq!(hi.chroma_format_idc(), 0);
        assert_eq!(hi.initial_presentation_delay_minus_one, Some(15));
        assert_eq!(hi.serialize(), [0x81, 0x4d, 0x70, 0x1f]);
        let c422 = Av1Config::parse(&[0x81, 0x00, 0x08, 0x00]).unwrap();
        assert_eq!(c422.chroma_format_idc(), 2);
        let c444 = Av1Config::parse(&[0x81, 0x00, 0x40, 0x00]).unwrap();
        assert_eq!(c444.chroma_format_idc(), 3);
        assert_eq!(c444.bit_depth(), 10);
    }

    #[test]
    fn rejects_marker_version_and_short() {
        assert!(Av1Config::parse(&[0x01, 0, 0, 0]).is_err());
        assert!(Av1Config::parse(&[0x82, 0, 0, 0]).is_err());
        assert!(Av1Config::parse(&[0x81, 0, 0]).is_err());
    }
}
