//! `VvcDecoderConfigurationRecord` (`vvcC`, ISO/IEC 14496-15:2024
//! §11.2.4) — the decoder configuration property of `vvc1` image
//! items (HEIF Annex L.2.3.1) and of `vvc1` / `vvi1` sample entries in
//! image sequences (L.3.2), plus the `CompactVvcDecoderConfigurationRecord`
//! of low-overhead `vvi3` files (HEIF Amd 2:2026 L.4.3.3).
//!
//! The property is a FullBox (`VvcConfigurationBox extends
//! FullBox('vvcC', version = 0, flags)`); [`VvcConfig`] models the
//! record and keeps the box flags (`flags & 1` on a subpicture item
//! marks content not intended for display).
//!
//! ```text
//! aligned(8) class VvcDecoderConfigurationRecord {
//!   bit(5) reserved = '11111'b;
//!   unsigned int(2) LengthSizeMinusOne;
//!   unsigned int(1) ptl_present_flag;
//!   if (ptl_present_flag) {
//!     unsigned int(9) ols_idx;
//!     unsigned int(3) num_sublayers;
//!     unsigned int(2) constant_frame_rate;
//!     unsigned int(2) chroma_format_idc;
//!     unsigned int(3) bit_depth_minus8;
//!     bit(5) reserved = '11111'b;
//!     VvcPTLRecord(num_sublayers) native_ptl;
//!     unsigned int(16) max_picture_width;
//!     unsigned int(16) max_picture_height;
//!     unsigned int(16) avg_frame_rate;
//!   }
//!   unsigned int(8) num_of_arrays;
//!   for (j = 0; j < num_of_arrays; j++) {
//!     unsigned int(1) array_completeness;
//!     bit(2) reserved = 0;
//!     unsigned int(5) NAL_unit_type;
//!     if (NAL_unit_type != DCI_NUT && NAL_unit_type != OPI_NUT)
//!       unsigned int(16) num_nalus;          // else 1
//!     for (i = 0; i < num_nalus; i++) {
//!       unsigned int(16) nal_unit_length;
//!       bit(8*nal_unit_length) nal_unit;
//!     }
//!   }
//! }
//!
//! aligned(8) class VvcPTLRecord(num_sublayers) {
//!   bit(2) reserved = 0;
//!   unsigned int(6) num_bytes_constraint_info;
//!   unsigned int(7) general_profile_idc;
//!   unsigned int(1) general_tier_flag;
//!   unsigned int(8) general_level_idc;
//!   unsigned int(1) ptl_frame_only_constraint_flag;
//!   unsigned int(1) ptl_multilayer_enabled_flag;
//!   unsigned int(8*num_bytes_constraint_info - 2) general_constraint_info;
//!   for (i = num_sublayers - 2; i >= 0; i--)
//!     unsigned int(1) ptl_sublayer_level_present_flag[i];
//!   for (j = num_sublayers; j <= 8 && num_sublayers > 1; j++)
//!     bit(1) ptl_reserved_zero_bit = 0;
//!   for (i = num_sublayers - 2; i >= 0; i--)
//!     if (ptl_sublayer_level_present_flag[i])
//!       unsigned int(8) sublayer_level_idc[i];
//!   unsigned int(8) ptl_num_sub_profiles;
//!   for (j = 0; j < ptl_num_sub_profiles; j++)
//!     unsigned int(32) general_sub_profile_idc[j];
//! }
//! ```
//!
//! The parse is standalone (no dependency on the VVC decoder) so the
//! container can be inspected without `registry`; with `registry` on,
//! the record's parameter sets and the item's length-prefixed NAL
//! units are re-assembled into one Annex B access unit
//! ([`access_unit_annex_b`]) for `oxideav-h266`'s stream decoder.

use crate::boxes::Reader;
use crate::error::{HeifError, Result};

/// VVC NAL unit type: `IDR_W_RADL`.
pub const NAL_IDR_W_RADL: u8 = 7;
/// VVC NAL unit type: `IDR_N_LP`.
pub const NAL_IDR_N_LP: u8 = 8;
/// VVC NAL unit type: `CRA_NUT`.
pub const NAL_CRA: u8 = 9;
/// VVC NAL unit type: `GDR_NUT`.
pub const NAL_GDR: u8 = 10;
/// VVC NAL unit type: `OPI_NUT`.
pub const NAL_OPI: u8 = 12;
/// VVC NAL unit type: `DCI_NUT`.
pub const NAL_DCI: u8 = 13;
/// VVC NAL unit type: `VPS_NUT`.
pub const NAL_VPS: u8 = 14;
/// VVC NAL unit type: `SPS_NUT`.
pub const NAL_SPS: u8 = 15;
/// VVC NAL unit type: `PPS_NUT`.
pub const NAL_PPS: u8 = 16;
/// VVC NAL unit type: `PREFIX_APS_NUT`.
pub const NAL_PREFIX_APS: u8 = 17;
/// VVC NAL unit type: `SUFFIX_APS_NUT`.
pub const NAL_SUFFIX_APS: u8 = 18;
/// VVC NAL unit type: `PH_NUT`.
pub const NAL_PH: u8 = 19;
/// VVC NAL unit type: `AUD_NUT`.
pub const NAL_AUD: u8 = 20;
/// VVC NAL unit type: `EOS_NUT`.
pub const NAL_EOS: u8 = 21;
/// VVC NAL unit type: `EOB_NUT`.
pub const NAL_EOB: u8 = 22;
/// VVC NAL unit type: `PREFIX_SEI_NUT`.
pub const NAL_PREFIX_SEI: u8 = 23;
/// VVC NAL unit type: `SUFFIX_SEI_NUT`.
pub const NAL_SUFFIX_SEI: u8 = 24;

/// `nal_unit_type` of a VVC NAL unit (bits 3..8 of its second header
/// byte; ISO/IEC 23090-3 §7.3.1.2).
pub fn nal_unit_type(nal: &[u8]) -> Option<u8> {
    nal.get(1).map(|b| (b >> 3) & 0x1f)
}

/// `nuh_layer_id` of a VVC NAL unit (low six bits of the first header byte).
pub fn nal_layer_id(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x3f)
}

/// `nuh_temporal_id_plus1` of a VVC NAL unit (low three bits of the
/// second header byte).
pub fn nal_temporal_id_plus1(nal: &[u8]) -> Option<u8> {
    nal.get(1).map(|b| b & 0x07)
}

/// The two-byte VVC NAL unit header (`forbidden_zero_bit` 0,
/// `nuh_reserved_zero_bit` 0, `nuh_layer_id`, `nal_unit_type`,
/// `nuh_temporal_id_plus1`).
pub fn nal_header(nal_unit_type: u8, layer_id: u8, temporal_id_plus1: u8) -> [u8; 2] {
    [
        layer_id & 0x3f,
        ((nal_unit_type & 0x1f) << 3) | (temporal_id_plus1 & 0x07),
    ]
}

/// `true` for the VCL NAL unit types (0..=11).
pub fn is_vcl(nal_unit_type: u8) -> bool {
    nal_unit_type <= 11
}

/// `true` for the NAL unit types a `vvcC` record may carry (DCI, OPI,
/// VPS, SPS, PPS, prefix APS, prefix SEI).
pub fn is_record_nal_type(nal_unit_type: u8) -> bool {
    matches!(
        nal_unit_type,
        NAL_DCI | NAL_OPI | NAL_VPS | NAL_SPS | NAL_PPS | NAL_PREFIX_APS | NAL_PREFIX_SEI
    )
}

/// One NAL unit array of the record.
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct VvcNalArray {
    /// `array_completeness`.
    pub complete: bool,
    /// `NAL_unit_type` (5 bits).
    pub nal_unit_type: u8,
    /// The NAL units (two-byte header + payload each, no length prefix).
    pub nal_units: Vec<Vec<u8>>,
}
impl VvcNalArray {
    /// Every field as a positional argument, in declaration order.
    pub fn new(complete: bool, nal_unit_type: u8, nal_units: Vec<Vec<u8>>) -> Self {
        Self {
            complete,
            nal_unit_type,
            nal_units,
        }
    }
}

/// `VvcPTLRecord` (14496-15 §11.2.4.1): the profile / tier / level of
/// the operating point the record describes.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct VvcPtlRecord {
    /// `general_profile_idc` (1 Main 10, 17 Multilayer Main 10, 33
    /// Main 10 4:4:4, 65 Main 10 Still Picture, 97 Main 10 4:4:4 Still
    /// Picture, …; ISO/IEC 23090-3 Annex A).
    pub general_profile_idc: u8,
    /// `general_tier_flag`.
    pub general_tier_flag: bool,
    /// `general_level_idc` (level × 16: 4.0 → 64, 5.1 → 83, 6.2 → 102).
    pub general_level_idc: u8,
    /// `ptl_frame_only_constraint_flag`.
    pub ptl_frame_only_constraint_flag: bool,
    /// `ptl_multilayer_enabled_flag`.
    pub ptl_multilayer_enabled_flag: bool,
    /// The `general_constraint_info` bits as `num_bytes_constraint_info`
    /// bytes with the two leading flag bits cleared — the byte-aligned
    /// `general_constraints_info()` run of the parameter set's
    /// `profile_tier_level()` (its first bit is `gci_present_flag`).
    /// One byte of zeros when the bitstream signals no constraints.
    pub general_constraint_info: Vec<u8>,
    /// `sublayer_level_idc[i]` for `i` in `0..num_sublayers - 1`
    /// (`None` where `ptl_sublayer_level_present_flag[i]` is 0). Empty
    /// for a single sublayer.
    pub sublayer_level_idc: Vec<Option<u8>>,
    /// `general_sub_profile_idc[]`.
    pub general_sub_profile_idc: Vec<u32>,
}
impl VvcPtlRecord {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here or from
    /// `Default`, then read / assign its public fields).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        general_profile_idc: u8,
        general_tier_flag: bool,
        general_level_idc: u8,
        ptl_frame_only_constraint_flag: bool,
        ptl_multilayer_enabled_flag: bool,
        general_constraint_info: Vec<u8>,
        sublayer_level_idc: Vec<Option<u8>>,
        general_sub_profile_idc: Vec<u32>,
    ) -> Self {
        Self {
            general_profile_idc,
            general_tier_flag,
            general_level_idc,
            ptl_frame_only_constraint_flag,
            ptl_multilayer_enabled_flag,
            general_constraint_info,
            sublayer_level_idc,
            general_sub_profile_idc,
        }
    }

    /// `num_bytes_constraint_info` (1 when no constraints are signalled).
    pub fn num_bytes_constraint_info(&self) -> usize {
        self.general_constraint_info.len().max(1)
    }

    /// The number of sublayers the record was written for
    /// (`sublayer_level_idc.len() + 1`).
    pub fn num_sublayers(&self) -> u8 {
        (self.sublayer_level_idc.len() + 1).min(7) as u8
    }

    fn parse(r: &mut Reader<'_>, num_sublayers: u8) -> Result<Self> {
        let n = (r.u8("vvcC num_bytes_constraint_info")? & 0x3f) as usize;
        if n == 0 {
            return Err(HeifError::invalid(
                "vvcC num_bytes_constraint_info 0 (shall be greater than 0)",
            ));
        }
        let b = r.u8("vvcC general_profile_idc")?;
        let general_profile_idc = b >> 1;
        let general_tier_flag = b & 1 == 1;
        let general_level_idc = r.u8("vvcC general_level_idc")?;
        let mut gci = r.bytes(n, "vvcC general_constraint_info")?.to_vec();
        let ptl_frame_only_constraint_flag = gci[0] & 0x80 != 0;
        let ptl_multilayer_enabled_flag = gci[0] & 0x40 != 0;
        gci[0] &= 0x3f;
        let sub = num_sublayers as usize;
        let mut sublayer_level_idc = vec![None; sub.saturating_sub(1)];
        if sub > 1 {
            let flags = r.u8("vvcC ptl_sublayer_level_present_flag")?;
            // Bit 7 is flag[num_sublayers - 2], descending to
            // flag[0] at bit 7 - (num_sublayers - 2).
            for (k, i) in (0..sub - 1).rev().enumerate() {
                if flags & (0x80 >> k) != 0 {
                    sublayer_level_idc[i] = Some(0);
                }
            }
            for i in (0..sub - 1).rev() {
                if sublayer_level_idc[i].is_some() {
                    sublayer_level_idc[i] = Some(r.u8("vvcC sublayer_level_idc")?);
                }
            }
        }
        let n_sub = r.u8("vvcC ptl_num_sub_profiles")? as usize;
        let mut general_sub_profile_idc = Vec::with_capacity(n_sub);
        for _ in 0..n_sub {
            general_sub_profile_idc.push(r.u32("vvcC general_sub_profile_idc")?);
        }
        Ok(Self {
            general_profile_idc,
            general_tier_flag,
            general_level_idc,
            ptl_frame_only_constraint_flag,
            ptl_multilayer_enabled_flag,
            general_constraint_info: gci,
            sublayer_level_idc,
            general_sub_profile_idc,
        })
    }

    fn serialize(&self, out: &mut Vec<u8>, num_sublayers: u8) {
        let n = self.num_bytes_constraint_info();
        out.push((n as u8) & 0x3f);
        out.push((self.general_profile_idc << 1) | (self.general_tier_flag as u8));
        out.push(self.general_level_idc);
        let mut gci = self.general_constraint_info.clone();
        gci.resize(n, 0);
        gci[0] = (gci[0] & 0x3f)
            | ((self.ptl_frame_only_constraint_flag as u8) << 7)
            | ((self.ptl_multilayer_enabled_flag as u8) << 6);
        out.extend_from_slice(&gci);
        let sub = num_sublayers as usize;
        if sub > 1 {
            let mut flags = 0u8;
            for (k, i) in (0..sub - 1).rev().enumerate() {
                if self.sublayer_level_idc.get(i).copied().flatten().is_some() {
                    flags |= 0x80 >> k;
                }
            }
            out.push(flags);
            for i in (0..sub - 1).rev() {
                if let Some(Some(l)) = self.sublayer_level_idc.get(i) {
                    out.push(*l);
                }
            }
        }
        out.push(self.general_sub_profile_idc.len() as u8);
        for s in &self.general_sub_profile_idc {
            out.extend_from_slice(&s.to_be_bytes());
        }
    }
}

/// Parsed `vvcC` record (with the configuration box's flags). The raw
/// record bytes are kept for the decoder hand-off and for byte-exact
/// rewriting.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct VvcConfig {
    /// The `VvcConfigurationBox` FullBox flags (`flags & 1`: a
    /// subpicture item whose content is not intended for display).
    pub flags: u32,
    /// `LengthSizeMinusOne + 1`: the byte width of the NAL length
    /// prefixes in the item data (1, 2 or 4).
    pub length_size: u8,
    /// `ptl_present_flag`.
    pub ptl_present_flag: bool,
    /// `ols_idx` (when `ptl_present_flag`).
    pub ols_idx: u16,
    /// `num_sublayers` (when `ptl_present_flag`; 0 = unknown).
    pub num_sublayers: u8,
    /// `constant_frame_rate` (unspecified for image items).
    pub constant_frame_rate: u8,
    /// `chroma_format_idc` (0 mono, 1 4:2:0, 2 4:2:2, 3 4:4:4; when
    /// `ptl_present_flag`).
    pub chroma_format_idc: u8,
    /// `bit_depth_minus8` (when `ptl_present_flag`).
    pub bit_depth_minus8: u8,
    /// `native_ptl` (when `ptl_present_flag`).
    pub native_ptl: Option<VvcPtlRecord>,
    /// `max_picture_width` in luma samples (when `ptl_present_flag`).
    pub max_picture_width: u16,
    /// `max_picture_height` in luma samples (when `ptl_present_flag`).
    pub max_picture_height: u16,
    /// `avg_frame_rate` (unspecified for image items).
    pub avg_frame_rate: u16,
    /// The NAL unit arrays, in record order.
    pub arrays: Vec<VvcNalArray>,
    /// The record bytes as found in the file (after the FullBox
    /// header); empty for a record built from fields.
    pub raw: Vec<u8>,
}
impl VvcConfig {
    /// Every field as a positional argument, in declaration order
    /// (the struct is `#[non_exhaustive]`: build it here, then read /
    /// assign its public fields).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        flags: u32,
        length_size: u8,
        ptl_present_flag: bool,
        ols_idx: u16,
        num_sublayers: u8,
        constant_frame_rate: u8,
        chroma_format_idc: u8,
        bit_depth_minus8: u8,
        native_ptl: Option<VvcPtlRecord>,
        max_picture_width: u16,
        max_picture_height: u16,
        avg_frame_rate: u16,
        arrays: Vec<VvcNalArray>,
        raw: Vec<u8>,
    ) -> Self {
        Self {
            flags,
            length_size,
            ptl_present_flag,
            ols_idx,
            num_sublayers,
            constant_frame_rate,
            chroma_format_idc,
            bit_depth_minus8,
            native_ptl,
            max_picture_width,
            max_picture_height,
            avg_frame_rate,
            arrays,
            raw,
        }
    }

    /// Parse a record (the bytes after the `vvcC` FullBox header; the
    /// box flags are set by the caller).
    pub fn parse(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let b0 = r.u8("vvcC LengthSizeMinusOne")?;
        let length_size = ((b0 >> 1) & 0x03) + 1;
        if length_size == 3 {
            return Err(HeifError::invalid(
                "vvcC LengthSizeMinusOne 2 (shall be 0, 1 or 3)",
            ));
        }
        let ptl_present_flag = b0 & 1 == 1;
        let mut cfg = Self {
            flags: 0,
            length_size,
            ptl_present_flag,
            ols_idx: 0,
            num_sublayers: 0,
            constant_frame_rate: 0,
            chroma_format_idc: 0,
            bit_depth_minus8: 0,
            native_ptl: None,
            max_picture_width: 0,
            max_picture_height: 0,
            avg_frame_rate: 0,
            arrays: Vec::new(),
            raw: b.to_vec(),
        };
        if ptl_present_flag {
            let w = r.u16("vvcC ols_idx")?;
            cfg.ols_idx = w >> 7;
            cfg.num_sublayers = ((w >> 4) & 0x07) as u8;
            cfg.constant_frame_rate = ((w >> 2) & 0x03) as u8;
            cfg.chroma_format_idc = (w & 0x03) as u8;
            cfg.bit_depth_minus8 = r.u8("vvcC bit_depth_minus8")? >> 5;
            cfg.native_ptl = Some(VvcPtlRecord::parse(&mut r, cfg.num_sublayers)?);
            cfg.max_picture_width = r.u16("vvcC max_picture_width")?;
            cfg.max_picture_height = r.u16("vvcC max_picture_height")?;
            cfg.avg_frame_rate = r.u16("vvcC avg_frame_rate")?;
        }
        let num_arrays = r.u8("vvcC num_of_arrays")? as usize;
        for _ in 0..num_arrays {
            let t = r.u8("vvcC array type")?;
            let nal_unit_type = t & 0x1f;
            let n = if nal_unit_type == NAL_DCI || nal_unit_type == NAL_OPI {
                1
            } else {
                r.u16("vvcC num_nalus")? as usize
            };
            let mut nal_units = Vec::with_capacity(n);
            for _ in 0..n {
                let len = r.u16("vvcC nal_unit_length")? as usize;
                nal_units.push(r.bytes(len, "vvcC nal_unit")?.to_vec());
            }
            cfg.arrays.push(VvcNalArray {
                complete: t & 0x80 != 0,
                nal_unit_type,
                nal_units,
            });
        }
        Ok(cfg)
    }

    /// Serialize the record. When `raw` holds the bytes the record was
    /// parsed from they are returned unchanged.
    pub fn to_bytes(&self) -> Vec<u8> {
        if !self.raw.is_empty() {
            return self.raw.clone();
        }
        self.serialize()
    }

    /// Serialize the record from its fields (ignoring `raw`).
    pub fn serialize(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(32 + self.nal_count() * 32);
        b.push(
            0xf8 | ((self.length_size.saturating_sub(1) & 0x03) << 1)
                | (self.ptl_present_flag as u8),
        );
        if self.ptl_present_flag {
            let w = ((self.ols_idx & 0x1ff) << 7)
                | (((self.num_sublayers & 0x07) as u16) << 4)
                | (((self.constant_frame_rate & 0x03) as u16) << 2)
                | ((self.chroma_format_idc & 0x03) as u16);
            b.extend_from_slice(&w.to_be_bytes());
            b.push(((self.bit_depth_minus8 & 0x07) << 5) | 0x1f);
            self.native_ptl
                .clone()
                .unwrap_or_default()
                .serialize(&mut b, self.num_sublayers);
            b.extend_from_slice(&self.max_picture_width.to_be_bytes());
            b.extend_from_slice(&self.max_picture_height.to_be_bytes());
            b.extend_from_slice(&self.avg_frame_rate.to_be_bytes());
        }
        b.push(self.arrays.len() as u8);
        for a in &self.arrays {
            b.push(((a.complete as u8) << 7) | (a.nal_unit_type & 0x1f));
            if a.nal_unit_type != NAL_DCI && a.nal_unit_type != NAL_OPI {
                b.extend_from_slice(&(a.nal_units.len() as u16).to_be_bytes());
            }
            for n in a.nal_units.iter().take(
                if a.nal_unit_type == NAL_DCI || a.nal_unit_type == NAL_OPI {
                    1
                } else {
                    usize::MAX
                },
            ) {
                b.extend_from_slice(&(n.len() as u16).to_be_bytes());
                b.extend_from_slice(n);
            }
        }
        b
    }

    /// Luma / chroma bit depth (`bit_depth_minus8 + 8`; meaningful when
    /// `ptl_present_flag`).
    pub fn bit_depth(&self) -> u8 {
        self.bit_depth_minus8 + 8
    }

    /// Total number of NAL units across the arrays.
    pub fn nal_count(&self) -> usize {
        self.arrays.iter().map(|a| a.nal_units.len()).sum()
    }

    /// Every NAL unit of the record, in array order.
    pub fn nal_units(&self) -> impl Iterator<Item = &[u8]> {
        self.arrays
            .iter()
            .flat_map(|a| a.nal_units.iter().map(Vec::as_slice))
    }

    /// NAL units of the given type, in record order.
    pub fn nal_units_of_type(&self, nal_unit_type: u8) -> Vec<&[u8]> {
        self.arrays
            .iter()
            .filter(|a| a.nal_unit_type == nal_unit_type)
            .flat_map(|a| a.nal_units.iter().map(Vec::as_slice))
            .collect()
    }

    /// The first SPS NAL unit of the record, when any.
    pub fn sps(&self) -> Option<&[u8]> {
        self.nal_units_of_type(NAL_SPS).first().copied()
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

    /// The sample layout the record announces: the `chroma_format_idc`
    /// / `bit_depth_minus8` head when `ptl_present_flag`, else the SPS
    /// carried in the arrays (parsed standalone, see [`SpsHead`]).
    pub fn sample_format(&self) -> Result<(u8, u8)> {
        if self.ptl_present_flag {
            return Ok((self.chroma_format_idc, self.bit_depth()));
        }
        let sps = self
            .sps()
            .ok_or_else(|| HeifError::invalid("vvcC without ptl_present_flag carries no SPS"))?;
        let head = SpsHead::parse_nal(sps)?;
        Ok((head.chroma_format_idc, head.bit_depth))
    }
}

/// Re-assemble the Annex B access unit of a `vvc1` item (HEIF L.2.2.2):
/// the record's NAL units (DCI / OPI / VPS / SPS / PPS / APS / SEI, in
/// array order) followed by the item's length-prefixed NAL units,
/// every unit behind a 4-byte start code. An AUD or OPI at the head of
/// the item data stays first (14496-15 §11.2.4.2 NOTE 4). A `tols`
/// (`target_ols_idx`) given by the caller applies L.2.2.1.2's reader
/// rule: an OPI NAL unit in the item data whose `opi_ols_idx` differs
/// from it (or, without a `tols`, from the record's `ols_idx`) is
/// dropped — `opi_ols_idx` is parsed here only far enough to compare.
pub fn access_unit_annex_b(cfg: &VvcConfig, item: &[u8], tols: Option<u16>) -> Result<Vec<u8>> {
    let nals = crate::hvcc::split_length_prefixed(item, cfg.length_size)?;
    let mut out = Vec::with_capacity(item.len() + 128);
    let push = |out: &mut Vec<u8>, nal: &[u8]| {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    };
    let target = tols.or_else(|| cfg.ptl_present_flag.then_some(cfg.ols_idx));
    let keep_opi = |nal: &[u8]| -> bool {
        match (target, opi_ols_idx(nal)) {
            (Some(t), Some(o)) => u32::from(t) == o,
            _ => true,
        }
    };
    let mut rest = nals.as_slice();
    while let Some((first, tail)) = rest.split_first() {
        match nal_unit_type(first) {
            Some(NAL_AUD) => push(&mut out, first),
            Some(NAL_OPI) => {
                if keep_opi(first) {
                    push(&mut out, first);
                }
            }
            _ => break,
        }
        rest = tail;
    }
    for n in cfg.nal_units() {
        push(&mut out, n);
    }
    for n in rest {
        if nal_unit_type(n) == Some(NAL_OPI) && !keep_opi(n) {
            continue;
        }
        push(&mut out, n);
    }
    if out.is_empty() {
        return Err(HeifError::invalid("vvc1 item carries no NAL units"));
    }
    Ok(out)
}

/// `opi_ols_idx` of an OPI NAL unit (ISO/IEC 23090-3 §7.3.2.2), when
/// `opi_ols_info_present_flag` is set.
fn opi_ols_idx(nal: &[u8]) -> Option<u32> {
    let rbsp = extract_rbsp(nal.get(2..)?);
    let mut br = BitReader::new(&rbsp);
    let ols_present = br.u(1).ok()? == 1;
    let _htid_present = br.u(1).ok()?;
    if !ols_present {
        return None;
    }
    br.ue().ok()
}

/// Strip the emulation-prevention bytes of a NAL unit payload (the
/// bytes after the two-byte header): `00 00 03` → `00 00`.
pub fn extract_rbsp(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0usize;
    for &b in payload {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// MSB-first bit reader over an RBSP.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}
impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn bit_position(&self) -> usize {
        self.pos
    }
    fn u(&mut self, n: u32) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self
                .data
                .get(self.pos / 8)
                .ok_or_else(|| HeifError::invalid("VVC parameter set truncated"))?;
            v = (v << 1) | ((byte >> (7 - (self.pos % 8))) & 1) as u32;
            self.pos += 1;
        }
        Ok(v)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        if self.pos + n > self.data.len() * 8 {
            return Err(HeifError::invalid("VVC parameter set truncated"));
        }
        self.pos += n;
        Ok(())
    }
    fn align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }
    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0u32;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(HeifError::invalid("VVC Exp-Golomb code too long"));
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let rest = self.u(zeros)?;
        Ok(((1u64 << zeros) - 1 + rest as u64) as u32)
    }
}

/// The head of a VVC SPS parsed standalone (ISO/IEC 23090-3 §7.3.2.4
/// up to `sps_bitdepth_minus8`): what a `vvcC` without
/// `ptl_present_flag`, and the compact record of a `vvi3` file, need
/// from the bitstream. Subpicture layouts stop the parse
/// (`Unsupported`): their syntax precedes the bit depth and is not
/// needed by this crate.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SpsHead {
    /// `sps_seq_parameter_set_id`.
    pub sps_id: u8,
    /// `sps_video_parameter_set_id`.
    pub vps_id: u8,
    /// `sps_max_sublayers_minus1`.
    pub max_sublayers_minus1: u8,
    /// `sps_chroma_format_idc`.
    pub chroma_format_idc: u8,
    /// `sps_log2_ctu_size_minus5 + 5`.
    pub log2_ctu_size: u8,
    /// `profile_tier_level()` as a [`VvcPtlRecord`], when
    /// `sps_ptl_dpb_hrd_params_present_flag`.
    pub ptl: Option<VvcPtlRecord>,
    /// `sps_pic_width_max_in_luma_samples`.
    pub pic_width: u32,
    /// `sps_pic_height_max_in_luma_samples`.
    pub pic_height: u32,
    /// `sps_conf_win_{left,right,top,bottom}_offset` (chroma units).
    pub conformance_window: Option<(u32, u32, u32, u32)>,
    /// `sps_bitdepth_minus8 + 8`.
    pub bit_depth: u8,
}
impl SpsHead {
    /// Every field as a positional argument, in declaration order.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sps_id: u8,
        vps_id: u8,
        max_sublayers_minus1: u8,
        chroma_format_idc: u8,
        log2_ctu_size: u8,
        ptl: Option<VvcPtlRecord>,
        pic_width: u32,
        pic_height: u32,
        conformance_window: Option<(u32, u32, u32, u32)>,
        bit_depth: u8,
    ) -> Self {
        Self {
            sps_id,
            vps_id,
            max_sublayers_minus1,
            chroma_format_idc,
            log2_ctu_size,
            ptl,
            pic_width,
            pic_height,
            conformance_window,
            bit_depth,
        }
    }

    /// Parse from a complete SPS NAL unit (two-byte header included).
    pub fn parse_nal(nal: &[u8]) -> Result<Self> {
        if nal_unit_type(nal) != Some(NAL_SPS) {
            return Err(HeifError::invalid("not a VVC SPS NAL unit"));
        }
        Self::parse_rbsp(&extract_rbsp(&nal[2..]))
    }

    /// Parse from an SPS RBSP (emulation prevention removed).
    pub fn parse_rbsp(rbsp: &[u8]) -> Result<Self> {
        let mut br = BitReader::new(rbsp);
        let sps_id = br.u(4)? as u8;
        let vps_id = br.u(4)? as u8;
        let max_sublayers_minus1 = br.u(3)? as u8;
        let chroma_format_idc = br.u(2)? as u8;
        let log2_ctu_size = br.u(2)? as u8 + 5;
        let ptl_present = br.u(1)? == 1;
        let ptl = if ptl_present {
            Some(parse_ptl(&mut br, rbsp, true, max_sublayers_minus1)?)
        } else {
            None
        };
        let _gdr_enabled = br.u(1)?;
        let ref_pic_resampling = br.u(1)? == 1;
        if ref_pic_resampling {
            let _res_change = br.u(1)?;
        }
        let pic_width = br.ue()?;
        let pic_height = br.ue()?;
        let conformance_window = if br.u(1)? == 1 {
            Some((br.ue()?, br.ue()?, br.ue()?, br.ue()?))
        } else {
            None
        };
        if br.u(1)? == 1 {
            return Err(HeifError::unsupported(
                "VVC SPS with subpicture information: the sample layout comes from the vvcC head",
            ));
        }
        let bit_depth = br.ue()?;
        if bit_depth > 8 {
            return Err(HeifError::invalid(format!(
                "VVC sps_bitdepth_minus8 {bit_depth}"
            )));
        }
        Ok(Self {
            sps_id,
            vps_id,
            max_sublayers_minus1,
            chroma_format_idc,
            log2_ctu_size,
            ptl,
            pic_width,
            pic_height,
            conformance_window,
            bit_depth: bit_depth as u8 + 8,
        })
    }

    /// The cropped picture size in luma samples (the conformance
    /// window applied at the chroma format's `SubWidthC` /
    /// `SubHeightC`).
    pub fn cropped_size(&self) -> (u32, u32) {
        let (sw, sh): (u32, u32) = match self.chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        match self.conformance_window {
            // Hostile ue(v) offsets may be anything up to 2^32 - 2:
            // saturate the whole derivation (a conformant window is a
            // few samples; a non-conformant one yields a 1×1 picture
            // rather than a panic).
            Some((l, r, t, b)) => (
                self.pic_width
                    .saturating_sub(sw.saturating_mul(l.saturating_add(r)))
                    .max(1),
                self.pic_height
                    .saturating_sub(sh.saturating_mul(t.saturating_add(b)))
                    .max(1),
            ),
            None => (self.pic_width, self.pic_height),
        }
    }
}

/// The head of a VVC VPS parsed standalone (ISO/IEC 23090-3 §7.3.2.3,
/// the single-layer shape): its first `profile_tier_level()`, which is
/// where a single-layer still's PTL lives when the SPS carries none
/// (`sps_ptl_dpb_hrd_params_present_flag = 0`).
pub fn vps_first_ptl(nal: &[u8]) -> Result<VvcPtlRecord> {
    if nal_unit_type(nal) != Some(NAL_VPS) {
        return Err(HeifError::invalid("not a VVC VPS NAL unit"));
    }
    let rbsp = extract_rbsp(&nal[2..]);
    let mut br = BitReader::new(&rbsp);
    let _vps_id = br.u(4)?;
    let max_layers_minus1 = br.u(6)?;
    let max_sublayers_minus1 = br.u(3)? as u8;
    if max_layers_minus1 != 0 {
        return Err(HeifError::unsupported(
            "multi-layer VVC VPS: the PTL is taken from the SPS or the vvcC head",
        ));
    }
    // Single layer (§7.3.2.3 / §7.4.3.3): vps_default_ptl_dpb_hrd_max_tid_flag
    // and vps_all_independent_layers_flag are absent (inferred 1), the
    // layer loop codes vps_layer_id[0] u(6) alone, vps_num_ptls_minus1
    // is absent (inferred 0) and so is vps_pt_present_flag[0]
    // (inferred 1) / vps_ptl_max_tid[0] (inferred
    // vps_max_sublayers_minus1); the PTL follows the alignment bits.
    let _layer_id0 = br.u(6)?;
    br.align();
    parse_ptl(&mut br, &rbsp, true, max_sublayers_minus1)
}

/// `profile_tier_level(profileTierPresentFlag, maxNumSubLayersMinus1)`
/// (§7.3.3.1) into a [`VvcPtlRecord`]; the `general_constraints_info()`
/// run is copied byte-exact from the RBSP (71 fixed bits,
/// `gci_num_additional_bits`, the additional bits, alignment).
fn parse_ptl(
    br: &mut BitReader<'_>,
    rbsp: &[u8],
    profile_tier_present: bool,
    max_sublayers_minus1: u8,
) -> Result<VvcPtlRecord> {
    let mut ptl = VvcPtlRecord::default();
    if profile_tier_present {
        ptl.general_profile_idc = br.u(7)? as u8;
        ptl.general_tier_flag = br.u(1)? == 1;
    }
    ptl.general_level_idc = br.u(8)? as u8;
    let flags_bit = br.bit_position();
    ptl.ptl_frame_only_constraint_flag = br.u(1)? == 1;
    ptl.ptl_multilayer_enabled_flag = br.u(1)? == 1;
    if profile_tier_present {
        let gci_present = br.u(1)? == 1;
        if gci_present {
            br.skip(71)?;
            let additional = br.u(8)? as usize;
            br.skip(additional)?;
        }
        br.align();
        // The run starts two bits before a byte boundary only when the
        // PTL itself is byte-aligned (it is, in the SPS and the VPS).
        if flags_bit % 8 != 0 {
            return Err(HeifError::invalid(
                "VVC profile_tier_level not byte aligned",
            ));
        }
        let start = flags_bit / 8;
        let end = br.bit_position() / 8;
        let mut gci = rbsp
            .get(start..end)
            .ok_or_else(|| HeifError::invalid("VVC parameter set truncated"))?
            .to_vec();
        if let Some(b0) = gci.first_mut() {
            *b0 &= 0x3f;
        }
        ptl.general_constraint_info = gci;
    } else {
        ptl.general_constraint_info = vec![0];
    }
    let sub = max_sublayers_minus1 as usize;
    let mut present = vec![false; sub];
    for i in (0..sub).rev() {
        present[i] = br.u(1)? == 1;
    }
    if sub > 0 {
        for _ in max_sublayers_minus1..8 {
            let _reserved = br.u(1)?;
        }
    }
    let mut levels = vec![None; sub];
    for i in (0..sub).rev() {
        if present[i] {
            levels[i] = Some(br.u(8)? as u8);
        }
    }
    ptl.sublayer_level_idc = levels;
    if profile_tier_present {
        let n = br.u(8)? as usize;
        for _ in 0..n {
            ptl.general_sub_profile_idc.push(br.u(32)?);
        }
    }
    Ok(ptl)
}

/// `CompactVvcDecoderConfigurationRecord` (HEIF Amd 2:2026 L.4.3.3):
/// the codec configuration of a `vvi3` low-overhead file.
///
/// ```text
/// aligned(8) class CompactVvcDecoderConfigurationRecord {
///   unsigned int(1) multi_layer_flag;
///   unsigned int(2) lengthSizeMinusOne;
///   unsigned int(5) num_nal_units_minus2;
///   for (i = 0; i < num_nal_units_minus2 + 2; i++) {
///     unsigned int(8) nal_unit_length[i];
///     bit(8 * nal_unit_length) nal_unit[i];
///   }
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompactVvcConfig {
    /// `multi_layer_flag`: 0 — the item data is one `IDR_N_LP` NAL
    /// unit without its length and header fields; 1 — any number of
    /// length-prefixed NAL units.
    pub multi_layer_flag: bool,
    /// `lengthSizeMinusOne + 1`.
    pub length_size: u8,
    /// The NAL units (at least SPS + PPS; VPS too when multi-layer).
    pub nal_units: Vec<Vec<u8>>,
}
impl CompactVvcConfig {
    /// Every field as a positional argument, in declaration order.
    pub fn new(multi_layer_flag: bool, length_size: u8, nal_units: Vec<Vec<u8>>) -> Self {
        Self {
            multi_layer_flag,
            length_size,
            nal_units,
        }
    }

    /// Parse the compact record.
    pub fn parse(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let b0 = r.u8("compact vvcC head")?;
        let multi_layer_flag = b0 & 0x80 != 0;
        let length_size = ((b0 >> 5) & 0x03) + 1;
        let n = (b0 & 0x1f) as usize + 2;
        let mut nal_units = Vec::with_capacity(n);
        for _ in 0..n {
            let len = r.u8("compact vvcC nal_unit_length")? as usize;
            nal_units.push(r.bytes(len, "compact vvcC nal_unit")?.to_vec());
        }
        if !r.is_empty() {
            return Err(HeifError::invalid(format!(
                "compact vvcC record has {} trailing bytes",
                r.remaining()
            )));
        }
        Ok(Self {
            multi_layer_flag,
            length_size,
            nal_units,
        })
    }

    /// Serialize the compact record.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        if self.nal_units.len() < 2 || self.nal_units.len() > 33 {
            return Err(HeifError::invalid(
                "compact vvcC record holds 2..=33 NAL units",
            ));
        }
        let mut b = vec![
            ((self.multi_layer_flag as u8) << 7)
                | ((self.length_size.saturating_sub(1) & 0x03) << 5)
                | ((self.nal_units.len() - 2) as u8),
        ];
        for n in &self.nal_units {
            if n.len() > 255 {
                return Err(HeifError::invalid(
                    "compact vvcC NAL unit longer than 255 bytes",
                ));
            }
            b.push(n.len() as u8);
            b.extend_from_slice(n);
        }
        Ok(b)
    }

    /// The equivalent `VvcDecoderConfigurationRecord` (L.4.3.3.4):
    /// `ptl_present_flag` 1, one sublayer, `constant_frame_rate` 1,
    /// the chroma format / bit depth / picture size supplied by the
    /// caller from the `MinimizedImageBox`, `avg_frame_rate` 0, the
    /// `native_ptl` from the SPS (or the VPS when the SPS carries no
    /// PTL), one complete array per NAL unit type in first-appearance
    /// order. With `multi_layer_flag` 1 the operating point comes from
    /// the VPS: a single-layer VPS has one output layer set (`ols_idx`
    /// 0) and its first PTL; a VPS with several layers needs the OLS
    /// derivation this crate does not reconstruct (`Unsupported`).
    pub fn to_full(
        &self,
        chroma_format_idc: u8,
        bit_depth: u8,
        width: u32,
        height: u32,
    ) -> Result<VvcConfig> {
        let vps = self
            .nal_units
            .iter()
            .find(|n| nal_unit_type(n) == Some(NAL_VPS));
        if self.multi_layer_flag {
            // L.4.3.3.4: at least VPS + SPS + PPS, the operating point
            // from the VPS. `vps_first_ptl` refuses a multi-layer VPS.
            let vps = vps.ok_or_else(|| {
                HeifError::invalid("compact vvcC with multi_layer_flag but no VPS")
            })?;
            vps_first_ptl(vps).map_err(|e| match e {
                HeifError::Unsupported(_) => HeifError::unsupported(
                    "compact vvcC with multi_layer_flag over a multi-layer VPS: the equivalent record's operating point (HEIF Amd 2 L.4.3.3.4) is not reconstructed by this crate",
                ),
                other => other,
            })?;
        }
        let sps = self
            .nal_units
            .iter()
            .find(|n| nal_unit_type(n) == Some(NAL_SPS))
            .ok_or_else(|| HeifError::invalid("compact vvcC record without an SPS"))?;
        let head = SpsHead::parse_nal(sps)?;
        let ptl = match head.ptl {
            Some(p) => p,
            None => {
                let vps = vps.ok_or_else(|| {
                    HeifError::invalid(
                        "compact vvcC record: the SPS carries no PTL and there is no VPS",
                    )
                })?;
                vps_first_ptl(vps)?
            }
        };
        let mut arrays: Vec<VvcNalArray> = Vec::new();
        for n in &self.nal_units {
            let t = nal_unit_type(n).unwrap_or(31);
            match arrays.iter_mut().find(|a| a.nal_unit_type == t) {
                Some(a) => a.nal_units.push(n.clone()),
                None => arrays.push(VvcNalArray {
                    complete: true,
                    nal_unit_type: t,
                    nal_units: vec![n.clone()],
                }),
            }
        }
        Ok(VvcConfig {
            flags: 0,
            length_size: self.length_size,
            ptl_present_flag: true,
            ols_idx: 0,
            num_sublayers: 1,
            constant_frame_rate: 1,
            chroma_format_idc,
            bit_depth_minus8: bit_depth.saturating_sub(8),
            native_ptl: Some(ptl),
            max_picture_width: width.min(u16::MAX as u32) as u16,
            max_picture_height: height.min(u16::MAX as u32) as u16,
            avg_frame_rate: 0,
            arrays,
            raw: Vec::new(),
        })
    }

    /// The `VVCItemData` of the equivalent file for this record's item
    /// data (L.4.3.3.3): with `multi_layer_flag` 0 the stored bytes are
    /// one `IDR_N_LP` NAL unit payload without header or length, so the
    /// two-byte header (`nal_unit_type` 8, `nuh_layer_id` 0,
    /// `nuh_temporal_id_plus1` 1) and a `length_size` prefix are
    /// restored; with `multi_layer_flag` 1 the bytes are already
    /// length-prefixed NAL units.
    pub fn item_data(&self, stored: &[u8]) -> Result<Vec<u8>> {
        if self.multi_layer_flag {
            return Ok(stored.to_vec());
        }
        let mut nal = nal_header(NAL_IDR_N_LP, 0, 1).to_vec();
        nal.extend_from_slice(stored);
        crate::hvcc::join_length_prefixed(&[&nal], self.length_size)
    }

    /// The compact form of a single-layer item: the record's NAL
    /// units with the item's one `IDR_N_LP` NAL unit stripped of its
    /// length prefix and header (`multi_layer_flag` 0), when the item
    /// data is exactly that; else `None` (the item needs
    /// `multi_layer_flag` 1).
    pub fn strip_item_data(&self, item_data: &[u8]) -> Option<Vec<u8>> {
        if self.multi_layer_flag {
            return Some(item_data.to_vec());
        }
        let nals = crate::hvcc::split_length_prefixed(item_data, self.length_size).ok()?;
        let [one] = nals.as_slice() else {
            return None;
        };
        if nal_unit_type(one) != Some(NAL_IDR_N_LP)
            || nal_layer_id(one) != Some(0)
            || nal_temporal_id_plus1(one) != Some(1)
        {
            return None;
        }
        Some(one[2..].to_vec())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The parameter sets of an 8×8 8-bit 4:2:0 IDR picture coded by
    /// the workspace's own VVC encoder (VPS, SPS, PPS), as NAL units.
    pub(crate) const VPS: [u8; 11] = [
        0x00, 0x71, 0x10, 0x00, 0x00, 0x03, 0x02, 0x5a, 0x80, 0x00, 0x40,
    ];
    pub(crate) const SPS: [u8; 16] = [
        0x00, 0x79, 0x00, 0x0c, 0x04, 0x89, 0x22, 0x02, 0xdc, 0x3d, 0x30, 0x30, 0x10, 0x40, 0x00,
        0x20,
    ];
    pub(crate) const PPS: [u8; 8] = [0x00, 0x81, 0x00, 0x02, 0x44, 0x89, 0x86, 0x08];
    /// The picture header and the one `IDR_N_LP` slice of that picture.
    pub(crate) const PH: [u8; 5] = [0x00, 0x99, 0x88, 0x00, 0xb8];
    pub(crate) const SLICE: [u8; 24] = [
        0x00, 0x41, 0x20, 0x02, 0x30, 0xe3, 0x2b, 0xa6, 0x10, 0xba, 0xea, 0x81, 0x35, 0x9c, 0x7d,
        0xdf, 0x25, 0xd7, 0x6f, 0x11, 0x26, 0x45, 0xdc, 0x5e,
    ];

    /// A record with the single-layer still shape this crate writes.
    pub(crate) fn sample_record() -> Vec<u8> {
        let mut b = vec![
            0xff, // reserved 11111, LengthSizeMinusOne 3, ptl_present 1
            0x00, 0x11, // ols_idx 0, num_sublayers 1, cfr 0, chroma 1
            0x1f, // bit_depth_minus8 0, reserved 11111
            0x01, // num_bytes_constraint_info 1
            0x02, // profile 1, tier 0
            0x5a, // level
            0x80, // frame_only 1, multilayer 0, gci_present 0
            0x00, // num_sub_profiles
            0x00, 0x08, 0x00, 0x08, // max w/h
            0x00, 0x00, // avg_frame_rate
            0x03, // arrays
        ];
        for (t, nal) in [
            (NAL_VPS, &VPS[..]),
            (NAL_SPS, &SPS[..]),
            (NAL_PPS, &PPS[..]),
        ] {
            b.push(0x80 | t);
            b.extend_from_slice(&1u16.to_be_bytes());
            b.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            b.extend_from_slice(nal);
        }
        b
    }

    #[test]
    fn parses_and_reserializes_byte_exact() {
        let raw = sample_record();
        let c = VvcConfig::parse(&raw).unwrap();
        assert_eq!(c.length_size, 4);
        assert!(c.ptl_present_flag);
        assert_eq!(c.ols_idx, 0);
        assert_eq!(c.num_sublayers, 1);
        assert_eq!(c.chroma_format_idc, 1);
        assert_eq!(c.bit_depth(), 8);
        assert_eq!((c.max_picture_width, c.max_picture_height), (8, 8));
        let ptl = c.native_ptl.as_ref().unwrap();
        assert_eq!(ptl.general_profile_idc, 1);
        assert_eq!(ptl.general_level_idc, 0x5a);
        assert!(ptl.ptl_frame_only_constraint_flag);
        assert_eq!(ptl.general_constraint_info, vec![0]);
        assert_eq!(c.arrays.len(), 3);
        assert_eq!(c.nal_count(), 3);
        assert_eq!(c.sps(), Some(&SPS[..]));
        assert_eq!(c.to_bytes(), raw);
        assert_eq!(c.serialize(), raw);
        assert_eq!(c.sample_format().unwrap(), (1, 8));
    }

    #[test]
    fn sps_head_matches_the_record() {
        let h = SpsHead::parse_nal(&SPS).unwrap();
        assert_eq!(h.chroma_format_idc, 1);
        assert_eq!(h.bit_depth, 8);
        assert_eq!((h.pic_width, h.pic_height), (8, 8));
        assert_eq!(h.cropped_size(), (8, 8));
        assert!(h.ptl.is_none(), "this encoder puts the PTL in the VPS");
        let ptl = vps_first_ptl(&VPS).unwrap();
        assert_eq!(ptl.general_profile_idc, 1);
        assert_eq!(ptl.general_level_idc, 0x5a);
        assert!(ptl.ptl_frame_only_constraint_flag);
        assert!(!ptl.ptl_multilayer_enabled_flag);
        assert_eq!(ptl.general_constraint_info, vec![0]);
        assert!(ptl.general_sub_profile_idc.is_empty());
    }

    #[test]
    fn record_without_ptl_takes_the_layout_from_the_sps() {
        let raw = sample_record();
        let mut c = VvcConfig::parse(&raw).unwrap();
        c.ptl_present_flag = false;
        c.raw.clear();
        let bytes = c.serialize();
        let back = VvcConfig::parse(&bytes).unwrap();
        assert!(!back.ptl_present_flag);
        assert_eq!(back.sample_format().unwrap(), (1, 8));
        assert_eq!(back.serialize(), bytes);
    }

    #[test]
    fn sublayer_levels_round_trip() {
        let mut c = VvcConfig::parse(&sample_record()).unwrap();
        c.raw.clear();
        c.num_sublayers = 3;
        let ptl = c.native_ptl.as_mut().unwrap();
        ptl.sublayer_level_idc = vec![Some(0x30), None];
        ptl.general_sub_profile_idc = vec![0xdead_beef];
        ptl.general_constraint_info = vec![0x20, 0x01, 0x00];
        let bytes = c.serialize();
        let back = VvcConfig::parse(&bytes).unwrap();
        assert_eq!(back.num_sublayers, 3);
        let p = back.native_ptl.as_ref().unwrap();
        assert_eq!(p.sublayer_level_idc, vec![Some(0x30), None]);
        assert_eq!(p.general_sub_profile_idc, vec![0xdead_beef]);
        assert_eq!(p.general_constraint_info, vec![0x20, 0x01, 0x00]);
        assert_eq!(p.num_bytes_constraint_info(), 3);
        assert_eq!(back.serialize(), bytes);
    }

    #[test]
    fn rejects_truncation_and_bad_length_size() {
        let raw = sample_record();
        assert!(VvcConfig::parse(&raw[..raw.len() - 1]).is_err());
        assert!(VvcConfig::parse(&raw[..6]).is_err());
        let mut bad = raw.clone();
        bad[0] = 0xfd; // LengthSizeMinusOne 2
        assert!(VvcConfig::parse(&bad).is_err());
        let mut zero = raw;
        zero[4] = 0; // num_bytes_constraint_info 0
        assert!(VvcConfig::parse(&zero).is_err());
    }

    #[test]
    fn annex_b_reassembly_orders_aud_and_filters_opi() {
        let c = VvcConfig::parse(&sample_record()).unwrap();
        let slice = [0x00, 0x41, 0xaa, 0xbb];
        let aud = [0x00, 0xa1, 0x50];
        // OPI with opi_ols_info_present_flag 1, htid 0, opi_ols_idx = 1 (ue 010).
        let opi = [0x00, 0x61, 0b1001_0000];
        let item = crate::hvcc::join_length_prefixed(&[&aud, &opi, &slice], 4).unwrap();
        let bs = access_unit_annex_b(&c, &item, None).unwrap();
        let nals = crate::hvcc::split_annex_b(&bs);
        // AUD first, OPI dropped (record ols_idx 0 != 1), record, slice.
        assert_eq!(nals.len(), 5);
        assert_eq!(nals[0], &aud);
        assert_eq!(nals[1], &VPS);
        assert_eq!(nals[4], &slice);
        let kept = access_unit_annex_b(&c, &item, Some(1)).unwrap();
        assert_eq!(crate::hvcc::split_annex_b(&kept).len(), 6);
        assert_eq!(opi_ols_idx(&opi), Some(1));
        assert!(access_unit_annex_b(&c, &[], None).is_ok());
        assert!(access_unit_annex_b(&c, &[0, 0], None).is_err());
    }

    #[test]
    fn hostile_conformance_window_saturates() {
        let mut h = SpsHead::parse_nal(&SPS).unwrap();
        h.conformance_window = Some((u32::MAX - 1, u32::MAX - 1, 7, u32::MAX / 2));
        assert_eq!(h.cropped_size(), (1, 1));
        h.conformance_window = Some((1, 1, 0, 0));
        assert_eq!(h.cropped_size(), (4, 8));
    }

    /// The Fuzz workflow's `heif_records` crash unit (r473): a record
    /// head whose bytes, read as an SPS RBSP, carry conformance-window
    /// offsets near 2^32 — `SubWidthC × (left + right)` overflowed.
    /// Every parser the target drives must return, not panic.
    #[test]
    fn fuzz_unit_conformance_window_overflow_returns() {
        const UNIT: [u8; 27] = [
            0xff, 0x01, 0x0c, 0x00, 0x00, 0x01, 0x00, 0x00, 0xfe, 0x03, 0x00, 0x1b, 0x00, 0x00,
            0x00, 0x00, 0x86, 0x00, 0x01, 0x2c, 0x66, 0x72, 0x65, 0x65, 0x00, 0x00, 0x03,
        ];
        if let Ok(c) = VvcConfig::parse(&UNIT) {
            let _ = c.sample_format();
            let again = VvcConfig::parse(&c.serialize()).unwrap();
            assert_eq!(again.serialize(), c.serialize());
            let _ = access_unit_annex_b(&c, &UNIT, None);
        }
        if let Ok(h) = SpsHead::parse_rbsp(&UNIT) {
            let (w, hgt) = h.cropped_size();
            assert!(w >= 1 && hgt >= 1);
        }
        let _ = CompactVvcConfig::parse(&UNIT);
        let _ = vps_first_ptl(&UNIT);
    }

    #[test]
    fn nal_header_helpers() {
        let h = nal_header(NAL_IDR_N_LP, 0, 1);
        assert_eq!(h, [0x00, 0x41]);
        assert_eq!(nal_unit_type(&h), Some(NAL_IDR_N_LP));
        assert_eq!(nal_layer_id(&h), Some(0));
        assert_eq!(nal_temporal_id_plus1(&h), Some(1));
        assert_eq!(nal_unit_type(&SPS), Some(NAL_SPS));
        assert_eq!(nal_unit_type(&PPS), Some(NAL_PPS));
        assert!(is_vcl(NAL_IDR_N_LP) && !is_vcl(NAL_SPS));
        assert!(is_record_nal_type(NAL_PREFIX_APS) && !is_record_nal_type(NAL_PH));
        assert_eq!(extract_rbsp(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
    }

    #[test]
    fn compact_record_round_trips_and_expands() {
        let compact = CompactVvcConfig {
            multi_layer_flag: false,
            length_size: 4,
            nal_units: vec![VPS.to_vec(), SPS.to_vec(), PPS.to_vec()],
        };
        let bytes = compact.serialize().unwrap();
        assert_eq!(bytes[0], 0x60 | 1); // single layer, lengthSizeMinusOne 3, 3 - 2
        assert_eq!(CompactVvcConfig::parse(&bytes).unwrap(), compact);
        let full = compact.to_full(1, 8, 8, 8).unwrap();
        assert_eq!(
            full.serialize(),
            sample_record()[..]
                .to_vec()
                .iter()
                .enumerate()
                .map(|(i, b)| if i == 2 { 0x15 } else { *b })
                .collect::<Vec<u8>>(),
            "the equivalent record is the sample record with constant_frame_rate 1"
        );
        let payload = [0xaa, 0xbb, 0xcc];
        let item = compact.item_data(&payload).unwrap();
        assert_eq!(item, [0, 0, 0, 5, 0x00, 0x41, 0xaa, 0xbb, 0xcc]);
        assert_eq!(compact.strip_item_data(&item).unwrap(), payload);
        let two = crate::hvcc::join_length_prefixed(&[&item[4..], &item[4..]], 4).unwrap();
        assert!(compact.strip_item_data(&two).is_none());
        assert!(CompactVvcConfig::parse(&bytes[..bytes.len() - 1]).is_err());
        // This encoder's stills carry a separate PH NAL unit, so their
        // compact form is multi_layer_flag 1 over the single-layer VPS:
        // the item data stays length-prefixed and the equivalent record
        // is the same single-operating-point one.
        let ml = CompactVvcConfig {
            multi_layer_flag: true,
            ..compact.clone()
        };
        let au = crate::hvcc::join_length_prefixed(&[&PH, &SLICE], 4).unwrap();
        assert_eq!(ml.item_data(&au).unwrap(), au);
        assert_eq!(ml.strip_item_data(&au).unwrap(), au);
        assert_eq!(
            ml.to_full(1, 8, 8, 8).unwrap().serialize(),
            full.serialize()
        );
        let no_vps = CompactVvcConfig {
            multi_layer_flag: true,
            length_size: 4,
            nal_units: vec![SPS.to_vec(), PPS.to_vec()],
        };
        assert!(no_vps.to_full(1, 8, 8, 8).is_err());
        let bytes = ml.serialize().unwrap();
        assert_eq!(bytes[0] & 0x80, 0x80);
        assert_eq!(CompactVvcConfig::parse(&bytes).unwrap(), ml);
    }
}
