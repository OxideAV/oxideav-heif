//! VVC image items → pixels through `oxideav-h266` (`registry`
//! feature).
//!
//! A `vvc1` item is one VVC access unit of length-prefixed NAL units
//! (HEIF L.2.2.2) whose parameter sets live in the `vvcC` property.
//! `oxideav-h266`'s registry factory is a parser-only placeholder in
//! its published line, so this module drives the crate's Annex B
//! stream decoder ([`oxideav_h266::stream::StreamDecoder`]) directly,
//! behind the framework [`Decoder`] trait: the `vvcC` record arrives
//! as `extradata`, every packet is one access unit, and the record's
//! NAL units are prepended to each ([`crate::vvcc::access_unit_annex_b`]).
//! An item's `tols` rides as the `tols` codec option (the L.2.2.1.2
//! OPI rule).
//!
//! The reverse direction — an Annex B access unit from the VVC encoder
//! into a `vvcC` record + `VVCItemData` — is [`vvc_item_from_annex_b`].

use oxideav_core::{CodecId, CodecParameters, Decoder, Error as CoreError, Frame, Packet, Result};

use crate::error::HeifError;
use crate::image::{Chroma, HeifFrame, HeifPixelFormat, Plane};
use crate::vvcc::{
    self, access_unit_annex_b, is_record_nal_type, is_vcl, nal_unit_type, SpsHead, VvcConfig,
    VvcNalArray, NAL_AUD, NAL_EOB, NAL_EOS, NAL_SPS, NAL_SUFFIX_SEI, NAL_VPS,
};

/// Codec id the VVC decoder is reached under.
pub const CODEC_ID: &str = "h266";

/// Build the VVC item decoder: `params.extradata` is the `vvcC`
/// record (the bytes after the FullBox header), `params.options`
/// may carry `tols`.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let cfg = if params.extradata.is_empty() {
        None
    } else {
        Some(
            VvcConfig::parse(&params.extradata)
                .map_err(|e| CoreError::invalid(format!("h266: vvcC extradata: {e}")))?,
        )
    };
    let tols = params
        .options
        .get("tols")
        .and_then(|v| v.parse::<u16>().ok());
    Ok(Box::new(VvcItemDecoder {
        codec_id: CodecId::new(CODEC_ID),
        cfg,
        tols,
        inner: oxideav_h266::stream::StreamDecoder::new(),
        out: std::collections::VecDeque::new(),
        flushed: false,
    }))
}

/// Framework decoder over `oxideav-h266`'s Annex B stream decoder.
pub struct VvcItemDecoder {
    codec_id: CodecId,
    cfg: Option<VvcConfig>,
    tols: Option<u16>,
    inner: oxideav_h266::stream::StreamDecoder,
    out: std::collections::VecDeque<oxideav_core::VideoFrame>,
    flushed: bool,
}

impl Decoder for VvcItemDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let data = &packet.data;
        if data.is_empty() {
            return Err(CoreError::invalid("h266: empty packet"));
        }
        let annex_b_in = data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]);
        let bs = match (&self.cfg, annex_b_in) {
            (Some(cfg), false) => access_unit_annex_b(cfg, data, self.tols)
                .map_err(|e| CoreError::invalid(format!("h266: {e}")))?,
            (Some(cfg), true) => {
                let mut v = Vec::with_capacity(data.len() + 128);
                for n in cfg.nal_units() {
                    v.extend_from_slice(&[0, 0, 0, 1]);
                    v.extend_from_slice(n);
                }
                v.extend_from_slice(data);
                v
            }
            (None, true) => data.clone(),
            (None, false) => {
                return Err(CoreError::invalid(
                    "h266: length-prefixed packet without a vvcC extradata",
                ))
            }
        };
        let mut pics = Vec::new();
        self.inner
            .decode_annex_b(&bs, &mut |p| pics.push(p))
            .map_err(|e| CoreError::invalid(format!("h266: {e}")))?;
        for p in pics {
            if !p.output_flag {
                continue;
            }
            let frame = frame_of(&p).map_err(|e| CoreError::invalid(format!("h266: {e}")))?;
            let (mut vf, _) = frame
                .to_core()
                .map_err(|e| CoreError::invalid(format!("h266: {e}")))?;
            vf.pts = packet.pts;
            self.out.push_back(vf);
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.out.pop_front() {
            Some(v) => Ok(Frame::Video(v)),
            None if self.flushed => Err(CoreError::Eof),
            None => Err(CoreError::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.flushed = true;
        Ok(())
    }
}

/// The conformance-cropped picture of a decoded VVC picture as a
/// tightly packed [`HeifFrame`] (4:2:0 or monochrome at the SPS bit
/// depth; 8-bit samples narrowed to one byte).
fn frame_of(p: &oxideav_h266::stream::DecodedPicture) -> crate::error::Result<HeifFrame> {
    let chroma = match p.chroma_format_idc {
        0 => Chroma::Mono,
        1 => Chroma::Yuv420,
        2 => Chroma::Yuv422,
        3 => Chroma::Yuv444,
        other => return Err(HeifError::invalid(format!("VVC chroma_format_idc {other}"))),
    };
    let depth = u8::try_from(p.bit_depth)
        .ok()
        .filter(|d| (8..=16).contains(d))
        .ok_or_else(|| HeifError::invalid(format!("VVC bit depth {}", p.bit_depth)))?;
    let fmt = HeifPixelFormat::new(chroma, depth, false)?;
    let (left, top, w, h) = p.crop;
    if w == 0 || h == 0 || left + w > p.frame.luma.width || top + h > p.frame.luma.height {
        return Err(HeifError::invalid(format!(
            "VVC conformance window {left},{top} {w}x{h} outside the {}x{} picture",
            p.frame.luma.width, p.frame.luma.height
        )));
    }
    let (w32, h32) = (w as u32, h as u32);
    let bps = fmt.bytes_per_sample();
    let mut planes = Vec::with_capacity(fmt.plane_count());
    let srcs = [&p.frame.luma, &p.frame.cb, &p.frame.cr];
    for (i, src) in srcs.iter().enumerate().take(fmt.plane_count()) {
        let (pw, ph) = fmt.plane_dims(i, w32, h32);
        let (sx, sy) = if i == 0 {
            (left, top)
        } else {
            let (shx, shy) = chroma.shift();
            (left >> shx, top >> shy)
        };
        if sx + pw as usize > src.width || sy + ph as usize > src.height {
            return Err(HeifError::invalid(format!(
                "VVC plane {i} is {}x{}, window needs {}x{}",
                src.width,
                src.height,
                sx + pw as usize,
                sy + ph as usize
            )));
        }
        let row_bytes = pw as usize * bps;
        let mut data = Vec::with_capacity(row_bytes * ph as usize);
        for y in 0..ph as usize {
            let row =
                &src.samples[(sy + y) * src.stride + sx..(sy + y) * src.stride + sx + pw as usize];
            if bps == 1 {
                data.extend(row.iter().map(|v| *v as u8));
            } else {
                for v in row {
                    data.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
        planes.push(Plane {
            stride: row_bytes,
            data,
        });
    }
    let f = HeifFrame {
        width: w32,
        height: h32,
        format: fmt,
        planes,
    };
    f.validate()?;
    Ok(f)
}

/// Build a `vvcC` record + `VVCItemData` from an Annex B access unit
/// (what the VVC encoder emits): DCI / OPI / VPS / SPS / PPS / prefix
/// APS / prefix SEI NAL units go to the record (one complete array per
/// type, in the recommended order), AUD / EOS / EOB are dropped (an
/// image item needs none), everything else — the picture header, the
/// slices, suffix NAL units — is re-framed with 4-byte length prefixes
/// (HEIF L.2.2.2). The record head comes from the SPS (chroma format,
/// bit depth, maximum picture size) and its `native_ptl` from the SPS
/// `profile_tier_level()` or, when the SPS carries none, the VPS's
/// first one (`ols_idx` 0, one sublayer). Returns the record, the item
/// data, the cropped picture size and the sample layout.
pub fn vvc_item_from_annex_b(
    annex_b: &[u8],
) -> crate::error::Result<(VvcConfig, Vec<u8>, u32, u32, HeifPixelFormat)> {
    let nals = crate::hvcc::split_annex_b(annex_b);
    let mut arrays: Vec<VvcNalArray> = Vec::new();
    let mut item: Vec<&[u8]> = Vec::new();
    let mut sps_nal: Option<&[u8]> = None;
    let mut vps_nal: Option<&[u8]> = None;
    let mut has_vcl = false;
    for n in &nals {
        let Some(t) = nal_unit_type(n) else {
            return Err(HeifError::invalid("VVC NAL unit shorter than its header"));
        };
        if is_record_nal_type(t) {
            if t == NAL_SPS && sps_nal.is_none() {
                sps_nal = Some(n);
            }
            if t == NAL_VPS && vps_nal.is_none() {
                vps_nal = Some(n);
            }
            match arrays.iter_mut().find(|a| a.nal_unit_type == t) {
                Some(a) => a.nal_units.push(n.to_vec()),
                None => arrays.push(VvcNalArray {
                    complete: true,
                    nal_unit_type: t,
                    nal_units: vec![n.to_vec()],
                }),
            }
        } else if matches!(t, NAL_AUD | NAL_EOS | NAL_EOB) {
            // Not carried in image items.
        } else {
            has_vcl |= is_vcl(t);
            let _ = NAL_SUFFIX_SEI; // suffix SEI / APS stay in the item data
            item.push(n);
        }
    }
    let sps_nal = sps_nal.ok_or_else(|| HeifError::invalid("VVC access unit without an SPS"))?;
    if !has_vcl {
        return Err(HeifError::invalid("VVC access unit without VCL NAL units"));
    }
    // Recommended array order: DCI, OPI, VPS, SPS, PPS, prefix APS, prefix SEI.
    arrays.sort_by_key(|a| a.nal_unit_type);
    let head = SpsHead::parse_nal(sps_nal)?;
    let ptl = match head.ptl.clone() {
        Some(p) => p,
        None => {
            let vps = vps_nal.ok_or_else(|| {
                HeifError::invalid("VVC SPS carries no profile_tier_level and there is no VPS")
            })?;
            vvcc::vps_first_ptl(vps)?
        }
    };
    let (width, height) = head.cropped_size();
    let chroma = Chroma::from_idc(head.chroma_format_idc)
        .ok_or_else(|| HeifError::invalid("VVC sps_chroma_format_idc"))?;
    let layout = HeifPixelFormat::new(chroma, head.bit_depth, false)?;
    let mut cfg = VvcConfig {
        flags: 0,
        length_size: 4,
        ptl_present_flag: true,
        ols_idx: 0,
        num_sublayers: 1,
        constant_frame_rate: 0,
        chroma_format_idc: head.chroma_format_idc,
        bit_depth_minus8: head.bit_depth - 8,
        native_ptl: Some(ptl),
        max_picture_width: head.pic_width.min(u16::MAX as u32) as u16,
        max_picture_height: head.pic_height.min(u16::MAX as u32) as u16,
        avg_frame_rate: 0,
        arrays,
        raw: Vec::new(),
    };
    cfg.raw = cfg.serialize();
    let data = crate::hvcc::join_length_prefixed(&item, 4)?;
    Ok((cfg, data, width, height, layout))
}
