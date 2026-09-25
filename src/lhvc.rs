//! Layered HEVC (L-HEVC) item properties: the
//! `LHEVCDecoderConfigurationRecord` (`lhvC`, ISO/IEC 14496-15 §9.5),
//! the `OperatingPointsRecord` (`oinf`, §9.6.2.2, carried by the HEIF
//! B.2.3.3 property) and the `TargetOlsProperty` (`tols`, HEIF §6.5.29).
//!
//! An `lhv1` item (HEIF B.2.2.1.3) is one access unit that may hold
//! coded pictures of several layers; its `tols` names the output layer
//! set to decode. This crate types the layer structure and decodes the
//! base layer (`nuh_layer_id` 0) through the HEVC decoder; enhancement
//! layers are a typed refusal ([`crate::HeifError::LayeredHevc`]).
//!
//! ```text
//! LHEVCDecoderConfigurationRecord {
//!   unsigned int(8) configurationVersion = 1;
//!   bit(4) reserved; unsigned int(12) min_spatial_segmentation_idc;
//!   bit(6) reserved; unsigned int(2)  parallelismType;
//!   bit(2) reserved; unsigned int(3)  numTemporalLayers;
//!   unsigned int(1) temporalIdNested; unsigned int(2) lengthSizeMinusOne;
//!   unsigned int(8) numOfArrays;
//!   for (j) { array_completeness(1) reserved(1) NAL_unit_type(6)
//!             numNalus(16) { nalUnitLength(16) nalUnit } }
//! }
//! ```

use crate::boxes::Reader;
use crate::error::{HeifError, Result};
use crate::hvcc::NalArray;

/// Parsed `lhvC` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LhevcConfig {
    /// `configurationVersion` (1).
    pub configuration_version: u8,
    /// `min_spatial_segmentation_idc`.
    pub min_spatial_segmentation_idc: u16,
    /// `parallelismType`.
    pub parallelism_type: u8,
    /// `numTemporalLayers`.
    pub num_temporal_layers: u8,
    /// `temporalIdNested`.
    pub temporal_id_nested: bool,
    /// `lengthSizeMinusOne + 1`.
    pub length_size: u8,
    /// The parameter-set / SEI arrays.
    pub arrays: Vec<NalArray>,
    /// The record bytes as parsed / serialized.
    pub raw: Vec<u8>,
}

impl LhevcConfig {
    /// Parse an `lhvC` record body.
    pub fn parse(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let configuration_version = r.u8("lhvC configurationVersion")?;
        if configuration_version != 1 {
            return Err(HeifError::invalid(format!(
                "lhvC configurationVersion {configuration_version} (shall be 1)"
            )));
        }
        let min_spatial_segmentation_idc = r.u16("lhvC min_spatial_segmentation_idc")? & 0x0fff;
        let parallelism_type = r.u8("lhvC parallelismType")? & 0x03;
        let t = r.u8("lhvC numTemporalLayers")?;
        let num_temporal_layers = (t >> 3) & 0x07;
        let temporal_id_nested = (t >> 2) & 1 == 1;
        let length_size = (t & 0x03) + 1;
        let n = r.u8("lhvC numOfArrays")? as usize;
        let mut arrays = Vec::with_capacity(n);
        for _ in 0..n {
            let h = r.u8("lhvC array header")?;
            let count = r.u16("lhvC numNalus")? as usize;
            let mut nal_units = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                let len = r.u16("lhvC nalUnitLength")? as usize;
                nal_units.push(r.bytes(len, "lhvC nalUnit")?.to_vec());
            }
            arrays.push(NalArray {
                complete: h >> 7 == 1,
                reserved_bit: (h >> 6) & 1 == 1,
                nal_unit_type: h & 0x3f,
                nal_units,
            });
        }
        Ok(Self {
            configuration_version,
            min_spatial_segmentation_idc,
            parallelism_type,
            num_temporal_layers,
            temporal_id_nested,
            length_size,
            arrays,
            raw: b.to_vec(),
        })
    }

    /// Serialize to record bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&(0xf000 | (self.min_spatial_segmentation_idc & 0x0fff)).to_be_bytes());
        v.push(0xfc | (self.parallelism_type & 0x03));
        v.push(
            ((self.num_temporal_layers & 0x07) << 3)
                | ((self.temporal_id_nested as u8) << 2)
                | ((self.length_size.clamp(1, 4) - 1) & 0x03),
        );
        v.push(self.arrays.len().min(255) as u8);
        for a in self.arrays.iter().take(255) {
            v.push(
                ((a.complete as u8) << 7)
                    | ((a.reserved_bit as u8) << 6)
                    | (a.nal_unit_type & 0x3f),
            );
            v.extend_from_slice(&(a.nal_units.len().min(65535) as u16).to_be_bytes());
            for n in a.nal_units.iter().take(65535) {
                v.extend_from_slice(&(n.len() as u16).to_be_bytes());
                v.extend_from_slice(n);
            }
        }
        v
    }

    /// Every NAL unit of every array, in record order.
    pub fn nal_units(&self) -> impl Iterator<Item = &[u8]> {
        self.arrays
            .iter()
            .flat_map(|a| a.nal_units.iter().map(Vec::as_slice))
    }
}

/// One profile / tier / level entry of an `oinf` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperatingPointPtl {
    /// `general_profile_space`.
    pub profile_space: u8,
    /// `general_tier_flag`.
    pub tier_flag: bool,
    /// `general_profile_idc`.
    pub profile_idc: u8,
    /// `general_profile_compatibility_flags`.
    pub profile_compatibility_flags: u32,
    /// `general_constraint_indicator_flags` (48 bits).
    pub constraint_indicator_flags: u64,
    /// `general_level_idc`.
    pub level_idc: u8,
}

/// One layer of an operating point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperatingPointLayer {
    /// `ptl_idx` (1-based into the PTL list; 0 = none).
    pub ptl_idx: u8,
    /// `layer_id` (`nuh_layer_id`).
    pub layer_id: u8,
    /// `is_outputlayer`.
    pub is_output_layer: bool,
    /// `is_alternate_outputlayer`.
    pub is_alternate_output_layer: bool,
}

/// One operating point of an `oinf` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperatingPoint {
    /// `output_layer_set_idx`.
    pub output_layer_set_idx: u16,
    /// `max_temporal_id`.
    pub max_temporal_id: u8,
    /// The layers.
    pub layers: Vec<OperatingPointLayer>,
    /// `minPicWidth` / `minPicHeight`.
    pub min_pic_size: (u16, u16),
    /// `maxPicWidth` / `maxPicHeight`.
    pub max_pic_size: (u16, u16),
    /// `maxChromaFormat`.
    pub max_chroma_format: u8,
    /// `maxBitDepthMinus8`.
    pub max_bit_depth_minus8: u8,
    /// `avgFrameRate` / `constantFrameRate` when `frame_rate_info_flag`.
    pub frame_rate: Option<(u16, u8)>,
    /// `maxBitRate` / `avgBitRate` when `bit_rate_info_flag`.
    pub bit_rate: Option<(u32, u32)>,
}

/// One layer's dependency entry of an `oinf` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerDependency {
    /// `layerID`.
    pub layer_id: u8,
    /// `direct_ref_layerID[]`.
    pub direct_ref_layer_ids: Vec<u8>,
    /// `dimension_identifier[j]` for every set bit `j` of `scalability_mask`.
    pub dimension_identifiers: Vec<(u8, u8)>,
}

/// Parsed `OperatingPointsRecord` (`oinf`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperatingPoints {
    /// `scalability_mask`.
    pub scalability_mask: u16,
    /// The PTL list (`ptl_idx` is 1-based into it).
    pub ptls: Vec<OperatingPointPtl>,
    /// The operating points.
    pub operating_points: Vec<OperatingPoint>,
    /// Per-layer dependency information.
    pub layers: Vec<LayerDependency>,
    /// The record bytes as parsed.
    pub raw: Vec<u8>,
}

impl OperatingPoints {
    /// Parse an `OperatingPointsRecord` (the body of the `oinf`
    /// ItemFullProperty after its version / flags).
    pub fn parse(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let scalability_mask = r.u16("oinf scalability_mask")?;
        let n_ptl = (r.u8("oinf num_profile_tier_level")? & 0x3f) as usize;
        let mut ptls = Vec::with_capacity(n_ptl);
        for _ in 0..n_ptl {
            let b0 = r.u8("oinf general_profile")?;
            let profile_compatibility_flags = r.u32("oinf general_profile_compatibility_flags")?;
            let c = r.bytes(6, "oinf general_constraint_indicator_flags")?;
            let constraint_indicator_flags = c.iter().fold(0u64, |acc, x| (acc << 8) | *x as u64);
            let level_idc = r.u8("oinf general_level_idc")?;
            ptls.push(OperatingPointPtl {
                profile_space: b0 >> 6,
                tier_flag: (b0 >> 5) & 1 == 1,
                profile_idc: b0 & 0x1f,
                profile_compatibility_flags,
                constraint_indicator_flags,
                level_idc,
            });
        }
        let n_ops = r.u16("oinf num_operating_points")? as usize;
        if n_ops > r.remaining() / 12 {
            return Err(HeifError::invalid(
                "oinf num_operating_points exceeds the record",
            ));
        }
        let mut operating_points = Vec::with_capacity(n_ops);
        for _ in 0..n_ops {
            let output_layer_set_idx = r.u16("oinf output_layer_set_idx")?;
            let max_temporal_id = r.u8("oinf max_temporal_id")?;
            let layer_count = r.u8("oinf layer_count")? as usize;
            let mut layers = Vec::with_capacity(layer_count);
            for _ in 0..layer_count {
                let ptl_idx = r.u8("oinf ptl_idx")?;
                let l = r.u8("oinf layer_id")?;
                layers.push(OperatingPointLayer {
                    ptl_idx,
                    layer_id: l >> 2,
                    is_output_layer: (l >> 1) & 1 == 1,
                    is_alternate_output_layer: l & 1 == 1,
                });
            }
            let min_pic_size = (r.u16("oinf minPicWidth")?, r.u16("oinf minPicHeight")?);
            let max_pic_size = (r.u16("oinf maxPicWidth")?, r.u16("oinf maxPicHeight")?);
            let f = r.u8("oinf maxChromaFormat")?;
            let max_chroma_format = f >> 6;
            let max_bit_depth_minus8 = (f >> 3) & 0x07;
            let frame_rate_info = (f >> 1) & 1 == 1;
            let bit_rate_info = f & 1 == 1;
            let frame_rate = if frame_rate_info {
                let avg = r.u16("oinf avgFrameRate")?;
                let c = r.u8("oinf constantFrameRate")? & 0x03;
                Some((avg, c))
            } else {
                None
            };
            let bit_rate = if bit_rate_info {
                Some((r.u32("oinf maxBitRate")?, r.u32("oinf avgBitRate")?))
            } else {
                None
            };
            operating_points.push(OperatingPoint {
                output_layer_set_idx,
                max_temporal_id,
                layers,
                min_pic_size,
                max_pic_size,
                max_chroma_format,
                max_bit_depth_minus8,
                frame_rate,
                bit_rate,
            });
        }
        let max_layer_count = r.u8("oinf max_layer_count")? as usize;
        let mut layers = Vec::with_capacity(max_layer_count);
        for _ in 0..max_layer_count {
            let layer_id = r.u8("oinf layerID")?;
            let n_ref = r.u8("oinf num_direct_ref_layers")? as usize;
            let mut direct_ref_layer_ids = Vec::with_capacity(n_ref);
            for _ in 0..n_ref {
                direct_ref_layer_ids.push(r.u8("oinf direct_ref_layerID")?);
            }
            let mut dimension_identifiers = Vec::new();
            for j in 0..16u8 {
                if scalability_mask & (1 << j) != 0 {
                    dimension_identifiers.push((j, r.u8("oinf dimension_identifier")?));
                }
            }
            layers.push(LayerDependency {
                layer_id,
                direct_ref_layer_ids,
                dimension_identifiers,
            });
        }
        Ok(Self {
            scalability_mask,
            ptls,
            operating_points,
            layers,
            raw: b.to_vec(),
        })
    }

    /// Serialize to record bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut v = self.scalability_mask.to_be_bytes().to_vec();
        v.push(self.ptls.len().min(63) as u8);
        for p in self.ptls.iter().take(63) {
            v.push((p.profile_space << 6) | ((p.tier_flag as u8) << 5) | (p.profile_idc & 0x1f));
            v.extend_from_slice(&p.profile_compatibility_flags.to_be_bytes());
            v.extend_from_slice(&p.constraint_indicator_flags.to_be_bytes()[2..]);
            v.push(p.level_idc);
        }
        v.extend_from_slice(&(self.operating_points.len().min(65535) as u16).to_be_bytes());
        for op in self.operating_points.iter().take(65535) {
            v.extend_from_slice(&op.output_layer_set_idx.to_be_bytes());
            v.push(op.max_temporal_id);
            v.push(op.layers.len().min(255) as u8);
            for l in op.layers.iter().take(255) {
                v.push(l.ptl_idx);
                v.push(
                    ((l.layer_id & 0x3f) << 2)
                        | ((l.is_output_layer as u8) << 1)
                        | (l.is_alternate_output_layer as u8),
                );
            }
            v.extend_from_slice(&op.min_pic_size.0.to_be_bytes());
            v.extend_from_slice(&op.min_pic_size.1.to_be_bytes());
            v.extend_from_slice(&op.max_pic_size.0.to_be_bytes());
            v.extend_from_slice(&op.max_pic_size.1.to_be_bytes());
            v.push(
                ((op.max_chroma_format & 0x03) << 6)
                    | ((op.max_bit_depth_minus8 & 0x07) << 3)
                    | ((op.frame_rate.is_some() as u8) << 1)
                    | (op.bit_rate.is_some() as u8),
            );
            if let Some((avg, c)) = op.frame_rate {
                v.extend_from_slice(&avg.to_be_bytes());
                v.push(c & 0x03);
            }
            if let Some((max, avg)) = op.bit_rate {
                v.extend_from_slice(&max.to_be_bytes());
                v.extend_from_slice(&avg.to_be_bytes());
            }
        }
        v.push(self.layers.len().min(255) as u8);
        for l in self.layers.iter().take(255) {
            v.push(l.layer_id);
            v.push(l.direct_ref_layer_ids.len().min(255) as u8);
            v.extend_from_slice(&l.direct_ref_layer_ids[..l.direct_ref_layer_ids.len().min(255)]);
            for j in 0..16u8 {
                if self.scalability_mask & (1 << j) != 0 {
                    let d = l
                        .dimension_identifiers
                        .iter()
                        .find(|(k, _)| *k == j)
                        .map(|(_, d)| *d)
                        .unwrap_or(0);
                    v.push(d);
                }
            }
        }
        v
    }

    /// The operating point for an output layer set index.
    pub fn operating_point(&self, output_layer_set_idx: u16) -> Option<&OperatingPoint> {
        self.operating_points
            .iter()
            .find(|op| op.output_layer_set_idx == output_layer_set_idx)
    }

    /// The `nuh_layer_id`s output by an operating point.
    pub fn output_layers(&self, output_layer_set_idx: u16) -> Vec<u8> {
        self.operating_point(output_layer_set_idx)
            .map(|op| {
                op.layers
                    .iter()
                    .filter(|l| l.is_output_layer)
                    .map(|l| l.layer_id)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// `nuh_layer_id` of a two-byte HEVC NAL unit header (H.265 §7.3.1.2:
/// `forbidden_zero_bit(1) nal_unit_type(6) nuh_layer_id(6)
/// nuh_temporal_id_plus1(3)`).
pub fn nuh_layer_id(nal: &[u8]) -> Option<u8> {
    let h = nal.get(0..2)?;
    Some(((h[0] & 1) << 5) | (h[1] >> 3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lhvc_round_trips() {
        let cfg = LhevcConfig {
            configuration_version: 1,
            min_spatial_segmentation_idc: 5,
            parallelism_type: 2,
            num_temporal_layers: 1,
            temporal_id_nested: true,
            length_size: 4,
            arrays: vec![
                NalArray {
                    complete: true,
                    reserved_bit: false,
                    nal_unit_type: 32,
                    nal_units: vec![vec![0x40, 0x01, 0x0c]],
                },
                NalArray {
                    complete: true,
                    reserved_bit: false,
                    nal_unit_type: 33,
                    nal_units: vec![vec![0x42, 0x09, 0x01], vec![0x42, 0x01, 0x01]],
                },
            ],
            raw: Vec::new(),
        };
        let bytes = cfg.serialize();
        assert_eq!(bytes.len(), 6 + (3 + 2 + 3) + (3 + 2 + 3 + 2 + 3));
        assert_eq!(bytes[1] & 0xf0, 0xf0);
        let back = LhevcConfig::parse(&bytes).unwrap();
        assert_eq!(back.raw, bytes);
        assert_eq!(
            LhevcConfig {
                raw: Vec::new(),
                ..back
            },
            cfg
        );
        assert_eq!(cfg.nal_units().count(), 3);
        // Layer id 1 sits in the low bit of byte 0 and the top 5 of byte 1.
        assert_eq!(nuh_layer_id(&[0x42, 0x09]), Some(1));
        assert_eq!(nuh_layer_id(&[0x42, 0x01]), Some(0));
        assert_eq!(nuh_layer_id(&[0x43, 0x01]), Some(32));
        assert_eq!(nuh_layer_id(&[0x42]), None);
        assert!(LhevcConfig::parse(&[2, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn oinf_round_trips() {
        let o = OperatingPoints {
            scalability_mask: 0b100,
            ptls: vec![OperatingPointPtl {
                profile_space: 0,
                tier_flag: false,
                profile_idc: 1,
                profile_compatibility_flags: 0x6000_0000,
                constraint_indicator_flags: 0x9000_0000_0000,
                level_idc: 93,
            }],
            operating_points: vec![
                OperatingPoint {
                    output_layer_set_idx: 0,
                    max_temporal_id: 0,
                    layers: vec![OperatingPointLayer {
                        ptl_idx: 1,
                        layer_id: 0,
                        is_output_layer: true,
                        is_alternate_output_layer: false,
                    }],
                    min_pic_size: (64, 64),
                    max_pic_size: (64, 64),
                    max_chroma_format: 1,
                    max_bit_depth_minus8: 0,
                    frame_rate: None,
                    bit_rate: None,
                },
                OperatingPoint {
                    output_layer_set_idx: 1,
                    max_temporal_id: 0,
                    layers: vec![
                        OperatingPointLayer {
                            ptl_idx: 1,
                            layer_id: 0,
                            is_output_layer: false,
                            is_alternate_output_layer: false,
                        },
                        OperatingPointLayer {
                            ptl_idx: 1,
                            layer_id: 1,
                            is_output_layer: true,
                            is_alternate_output_layer: false,
                        },
                    ],
                    min_pic_size: (64, 64),
                    max_pic_size: (128, 128),
                    max_chroma_format: 1,
                    max_bit_depth_minus8: 2,
                    frame_rate: Some((30, 1)),
                    bit_rate: Some((1000, 500)),
                },
            ],
            layers: vec![
                LayerDependency {
                    layer_id: 0,
                    direct_ref_layer_ids: vec![],
                    dimension_identifiers: vec![(2, 0)],
                },
                LayerDependency {
                    layer_id: 1,
                    direct_ref_layer_ids: vec![0],
                    dimension_identifiers: vec![(2, 1)],
                },
            ],
            raw: Vec::new(),
        };
        let bytes = o.serialize();
        let back = OperatingPoints::parse(&bytes).unwrap();
        assert_eq!(back.raw, bytes);
        assert_eq!(
            OperatingPoints {
                raw: Vec::new(),
                ..back.clone()
            },
            o
        );
        assert_eq!(back.output_layers(1), vec![1]);
        assert_eq!(back.output_layers(0), vec![0]);
        assert!(back.operating_point(7).is_none());
        assert!(OperatingPoints::parse(&bytes[..10]).is_err());
    }
}
