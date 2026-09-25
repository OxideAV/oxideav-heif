//! `AVCDecoderConfigurationRecord` (`avcC`, ISO/IEC 14496-15 §5.3.2.1)
//! — the decoder configuration property of `avc1` image items (HEIF
//! Annex E.2.3) and of `avc1` sample entries.
//!
//! ```text
//! unsigned int(8)  configurationVersion = 1;
//! unsigned int(8)  AVCProfileIndication;
//! unsigned int(8)  profile_compatibility;
//! unsigned int(8)  AVCLevelIndication;
//! bit(6) reserved = '111111'b; unsigned int(2) lengthSizeMinusOne;
//! bit(3) reserved = '111'b;    unsigned int(5) numOfSequenceParameterSets;
//! for (i) { unsigned int(16) sequenceParameterSetLength; bit(8*len) sequenceParameterSetNALUnit; }
//! unsigned int(8)  numOfPictureParameterSets;
//! for (i) { unsigned int(16) pictureParameterSetLength;  bit(8*len) pictureParameterSetNALUnit; }
//! if (AVCProfileIndication != 66 && != 77 && != 88) {
//!   bit(6) reserved; unsigned int(2) chroma_format;
//!   bit(5) reserved; unsigned int(3) bit_depth_luma_minus8;
//!   bit(5) reserved; unsigned int(3) bit_depth_chroma_minus8;
//!   unsigned int(8) numOfSequenceParameterSetExt;
//!   for (i) { unsigned int(16) sequenceParameterSetExtLength; bit(8*len) sequenceParameterSetExtNALUnit; }
//! }
//! ```
//!
//! The record bytes are kept for the decoder hand-off (`oxideav-h264`
//! takes the `avcC` record as `extradata` with length-prefixed
//! packets) and for byte-exact rewriting.

use crate::boxes::Reader;
use crate::error::{HeifError, Result};
use crate::image::{Chroma, HeifPixelFormat};

/// Parsed `avcC` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvcConfig {
    /// `configurationVersion` (1).
    pub configuration_version: u8,
    /// `AVCProfileIndication` (`profile_idc`).
    pub profile_idc: u8,
    /// `profile_compatibility` (the constraint-set byte of the SPS).
    pub profile_compatibility: u8,
    /// `AVCLevelIndication` (`level_idc`).
    pub level_idc: u8,
    /// `lengthSizeMinusOne + 1` — the NAL length-prefix width in bytes.
    pub length_size: u8,
    /// The SPS NAL units (one-byte header + payload each).
    pub sps: Vec<Vec<u8>>,
    /// The PPS NAL units.
    pub pps: Vec<Vec<u8>>,
    /// `chroma_format` when the High-family trailer is present.
    pub chroma_format: Option<u8>,
    /// `bit_depth_luma_minus8` when the trailer is present.
    pub bit_depth_luma_minus8: Option<u8>,
    /// `bit_depth_chroma_minus8` when the trailer is present.
    pub bit_depth_chroma_minus8: Option<u8>,
    /// SPS extension NAL units of the trailer.
    pub sps_ext: Vec<Vec<u8>>,
    /// The record bytes as parsed / serialized.
    pub raw: Vec<u8>,
}

impl AvcConfig {
    /// `true` when `profile_idc` carries the trailer (§5.3.2.1.2: any
    /// profile other than Baseline 66, Main 77, Extended 88).
    pub fn has_extension_trailer(profile_idc: u8) -> bool {
        !matches!(profile_idc, 66 | 77 | 88)
    }

    /// Parse an `avcC` record body.
    pub fn parse(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let configuration_version = r.u8("avcC configurationVersion")?;
        if configuration_version != 1 {
            return Err(HeifError::invalid(format!(
                "avcC configurationVersion {configuration_version} (shall be 1)"
            )));
        }
        let profile_idc = r.u8("avcC AVCProfileIndication")?;
        let profile_compatibility = r.u8("avcC profile_compatibility")?;
        let level_idc = r.u8("avcC AVCLevelIndication")?;
        let length_size = (r.u8("avcC lengthSizeMinusOne")? & 0x03) + 1;
        if length_size == 3 {
            return Err(HeifError::invalid(
                "avcC lengthSizeMinusOne 2 (three-byte prefixes are not permitted)",
            ));
        }
        let n_sps = (r.u8("avcC numOfSequenceParameterSets")? & 0x1f) as usize;
        let mut sps = Vec::with_capacity(n_sps);
        for _ in 0..n_sps {
            let len = r.u16("avcC sequenceParameterSetLength")? as usize;
            sps.push(r.bytes(len, "avcC sequenceParameterSetNALUnit")?.to_vec());
        }
        let n_pps = r.u8("avcC numOfPictureParameterSets")? as usize;
        let mut pps = Vec::with_capacity(n_pps);
        for _ in 0..n_pps {
            let len = r.u16("avcC pictureParameterSetLength")? as usize;
            pps.push(r.bytes(len, "avcC pictureParameterSetNALUnit")?.to_vec());
        }
        let (mut chroma_format, mut bit_depth_luma_minus8, mut bit_depth_chroma_minus8) =
            (None, None, None);
        let mut sps_ext = Vec::new();
        // The trailer is mandatory for High-family profiles but some
        // writers omit it; a record that ends here is accepted.
        if Self::has_extension_trailer(profile_idc) && r.remaining() >= 4 {
            chroma_format = Some(r.u8("avcC chroma_format")? & 0x03);
            bit_depth_luma_minus8 = Some(r.u8("avcC bit_depth_luma_minus8")? & 0x07);
            bit_depth_chroma_minus8 = Some(r.u8("avcC bit_depth_chroma_minus8")? & 0x07);
            let n = r.u8("avcC numOfSequenceParameterSetExt")? as usize;
            for _ in 0..n {
                let len = r.u16("avcC sequenceParameterSetExtLength")? as usize;
                sps_ext.push(
                    r.bytes(len, "avcC sequenceParameterSetExtNALUnit")?
                        .to_vec(),
                );
            }
        }
        Ok(Self {
            configuration_version,
            profile_idc,
            profile_compatibility,
            level_idc,
            length_size,
            sps,
            pps,
            chroma_format,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
            sps_ext,
            raw: b.to_vec(),
        })
    }

    /// Serialize to record bytes (the trailer is written when the
    /// profile calls for it and the fields are known).
    pub fn serialize(&self) -> Vec<u8> {
        let mut v = vec![
            1,
            self.profile_idc,
            self.profile_compatibility,
            self.level_idc,
            0xfc | ((self.length_size.clamp(1, 4) - 1) & 0x03),
            0xe0 | (self.sps.len().min(31) as u8),
        ];
        for s in self.sps.iter().take(31) {
            v.extend_from_slice(&(s.len() as u16).to_be_bytes());
            v.extend_from_slice(s);
        }
        v.push(self.pps.len().min(255) as u8);
        for p in self.pps.iter().take(255) {
            v.extend_from_slice(&(p.len() as u16).to_be_bytes());
            v.extend_from_slice(p);
        }
        if Self::has_extension_trailer(self.profile_idc) {
            if let (Some(c), Some(l), Some(ch)) = (
                self.chroma_format,
                self.bit_depth_luma_minus8,
                self.bit_depth_chroma_minus8,
            ) {
                v.push(0xfc | (c & 0x03));
                v.push(0xf8 | (l & 0x07));
                v.push(0xf8 | (ch & 0x07));
                v.push(self.sps_ext.len().min(255) as u8);
                for e in self.sps_ext.iter().take(255) {
                    v.extend_from_slice(&(e.len() as u16).to_be_bytes());
                    v.extend_from_slice(e);
                }
            }
        }
        v
    }

    /// Luma bit depth (8 without a trailer).
    pub fn bit_depth_luma(&self) -> u8 {
        8 + self.bit_depth_luma_minus8.unwrap_or(0)
    }

    /// Sample layout the record announces: the trailer's
    /// `chroma_format` / bit depth, else 4:2:0 8-bit (the only layout
    /// of the Baseline / Main / Extended profiles).
    pub fn layout(&self) -> Result<HeifPixelFormat> {
        let chroma = Chroma::from_idc(self.chroma_format.unwrap_or(1))
            .ok_or_else(|| HeifError::invalid("avcC chroma_format"))?;
        HeifPixelFormat::new(chroma, self.bit_depth_luma(), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_with_and_without_the_trailer() {
        let high = AvcConfig {
            configuration_version: 1,
            profile_idc: 100,
            profile_compatibility: 0,
            level_idc: 31,
            length_size: 4,
            sps: vec![vec![0x67, 0x64, 0, 31, 0xac]],
            pps: vec![vec![0x68, 0xee, 0x3c, 0x80]],
            chroma_format: Some(1),
            bit_depth_luma_minus8: Some(0),
            bit_depth_chroma_minus8: Some(0),
            sps_ext: Vec::new(),
            raw: Vec::new(),
        };
        let bytes = high.serialize();
        assert_eq!(bytes[4], 0xff, "reserved 111111 + lengthSizeMinusOne 3");
        assert_eq!(bytes[5], 0xe1, "reserved 111 + 1 SPS");
        assert_eq!(bytes.len(), 6 + 2 + 5 + 1 + 2 + 4 + 4);
        let back = AvcConfig::parse(&bytes).unwrap();
        assert_eq!(back.raw, bytes);
        assert_eq!(
            AvcConfig {
                raw: Vec::new(),
                ..back.clone()
            },
            high
        );
        assert_eq!(back.layout().unwrap().chroma, Chroma::Yuv420);
        let base = AvcConfig {
            profile_idc: 66,
            chroma_format: None,
            bit_depth_luma_minus8: None,
            bit_depth_chroma_minus8: None,
            ..high.clone()
        };
        let b = base.serialize();
        assert_eq!(b.len(), 6 + 2 + 5 + 1 + 2 + 4);
        let back = AvcConfig::parse(&b).unwrap();
        assert_eq!(back.chroma_format, None);
        assert_eq!(back.layout().unwrap().bit_depth, 8);
        // A High record without its trailer still parses.
        let truncated = AvcConfig::parse(&bytes[..bytes.len() - 4]).unwrap();
        assert_eq!(truncated.chroma_format, None);
        // Illegal three-byte prefix.
        let mut bad = bytes.clone();
        bad[4] = 0xfe;
        assert!(AvcConfig::parse(&bad).is_err());
        assert!(AvcConfig::parse(&bytes[..3]).is_err());
    }
}
