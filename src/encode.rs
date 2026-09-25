//! Still-image encoding through the oxideav encoders (`registry`
//! feature): pixels → HEVC / AV1 items → a MIAF-conformant file via
//! [`HeifWriter`], plus the framework [`Encoder`] (`"heif"`: one frame
//! in, one complete `.heic` file per packet out).
//!
//! HEVC items come from `oxideav-h265`'s registry encoder: it emits
//! Annex B access units with the parameter sets in band; this module
//! splits the NAL units, lifts VPS / SPS / PPS into the `hvcC` record
//! (profile / tier / level from the SPS profile-tier-level, chroma and
//! bit depth from the SPS) and re-frames the VCL NAL units with 4-byte
//! length prefixes (HEIF Annex B.2.2). AV1 items come from
//! `oxideav-av1`'s key-frame encoder (an IVF buffer): the temporal unit
//! is extracted and the `av1C` record built from the Sequence Header
//! OBU.

use oxideav_core::{
    CodecId, CodecOptions, CodecParameters, Encoder, Error as CoreError, Frame, Packet,
    Result as CoreResult, TimeBase,
};

use crate::av1c::Av1Config;
use crate::compose::crop;
use crate::derived::GridDescriptor;
use crate::error::{HeifError, Result};
use crate::hvcc::{join_length_prefixed, HevcConfig, NalArray, NAL_PPS, NAL_SPS, NAL_VPS};
use crate::image::{Chroma, HeifFrame, HeifPixelFormat};
use crate::meta::{ITEM_TYPE_AV01, ITEM_TYPE_HVC1};
use crate::props::{Clap, Colr, CropRect, Ispe, Pixi, Property};
use crate::writer::HeifWriter;

/// Which codec produces the coded items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StillCodec {
    /// HEVC (`hvc1` items, `heic` brand).
    Hevc,
    /// AV1 (`av01` items, `avif` brand); lossless or quality-dialled
    /// reduced-header stills at every (depth, chroma) pairing.
    Av1,
}

/// Encoding options.
#[derive(Clone, Debug)]
pub struct EncodeOptions {
    /// Codec.
    pub codec: StillCodec,
    /// HEVC: `"pcm"` (lossless), `"intra"` (CABAC intra at `qp`).
    pub hevc_mode: String,
    /// HEVC intra QP (0..=51).
    pub qp: u8,
    /// Split the picture into a grid of `tile × tile` tiles (`grid`
    /// derived item) when set; tiles are at least 64 pixels (MIAF).
    pub grid_tile: Option<u32>,
    /// Add a thumbnail whose largest dimension is this many pixels.
    pub thumbnail_max_dim: Option<u32>,
    /// Colour information written as `colr`.
    pub colr: Colr,
    /// ICC profile written as a second `colr` (`prof`).
    pub icc_profile: Option<Vec<u8>>,
    /// Exif payload (from the TIFF header) to attach.
    pub exif: Option<Vec<u8>>,
    /// XMP packet to attach.
    pub xmp: Option<String>,
    /// Transformative properties applied to the primary (in order),
    /// written as an `iden` item over the coded image.
    pub transforms: Vec<Property>,
    /// AV1: quality dial 0..=100 (100 = lossless) for lossy items;
    /// `None` codes lossless (the pre-0.0.3 behaviour).
    pub av1_quality: Option<u8>,
    /// AV1 search effort: `"fast"` (default), `"balanced"`, `"thorough"`.
    pub av1_speed: String,
    /// HEVC intra mode-decision effort (`rd` 0..=2; the encoder's
    /// still default when `None`).
    pub hevc_rd: Option<u32>,
    /// HEVC tile layout (`"CxR"`, e.g. `"4x4"`) for parallel coding.
    pub hevc_tiles: Option<String>,
    /// ISO 21496-1 gain map to carry as a `tmap` derived item over
    /// the primary (HEIF Amd 1 §6.6.2.4).
    pub gain_map: Option<GainMapSpec>,
}

/// A gain map to write next to the base image (see
/// [`EncodeOptions::gain_map`]): the map picture, its ISO 21496-1
/// metadata and the alternate (fully applied) rendition's colour
/// information. The map is coded with the same codec as the base, as
/// a hidden item with an `nclx` `colr` of `colour_primaries =
/// transfer_characteristics = 2` (the clause's rule) carrying
/// `gain_map_matrix` / `gain_map_full_range`; monochrome maps stay
/// monochrome (AV1) or ride the luma of a 4:2:0 picture (HEVC).
#[derive(Clone, Debug)]
pub struct GainMapSpec {
    /// The gain-map picture (monochrome or colour; any size — 21496-1
    /// §6.2.2 resamples it to the base at application).
    pub frame: HeifFrame,
    /// The C.2 metadata (`GainMapMetadata`).
    pub metadata: crate::gainmap::GainMapMetadata,
    /// The alternate image's colour information, written as the
    /// `tmap` item's `colr` (21496-1 §5.3.2).
    pub alternate_colr: Colr,
    /// `matrix_coefficients` of the stored gain map's YCbCr coding
    /// (colour maps; ignored for monochrome).
    pub gain_map_matrix: u16,
    /// `full_range_flag` of the stored gain map.
    pub gain_map_full_range: bool,
    /// `clli` hint for the alternate rendition, written on the `tmap`
    /// item ("should", §6.6.2.4.1).
    pub alternate_clli: Option<crate::props::Clli>,
    /// The `pixi` hint on the `tmap` item: "the approximate amount of
    /// colour resolution available after fully applying the gain map"
    /// (§6.6.2.4.1); also the depth this crate reconstructs the applied
    /// rendition at. 10–12 suits a PQ / HLG alternate over an 8-bit base.
    pub alternate_bit_depth: u8,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            codec: StillCodec::Hevc,
            hevc_mode: "intra".into(),
            qp: 26,
            grid_tile: None,
            thumbnail_max_dim: None,
            colr: Colr::MIAF_DEFAULT,
            icc_profile: None,
            exif: None,
            xmp: None,
            transforms: Vec::new(),
            av1_quality: None,
            av1_speed: "fast".into(),
            hevc_rd: None,
            hevc_tiles: None,
            gain_map: None,
        }
    }
}

#[doc(hidden)]
/// A coded picture ready to become an item.
#[derive(Clone, Debug)]
pub struct CodedPicture {
    /// Item type.
    pub item_type: [u8; 4],
    /// Item payload (length-prefixed NAL units / AV1 temporal unit).
    pub data: Vec<u8>,
    /// Decoder configuration property.
    pub config: Property,
    /// Coded picture size (before any `clap`).
    pub coded_width: u32,
    /// Coded picture size.
    pub coded_height: u32,
    /// Sample layout of the coded picture.
    pub layout: HeifPixelFormat,
}

/// Align a dimension up to `n`.
fn align_up(v: u32, n: u32) -> u32 {
    v.div_ceil(n) * n
}

#[doc(hidden)]
/// Pad a frame to `w × h` by edge replication (encoders need aligned
/// sizes; the visible extent is restored with `clap`).
pub fn pad_frame(f: &HeifFrame, w: u32, h: u32) -> Result<HeifFrame> {
    if (w, h) == (f.width, f.height) {
        return Ok(f.tight());
    }
    let mut out = HeifFrame::zeroed(w, h, f.format)?;
    for p in 0..f.format.plane_count() {
        let (sw, sh) = f.plane_dims(p);
        let (dw, dh) = out.plane_dims(p);
        for y in 0..dh {
            let sy = y.min(sh - 1);
            for x in 0..dw {
                let sx = x.min(sw - 1);
                out.set_sample(p, x, y, f.sample(p, sx, sy));
            }
        }
    }
    Ok(out)
}

#[doc(hidden)]
/// Convert a frame to 8-bit 4:2:0 (the layout the HEVC / AV1 encoders
/// accept): monochrome gains neutral chroma, 4:2:2 / 4:4:4 chroma is
/// box-averaged, depths above 8 are rounded down.
pub fn to_yuv420_8(f: &HeifFrame) -> Result<HeifFrame> {
    let fmt = HeifPixelFormat::new(Chroma::Yuv420, 8, false)?;
    let shift = f.format.bit_depth - 8;
    let round = |v: u32| -> u16 {
        if shift == 0 {
            v as u16
        } else {
            ((v + (1 << (shift - 1))) >> shift).min(255) as u16
        }
    };
    let mut out = HeifFrame::filled(f.width, f.height, fmt, 128)?;
    for y in 0..f.height {
        for x in 0..f.width {
            out.set_sample(0, x, y, round(f.sample(0, x, y) as u32));
        }
    }
    if f.format.chroma != Chroma::Mono {
        let (sx, sy) = f.format.chroma.shift();
        let (cw, ch) = out.plane_dims(1);
        for p in 1..3 {
            for cy in 0..ch {
                for cx in 0..cw {
                    // Average the source chroma samples covering this
                    // 4:2:0 position.
                    let x0 = (cx * 2) >> sx;
                    let y0 = (cy * 2) >> sy;
                    let x1 = ((cx * 2 + 1) >> sx).min(f.plane_dims(p).0 - 1);
                    let y1 = ((cy * 2 + 1) >> sy).min(f.plane_dims(p).1 - 1);
                    let mut sum = 0u32;
                    let mut n = 0u32;
                    for yy in y0..=y1 {
                        for xx in x0..=x1 {
                            sum += f.sample(p, xx, yy) as u32;
                            n += 1;
                        }
                    }
                    out.set_sample(p, cx, cy, round((sum + n / 2) / n));
                }
            }
        }
    }
    Ok(out)
}

/// The two-byte NAL unit header for a parsed header.
fn nal_header_bytes(h: &oxideav_h265::NalHeader) -> [u8; 2] {
    [
        (h.nal_unit_type << 1) | (h.nuh_layer_id >> 5),
        ((h.nuh_layer_id & 0x1f) << 3) | ((h.temporal_id + 1) & 0x07),
    ]
}

/// Build an `avcC` record + `AVCItemData` (HEIF E.2.2: length-prefixed
/// NAL units of exactly one access unit) from an Annex B access unit;
/// returns `(record, item data, width, height, layout)` with the size
/// from the SPS (cropped, H.264 §7.4.2.1.1).
pub fn avc_item_from_annex_b(
    annex_b: &[u8],
) -> Result<(crate::avcc::AvcConfig, Vec<u8>, u32, u32, HeifPixelFormat)> {
    use oxideav_h264::nal::{parse_nal_unit, AnnexBSplitter};
    let mut sps_units: Vec<Vec<u8>> = Vec::new();
    let mut pps_units: Vec<Vec<u8>> = Vec::new();
    let mut vcl: Vec<&[u8]> = Vec::new();
    let mut sps: Option<oxideav_h264::sps::Sps> = None;
    for nal in AnnexBSplitter::new(annex_b) {
        let Some(&h) = nal.first() else { continue };
        match h & 0x1f {
            7 => {
                if sps.is_none() {
                    let parsed = parse_nal_unit(nal)
                        .map_err(|e| HeifError::invalid(format!("AVC NAL parse: {e}")))?;
                    sps = Some(
                        oxideav_h264::sps::Sps::parse(&parsed.rbsp)
                            .map_err(|e| HeifError::invalid(format!("AVC SPS parse: {e}")))?,
                    );
                }
                sps_units.push(nal.to_vec());
            }
            8 => pps_units.push(nal.to_vec()),
            1..=5 => vcl.push(nal),
            _ => {} // SEI / AUD / filler ride out.
        }
    }
    let sps = sps.ok_or_else(|| HeifError::invalid("AVC access unit without an SPS"))?;
    if vcl.is_empty() {
        return Err(HeifError::invalid("AVC access unit without VCL NAL units"));
    }
    // Cropped picture size (§7.4.2.1.1): CropUnitX / CropUnitY from the
    // chroma format (1 for 4:0:0 and separate planes), the map-unit
    // height doubled for field coding.
    let chroma_idc = sps.chroma_format_idc as u8;
    let chroma_array_type = if sps.separate_colour_plane_flag {
        0
    } else {
        chroma_idc
    };
    let (sub_w, sub_h) = match chroma_array_type {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let frame_mbs_only = sps.frame_mbs_only_flag as u32;
    let crop_unit_x = if chroma_array_type == 0 { 1 } else { sub_w };
    let crop_unit_y = if chroma_array_type == 0 { 1 } else { sub_h } * (2 - frame_mbs_only);
    let coded_w = (sps.pic_width_in_mbs_minus1 + 1) * 16;
    let coded_h: u32 = (2 - frame_mbs_only) * (sps.pic_height_in_map_units_minus1 + 1) * 16;
    let (w, h) = match &sps.frame_cropping {
        Some(c) => (
            coded_w
                .checked_sub(crop_unit_x * (c.left + c.right))
                .ok_or_else(|| HeifError::invalid("AVC SPS crop wider than the picture"))?,
            coded_h
                .checked_sub(crop_unit_y * (c.top + c.bottom))
                .ok_or_else(|| HeifError::invalid("AVC SPS crop taller than the picture"))?,
        ),
        None => (coded_w, coded_h),
    };
    let chroma = Chroma::from_idc(chroma_idc)
        .ok_or_else(|| HeifError::invalid("AVC SPS chroma_format_idc"))?;
    let layout = HeifPixelFormat::new(chroma, 8 + sps.bit_depth_luma_minus8 as u8, false)?;
    let first_sps = &sps_units[0];
    let trailer = crate::avcc::AvcConfig::has_extension_trailer(sps.profile_idc);
    let mut cfg = crate::avcc::AvcConfig {
        configuration_version: 1,
        profile_idc: sps.profile_idc,
        profile_compatibility: first_sps.get(2).copied().unwrap_or(0),
        level_idc: sps.level_idc,
        length_size: 4,
        sps: sps_units,
        pps: pps_units,
        chroma_format: trailer.then_some(chroma_idc),
        bit_depth_luma_minus8: trailer.then_some(sps.bit_depth_luma_minus8 as u8),
        bit_depth_chroma_minus8: trailer.then_some(sps.bit_depth_chroma_minus8 as u8),
        sps_ext: Vec::new(),
        raw: Vec::new(),
    };
    cfg.raw = cfg.serialize();
    let data = join_length_prefixed(&vcl, 4)?;
    Ok((cfg, data, w, h, layout))
}

/// Build an `hvcC` record + `HEVCItemData` from an Annex B access unit.
pub fn hevc_item_from_annex_b(
    annex_b: &[u8],
) -> Result<(HevcConfig, Vec<u8>, u32, u32, HeifPixelFormat)> {
    let nals = oxideav_h265::collect_nal_units(annex_b)
        .map_err(|e| HeifError::invalid(format!("HEVC Annex B split: {e}")))?;
    let mut arrays: Vec<NalArray> = Vec::new();
    let mut vcl: Vec<Vec<u8>> = Vec::new();
    let mut sps: Option<oxideav_h265::SeqParameterSet> = None;
    for n in &nals {
        let mut coded = nal_header_bytes(&n.header).to_vec();
        coded.extend_from_slice(&n.escaped);
        let t = n.header.nal_unit_type;
        match t {
            NAL_VPS | NAL_SPS | NAL_PPS => {
                if t == NAL_SPS && sps.is_none() {
                    sps = Some(
                        oxideav_h265::SeqParameterSet::parse(&n.rbsp)
                            .map_err(|e| HeifError::invalid(format!("SPS parse: {e}")))?,
                    );
                }
                match arrays.iter_mut().find(|a| a.nal_unit_type == t) {
                    Some(a) => a.nal_units.push(coded),
                    None => arrays.push(NalArray {
                        complete: true,
                        reserved_bit: false,
                        nal_unit_type: t,
                        nal_units: vec![coded],
                    }),
                }
            }
            _ if t < 32 => vcl.push(coded),
            _ => {} // SEI / AUD / EOS ride out; MIAF items need none.
        }
    }
    let sps = sps.ok_or_else(|| HeifError::invalid("HEVC access unit without an SPS"))?;
    if vcl.is_empty() {
        return Err(HeifError::invalid("HEVC access unit without VCL NAL units"));
    }
    arrays.sort_by_key(|a| a.nal_unit_type);
    let ptl = &sps.ptl;
    let mut cfg = HevcConfig {
        configuration_version: 1,
        general_profile_space: ptl.general_profile_space,
        general_tier_flag: ptl.general_tier_flag,
        general_profile_idc: ptl.general_profile_idc,
        general_profile_compatibility_flags: profile_compat_flags(ptl.general_profile_idc),
        general_constraint_indicator_flags: 0,
        general_level_idc: ptl.general_level_idc,
        min_spatial_segmentation_idc: 0,
        parallelism_type: 0,
        chroma_format_idc: sps.chroma_format_idc,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        avg_frame_rate: 0,
        constant_frame_rate: 0,
        num_temporal_layers: 1,
        temporal_id_nested: true,
        length_size: 4,
        arrays,
        raw: Vec::new(),
    };
    cfg.raw = cfg.serialize();
    let refs: Vec<&[u8]> = vcl.iter().map(Vec::as_slice).collect();
    let data = join_length_prefixed(&refs, 4)?;
    let cw = &sps.conformance_window;
    let (sub_w, sub_h) = match sps.chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let width = sps.pic_width_in_luma_samples - sub_w * (cw.left_offset + cw.right_offset);
    let height = sps.pic_height_in_luma_samples - sub_h * (cw.top_offset + cw.bottom_offset);
    let layout = HeifPixelFormat::new(
        Chroma::from_idc(sps.chroma_format_idc).unwrap_or(Chroma::Yuv420),
        sps.bit_depth_luma_minus8 + 8,
        false,
    )?;
    Ok((cfg, data, width, height, layout))
}

/// `general_profile_compatibility_flags` with the bit of `profile_idc`
/// set (bit 31 ↔ profile 0), plus the Main flag for Main Still Picture
/// and Main 10 (H.265 A.3.2 / A.3.3 compatibility rules).
fn profile_compat_flags(profile_idc: u8) -> u32 {
    let mut f = 0u32;
    for p in [profile_idc] {
        if p < 32 {
            f |= 1u32 << (31 - p);
        }
    }
    if profile_idc == 3 || profile_idc == 2 {
        f |= 1u32 << 30; // Main-compatible
    }
    if profile_idc == 3 {
        f |= 1u32 << 29; // Main 10-compatible
    }
    f
}

#[doc(hidden)]
/// Encode one 8-bit 4:2:0 picture as an HEVC item. `w`/`h` must be
/// multiples of 16 (the encoder's constraint); use [`pad_frame`].
pub fn encode_hevc_picture(frame: &HeifFrame, mode: &str, qp: u8) -> Result<CodedPicture> {
    encode_hevc_picture_with(frame, mode, qp, &[])
}

/// The HEVC encoder options that carry the item's colour information
/// into the bitstream VUI (§E.2.1 `video_signal_type`: range + H.273
/// colour description — the field an OS image reader takes the sample
/// range from) and mark the access unit as a Main Still Picture.
fn hevc_signal_options(colr: &Colr, opts: &EncodeOptions) -> Vec<(String, String)> {
    let mut v = vec![("still".to_string(), "1".to_string())];
    match colr {
        Colr::Nclx {
            primaries,
            transfer,
            matrix,
            full_range,
        } => {
            v.push((
                "range".into(),
                if *full_range { "full" } else { "limited" }.into(),
            ));
            v.push(("colorprim".into(), primaries.to_string()));
            v.push(("transfer".into(), transfer.to_string()));
            v.push(("matrix".into(), matrix.to_string()));
        }
        // ICC / other: full range (the MIAF default), no description.
        _ => v.push(("range".into(), "full".into())),
    }
    if let Some(rd) = opts.hevc_rd {
        v.push(("rd".into(), rd.min(2).to_string()));
    }
    if let Some(t) = &opts.hevc_tiles {
        v.push(("tiles".into(), t.clone()));
    }
    v
}

#[doc(hidden)]
/// [`encode_hevc_picture`] with extra codec options (`ctb`, `vpsid`,
/// …) passed through to the HEVC encoder.
pub fn encode_hevc_picture_with(
    frame: &HeifFrame,
    mode: &str,
    qp: u8,
    extra: &[(&str, &str)],
) -> Result<CodedPicture> {
    if frame.format.chroma != Chroma::Yuv420 || frame.format.bit_depth != 8 {
        return Err(HeifError::unsupported(
            "HEVC item encoding takes 8-bit 4:2:0 input (see to_yuv420_8)",
        ));
    }
    let mut params = CodecParameters::video(CodecId::new(crate::decode::CODEC_ID_HEVC));
    params.width = Some(frame.width);
    params.height = Some(frame.height);
    params.pixel_format = Some(oxideav_core::PixelFormat::Yuv420P);
    let mut options = CodecOptions::new()
        .set("mode", mode)
        .set("qp", qp.to_string());
    for (k, v) in extra {
        options = options.set(*k, *v);
    }
    params.options = options;
    let mut enc = oxideav_h265::make_encoder(&params)
        .map_err(|e| HeifError::unsupported(format!("HEVC encoder: {e}")))?;
    let (mut vf, _) = frame.to_core()?;
    vf.pts = Some(0);
    enc.send_frame(&Frame::Video(vf))
        .map_err(|e| HeifError::invalid(format!("HEVC encoder rejected the frame: {e}")))?;
    enc.flush()
        .map_err(|e| HeifError::invalid(format!("HEVC encoder flush: {e}")))?;
    let mut annex_b = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => annex_b.extend_from_slice(&p.data),
            Err(CoreError::NeedMore) | Err(CoreError::Eof) => break,
            Err(e) => return Err(HeifError::invalid(format!("HEVC encoder: {e}"))),
        }
    }
    let (cfg, data, w, h, layout) = hevc_item_from_annex_b(&annex_b)?;
    Ok(CodedPicture {
        item_type: ITEM_TYPE_HVC1,
        data,
        config: Property::HvcC(cfg),
        coded_width: w,
        coded_height: h,
        layout,
    })
}

#[doc(hidden)]
/// Encode one picture as an AV1 still item (`reduced_still_picture_header`)
/// at its own (depth, chroma) pairing — 8 / 10 / 12-bit, 4:0:0 /
/// 4:2:0 / 4:2:2 / 4:4:4 — lossless when `opts.av1_quality` is `None`
/// or 100, else at that quality. Dimensions must be multiples of 8.
pub fn encode_av1_picture(frame: &HeifFrame, opts: &EncodeOptions) -> Result<CodedPicture> {
    use oxideav_av1::encoder::{
        encode_still_yuv, ChromaFormat, StillOptions, StillSpeed, YuvFrame,
    };
    if !matches!(frame.format.bit_depth, 8 | 10 | 12) {
        return Err(HeifError::unsupported(format!(
            "AV1 items code 8 / 10 / 12-bit pictures, not {}-bit",
            frame.format.bit_depth
        )));
    }
    let t = frame.without_alpha().tight();
    let format = match t.format.chroma {
        Chroma::Mono => ChromaFormat::Monochrome,
        Chroma::Yuv420 => ChromaFormat::Yuv420,
        Chroma::Yuv422 => ChromaFormat::Yuv422,
        Chroma::Yuv444 => ChromaFormat::Yuv444,
    };
    let plane16 = |p: usize| -> Vec<u16> {
        let (w, h) = t.plane_dims(p);
        let mut v = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                v.push(t.sample(p, x, y));
            }
        }
        v
    };
    let (u, v) = if t.format.chroma == Chroma::Mono {
        (Vec::new(), Vec::new())
    } else {
        (plane16(1), plane16(2))
    };
    let input = YuvFrame {
        width: t.width,
        height: t.height,
        bit_depth: t.format.bit_depth,
        format,
        y: plane16(0),
        u,
        v,
    };
    let mut so = match opts.av1_quality {
        None | Some(100..) => StillOptions::from_quality(100),
        Some(q) => StillOptions::from_quality(q),
    };
    so.speed = match opts.av1_speed.as_str() {
        "balanced" => StillSpeed::Balanced,
        "thorough" => StillSpeed::Thorough,
        _ => StillSpeed::Fast,
    };
    so.reduced_header = true;
    match &opts.colr {
        Colr::Nclx {
            primaries,
            transfer,
            matrix,
            full_range,
        } => {
            so.full_range = *full_range;
            so.color_description = Some((*primaries as u8, *transfer as u8, *matrix as u8));
        }
        _ => {
            so.full_range = true;
            so.color_description = None;
        }
    }
    let still = encode_still_yuv(&input, &so)
        .map_err(|e| HeifError::unsupported(format!("AV1 still encoder: {e:?}")))?;
    let raw = still.codec_config.to_bytes();
    let cfg = Av1Config::parse(&raw)?;
    let layout = crate::decode::av1_layout(&cfg)?;
    Ok(CodedPicture {
        item_type: ITEM_TYPE_AV01,
        data: still.temporal_unit_bytes,
        config: Property::Av1C(cfg),
        coded_width: t.width,
        coded_height: t.height,
        layout,
    })
}

fn encode_picture(frame: &HeifFrame, opts: &EncodeOptions) -> Result<CodedPicture> {
    encode_picture_with(frame, opts, &[])
}

/// Codec options that give the alpha auxiliary's HEVC parameter sets
/// their own ids (VPS / SPS / PPS 1): Apple ImageIO refuses a file
/// whose two items carry byte-identical parameter sets (verified by
/// cross-muxing), and distinct ids make them differ on every mode.
const ALPHA_HEVC_OPTIONS: &[(&str, &str)] = &[("vpsid", "1"), ("spsid", "1"), ("ppsid", "1")];

fn encode_picture_with(
    frame: &HeifFrame,
    opts: &EncodeOptions,
    extra: &[(&str, &str)],
) -> Result<CodedPicture> {
    encode_picture_for(frame, opts, &opts.colr, extra)
}

/// [`encode_picture_with`] for an item whose colour information is
/// `colr` (the alpha auxiliary signals its own range).
fn encode_picture_for(
    frame: &HeifFrame,
    opts: &EncodeOptions,
    colr: &Colr,
    extra: &[(&str, &str)],
) -> Result<CodedPicture> {
    match opts.codec {
        StillCodec::Hevc => {
            let signal = hevc_signal_options(colr, opts);
            let mut all: Vec<(&str, &str)> = signal
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            all.extend_from_slice(extra);
            encode_hevc_picture_with(frame, &opts.hevc_mode, opts.qp, &all)
        }
        StillCodec::Av1 => encode_av1_picture(frame, opts),
    }
}

fn alignment(opts: &EncodeOptions) -> u32 {
    match opts.codec {
        StillCodec::Hevc => 16,
        StillCodec::Av1 => 8,
    }
}

/// The transformative chain of `opts` as essential property entries,
/// attached to every displayed item (master, alpha auxiliary,
/// thumbnails) so third-party readers that only honour properties on
/// the item they decode still render the intended orientation.
fn transform_props(opts: &EncodeOptions) -> Vec<(Property, bool)> {
    opts.transforms.iter().map(|t| (t.clone(), true)).collect()
}

/// The `nclx` to write next to `colr` when an ICC profile is present:
/// HEIF §6.5.5 requires `colour_primaries = transfer_characteristics = 2`
/// for an nclx paired with an ICC (only the matrix / range stay
/// meaningful, since the ICC governs colour); the caller's `nclx` is
/// used unchanged when there is no ICC.
fn nclx_for_icc(colr: &Colr, icc_present: bool) -> Colr {
    match (colr, icc_present) {
        (
            Colr::Nclx {
                matrix, full_range, ..
            },
            true,
        ) => Colr::Nclx {
            primaries: 2,
            transfer: 2,
            matrix: *matrix,
            full_range: *full_range,
        },
        _ => colr.clone(),
    }
}

/// Standard descriptive properties of a coded item.
fn coded_props(pic: &CodedPicture, colr: &Colr, icc: Option<&[u8]>) -> Vec<(Property, bool)> {
    let mut v = vec![
        (pic.config.clone(), true),
        (
            Property::Ispe(Ispe {
                width: pic.coded_width,
                height: pic.coded_height,
            }),
            false,
        ),
        (
            Property::Pixi(Pixi {
                bits_per_channel: vec![pic.layout.bit_depth; pic.layout.chroma.colour_planes()],
            }),
            false,
        ),
        (Property::Colr(nclx_for_icc(colr, icc.is_some())), false),
    ];
    if let Some(icc) = icc {
        v.push((
            Property::Colr(Colr::Icc {
                restricted: false,
                profile: icc.to_vec(),
            }),
            false,
        ));
    }
    v
}

/// Encode a coded item for `frame` at the visible size `vis`, padding
/// to the codec's alignment and attaching a `clap` when needed.
fn add_picture_item(
    w: &mut HeifWriter,
    frame: &HeifFrame,
    opts: &EncodeOptions,
    extra: Vec<(Property, bool)>,
) -> Result<u32> {
    let a = alignment(opts);
    let (pw, ph) = (
        align_up(frame.width, a).max(a),
        align_up(frame.height, a).max(a),
    );
    let padded = pad_frame(frame, pw, ph)?;
    let pic = encode_picture(&padded, opts)?;
    let mut props = coded_props(&pic, &opts.colr, opts.icc_profile.as_deref());
    props.extend(extra);
    if (pic.coded_width, pic.coded_height) != (frame.width, frame.height) {
        props.push((
            Property::Clap(Clap::for_rect(
                pic.coded_width,
                pic.coded_height,
                CropRect {
                    x: 0,
                    y: 0,
                    width: frame.width,
                    height: frame.height,
                },
            )),
            true,
        ));
    }
    props.extend(transform_props(opts));
    Ok(w.add_coded_item(pic.item_type, pic.data, props))
}

/// Encode `frame` (any planar layout; converted to 8-bit 4:2:0) into a
/// complete HEIF file per `opts`. Returns the file bytes.
pub fn encode_still(frame: &HeifFrame, opts: &EncodeOptions) -> Result<Vec<u8>> {
    frame.validate()?;
    // HEVC items are coded 8-bit 4:2:0; AV1 items keep the picture's
    // own (depth, chroma) pairing up to 12 bits.
    let native_av1 = opts.codec == StillCodec::Av1 && frame.format.bit_depth <= 12;
    let colour = if native_av1 {
        frame.without_alpha().tight()
    } else {
        to_yuv420_8(&frame.without_alpha())?
    };
    let mut w = HeifWriter::new();
    let master = match opts.grid_tile {
        Some(tile) if tile > 0 && (colour.width > tile || colour.height > tile) => {
            // MIAF §7.3.11.4.2: tiles are at least 64 pixels; keep them
            // aligned to the codec block size on both axes.
            let tile = align_up(tile.max(crate::miaf::MIN_TILE_EDGE), alignment(opts) * 2);
            let cols = colour.width.div_ceil(tile);
            let rows = colour.height.div_ceil(tile);
            if rows > 256 || cols > 256 {
                return Err(HeifError::invalid("grid: more than 256 rows or columns"));
            }
            let padded = pad_frame(&colour, cols * tile, rows * tile)?;
            let mut tiles = Vec::with_capacity((rows * cols) as usize);
            for r in 0..rows {
                for c in 0..cols {
                    let t = crop(&padded, c * tile, r * tile, tile, tile)?;
                    let pic = encode_picture(&t, opts)?;
                    let props = coded_props(&pic, &opts.colr, None);
                    let id = w.add_coded_item(pic.item_type, pic.data, props);
                    w.set_hidden(id, true);
                    tiles.push(id);
                }
            }
            let desc = GridDescriptor {
                rows: rows as u16,
                columns: cols as u16,
                output_width: colour.width,
                output_height: colour.height,
            };
            let mut gprops = vec![(
                Property::Colr(nclx_for_icc(&opts.colr, opts.icc_profile.is_some())),
                false,
            )];
            if let Some(icc) = &opts.icc_profile {
                gprops.push((
                    Property::Colr(Colr::Icc {
                        restricted: false,
                        profile: icc.clone(),
                    }),
                    false,
                ));
            }
            gprops.extend(transform_props(opts));
            w.add_grid(desc, &tiles, gprops)?
        }
        _ => add_picture_item(&mut w, &colour, opts, Vec::new())?,
    };
    // Alpha auxiliary: the alpha plane as a picture — monochrome for
    // AV1, monochrome-in-4:2:0 for HEVC (its encoder takes 4:2:0).
    if let Some(alpha) = frame.alpha_as_frame() {
        let a_src = if native_av1 {
            alpha.tight()
        } else {
            to_yuv420_8(&alpha)?
        };
        let a = alignment(opts);
        let (pw, ph) = (
            align_up(a_src.width, a).max(a),
            align_up(a_src.height, a).max(a),
        );
        let extra: &[(&str, &str)] = if opts.codec == StillCodec::Hevc {
            ALPHA_HEVC_OPTIONS
        } else {
            &[]
        };
        let padded = pad_frame(&a_src, pw, ph)?;
        // The alpha is an opacity: full range, no colour description.
        let alpha_colr = Colr::Nclx {
            primaries: 2,
            transfer: 2,
            matrix: 2,
            full_range: true,
        };
        let pic = encode_picture_for(&padded, opts, &alpha_colr, extra)?;
        // Alpha items carry no colour information (the plane is an
        // opacity, not a colour — the shape every third-party producer
        // writes), a single-channel `pixi` and an essential `auxC`.
        let mut props: Vec<(Property, bool)> = coded_props(&pic, &opts.colr, None)
            .into_iter()
            .filter(|(p, _)| !matches!(p, Property::Colr(_)))
            .map(|(p, e)| match p {
                // One channel: the opacity plane (the neutral chroma of
                // the 4:2:0 coding carries nothing).
                Property::Pixi(_) => (
                    Property::Pixi(Pixi {
                        bits_per_channel: vec![pic.layout.bit_depth],
                    }),
                    e,
                ),
                other => (other, e),
            })
            .collect();
        // HEVC alpha items use the codec-specific URN (HEIF §7.5.3.2 /
        // Annex B.2.3.2), the form both HEVC producers write and the
        // one Apple ImageIO opens; AV1 items use the codec-independent
        // CICP URN (§7.5.3.1).
        let urn = match opts.codec {
            StillCodec::Hevc => crate::props::AUX_URN_ALPHA_HEVC,
            StillCodec::Av1 => crate::props::AUX_URN_ALPHA,
        };
        props.push((
            Property::AuxC(crate::props::AuxC {
                aux_type: urn.into(),
                aux_subtype: Vec::new(),
            }),
            true,
        ));
        if (pic.coded_width, pic.coded_height) != (a_src.width, a_src.height) {
            props.push((
                Property::Clap(Clap::for_rect(
                    pic.coded_width,
                    pic.coded_height,
                    CropRect {
                        x: 0,
                        y: 0,
                        width: a_src.width,
                        height: a_src.height,
                    },
                )),
                true,
            ));
        }
        props.extend(transform_props(opts));
        w.add_alpha(master, pic.item_type, pic.data, props, false);
    }
    // Thumbnail.
    if let Some(max_dim) = opts.thumbnail_max_dim {
        let max_dim = max_dim.max(1);
        if colour.width.max(colour.height) > max_dim {
            let scale = colour.width.max(colour.height) as f64 / max_dim as f64;
            let tw = ((colour.width as f64 / scale).round() as u32).max(1);
            let th = ((colour.height as f64 / scale).round() as u32).max(1);
            let small = crate::compose::resize_nearest(&colour, tw, th)?;
            let a = alignment(opts);
            let padded = pad_frame(&small, align_up(tw, a).max(a), align_up(th, a).max(a))?;
            let pic = encode_picture(&padded, opts)?;
            let mut props = coded_props(&pic, &opts.colr, None);
            if (pic.coded_width, pic.coded_height) != (tw, th) {
                props.push((
                    Property::Clap(Clap::for_rect(
                        pic.coded_width,
                        pic.coded_height,
                        CropRect {
                            x: 0,
                            y: 0,
                            width: tw,
                            height: th,
                        },
                    )),
                    true,
                ));
            }
            props.extend(transform_props(opts));
            w.add_thumbnail(master, pic.item_type, pic.data, props);
        }
    }
    // Metadata.
    if let Some(exif) = &opts.exif {
        w.add_exif(master, exif);
    }
    if let Some(xmp) = &opts.xmp {
        w.add_xmp(master, xmp);
    }
    // Gain map → hidden coded item + tmap derived item (HEIF Amd 1
    // §6.6.2.4); the base stays the primary and an altr [tmap, base]
    // group gives tone-map readers the alternate.
    if let Some(gm) = &opts.gain_map {
        gm.frame.validate()?;
        let g_src = if native_av1 {
            gm.frame.without_alpha().tight()
        } else {
            to_yuv420_8(&gm.frame.without_alpha())?
        };
        let a = alignment(opts);
        let padded = pad_frame(
            &g_src,
            align_up(g_src.width, a).max(a),
            align_up(g_src.height, a).max(a),
        )?;
        let gain_colr = Colr::Nclx {
            primaries: 2,
            transfer: 2,
            matrix: if gm.frame.format.chroma == Chroma::Mono {
                2
            } else {
                gm.gain_map_matrix
            },
            full_range: gm.gain_map_full_range,
        };
        let extra: &[(&str, &str)] = if opts.codec == StillCodec::Hevc {
            GAIN_MAP_HEVC_OPTIONS
        } else {
            &[]
        };
        let pic = encode_picture_for(&padded, opts, &gain_colr, extra)?;
        let mut props = coded_props(&pic, &gain_colr, None);
        if gm.frame.format.chroma == Chroma::Mono && pic.layout.chroma != Chroma::Mono {
            // The map rides the luma plane: one channel of information.
            for (p, _) in props.iter_mut() {
                if let Property::Pixi(_) = p {
                    *p = Property::Pixi(Pixi {
                        bits_per_channel: vec![pic.layout.bit_depth],
                    });
                }
            }
        }
        if (pic.coded_width, pic.coded_height) != (g_src.width, g_src.height) {
            props.push((
                Property::Clap(Clap::for_rect(
                    pic.coded_width,
                    pic.coded_height,
                    CropRect {
                        x: 0,
                        y: 0,
                        width: g_src.width,
                        height: g_src.height,
                    },
                )),
                true,
            ));
        }
        let gain_id = w.add_coded_item(pic.item_type, pic.data, props);
        let mut tprops = vec![(
            Property::Pixi(Pixi {
                bits_per_channel: vec![
                    gm.alternate_bit_depth.clamp(8, 16);
                    colour.format.chroma.colour_planes()
                ],
            }),
            false,
        )];
        if let Some(c) = gm.alternate_clli {
            tprops.push((Property::Clli(c), false));
        }
        w.add_tone_map(
            master,
            gain_id,
            &gm.metadata,
            Property::Colr(gm.alternate_colr.clone()),
            tprops,
        )?;
    }
    w.set_primary(master);
    w.write_to_vec()
}

/// Codec options giving the gain-map item's HEVC parameter sets their
/// own ids (VPS / SPS / PPS 2), for the same reason as the alpha's.
const GAIN_MAP_HEVC_OPTIONS: &[(&str, &str)] = &[("vpsid", "2"), ("spsid", "2"), ("ppsid", "2")];

/// Accepted framework pixel formats of the `"heif"` encoder: the planar
/// YCbCr / grey layouts (`HeifFrame::from_core`) plus the packed RGB /
/// RGBA / grey+alpha ones converted by [`packed_to_planar`].
pub const ENCODER_PIXEL_FORMATS: &[oxideav_core::PixelFormat] = &[
    oxideav_core::PixelFormat::Rgb24,
    oxideav_core::PixelFormat::Rgba,
    oxideav_core::PixelFormat::Bgr24,
    oxideav_core::PixelFormat::Bgra,
    oxideav_core::PixelFormat::Rgb48Le,
    oxideav_core::PixelFormat::Rgba64Le,
    oxideav_core::PixelFormat::Gray8,
    oxideav_core::PixelFormat::Gray16Le,
    oxideav_core::PixelFormat::Ya8,
    oxideav_core::PixelFormat::Ya16Le,
    oxideav_core::PixelFormat::Yuv420P,
    oxideav_core::PixelFormat::YuvJ420P,
    oxideav_core::PixelFormat::Yuv422P,
    oxideav_core::PixelFormat::YuvJ422P,
    oxideav_core::PixelFormat::Yuv444P,
    oxideav_core::PixelFormat::YuvJ444P,
    oxideav_core::PixelFormat::Yuva420P,
    oxideav_core::PixelFormat::Yuva444P,
    oxideav_core::PixelFormat::Yuv420P10Le,
    oxideav_core::PixelFormat::Yuv444P10Le,
];

/// Convert a packed RGB / RGBA / BGR / BGRA (8 or 16-bit) or packed
/// grey + alpha frame to the planar layout the encoder consumes: 4:4:4
/// YCbCr (or monochrome for grey + alpha) at the source depth, through
/// the H.273 matrix and range of `colr` (the MIAF default — BT.601,
/// full range — for an ICC / absent one), with the alpha carried as a
/// plane. Row-major, `stride` honoured.
pub fn packed_to_planar(
    vf: &oxideav_core::VideoFrame,
    width: u32,
    height: u32,
    pf: oxideav_core::PixelFormat,
    colr: &Colr,
) -> Result<HeifFrame> {
    use oxideav_core::PixelFormat as P;
    // (bytes per pixel, channel order indices into [r, g, b, a] or [y, a], 16-bit?, has alpha, grey)
    let (bpp, order, wide, alpha, grey): (usize, [usize; 4], bool, bool, bool) = match pf {
        P::Rgb24 => (3, [0, 1, 2, 0], false, false, false),
        P::Rgba => (4, [0, 1, 2, 3], false, true, false),
        P::Bgr24 => (3, [2, 1, 0, 0], false, false, false),
        P::Bgra => (4, [2, 1, 0, 3], false, true, false),
        P::Rgb48Le => (6, [0, 1, 2, 0], true, false, false),
        P::Rgba64Le => (8, [0, 1, 2, 3], true, true, false),
        P::Ya8 => (2, [0, 0, 0, 1], false, true, true),
        P::Ya16Le => (4, [0, 0, 0, 1], true, true, true),
        other => {
            return Err(HeifError::unsupported(format!(
                "framework pixel format {other:?} is neither planar YCbCr / grey nor packed RGB(A)"
            )))
        }
    };
    let planes = vf.image_planes();
    let src = planes
        .first()
        .ok_or_else(|| HeifError::invalid("packed frame without a plane"))?;
    let row_bytes = width as usize * bpp;
    if src.stride < row_bytes || src.data.len() < src.stride * (height as usize - 1) + row_bytes {
        return Err(HeifError::invalid(format!(
            "packed {pf:?} frame too small for {width}x{height}"
        )));
    }
    let depth: u8 = if wide { 16 } else { 8 };
    let max = (1u32 << depth) - 1;
    let fmt = HeifPixelFormat::new(
        if grey { Chroma::Mono } else { Chroma::Yuv444 },
        depth,
        alpha,
    )?;
    let mut out = HeifFrame::zeroed(width, height, fmt)?;
    let alpha_plane = out.format.alpha_plane();
    let read = |px: &[u8], ch: usize| -> u16 {
        if wide {
            u16::from_le_bytes([px[2 * ch], px[2 * ch + 1]])
        } else {
            px[ch] as u16
        }
    };
    // 16-bit scale for the H.273 conversion, then back to `depth`.
    let to16 = |v: u16| -> u16 {
        if wide {
            v
        } else {
            (v as u32 * 257) as u16
        }
    };
    for y in 0..height {
        let row = &src.data[y as usize * src.stride..y as usize * src.stride + row_bytes];
        for (x, px) in row.chunks_exact(bpp).enumerate() {
            let x = x as u32;
            if grey {
                // Luma through the same range mapping as colour.
                let g = to16(read(px, order[0]));
                let ycc = crate::compose::fill_to_ycbcr([g, g, g], depth, Some(colr));
                out.set_sample(0, x, y, ycc[0]);
            } else {
                let rgb16 = [
                    to16(read(px, order[0])),
                    to16(read(px, order[1])),
                    to16(read(px, order[2])),
                ];
                let ycc = crate::compose::fill_to_ycbcr(rgb16, depth, Some(colr));
                out.set_sample(0, x, y, ycc[0]);
                out.set_sample(1, x, y, ycc[1]);
                out.set_sample(2, x, y, ycc[2]);
            }
            if let Some(ap) = alpha_plane {
                let a = read(px, order[3]) as u32;
                out.set_sample(ap, x, y, a.min(max) as u16);
            }
        }
    }
    Ok(out)
}

/// Typed options of the `"heif"` framework encoder (declared schema:
/// `oxideav info heif` lists them; unknown keys are refused). The
/// enum fields' listed default is empty because a `const` schema can
/// only hold `String::new()`; the effective defaults are `hevc`,
/// `intra` and `full` (see [`Default`] and the `help` text).
#[derive(Clone, Debug)]
pub struct HeifEncoderOptions {
    /// `codec`: `hevc` (alias `h265`) or `av1`.
    pub codec: String,
    /// `mode` (HEVC): `intra` (CABAC at `qp`) or `pcm` (lossless).
    pub mode: String,
    /// `qp` (HEVC intra), 0..=51.
    pub qp: u32,
    /// `grid`: tile size for a `grid` primary (0 = none).
    pub grid: u32,
    /// `thumbnail`: largest thumbnail dimension (0 = none).
    pub thumbnail: u32,
    /// `range`: `full` or `limited` sample range of the written `nclx`.
    pub range: String,
    /// `quality` (AV1, `mode=intra`): 0..=100, 100 = lossless.
    pub quality: u32,
    /// `speed` (AV1): `fast` / `balanced` / `thorough`.
    pub speed: String,
    /// `rd` (HEVC intra): mode-decision effort 0..=2 (255 = the
    /// encoder's still default).
    pub rd: u32,
    /// `tiles` (HEVC): `CxR` tile layout (empty = none).
    pub tiles: String,
}

impl Default for HeifEncoderOptions {
    fn default() -> Self {
        Self {
            codec: "hevc".into(),
            mode: "intra".into(),
            qp: 26,
            grid: 0,
            thumbnail: 0,
            range: "full".into(),
            quality: 60,
            speed: "fast".into(),
            rd: 255,
            tiles: String::new(),
        }
    }
}

impl oxideav_core::CodecOptionsStruct for HeifEncoderOptions {
    const SCHEMA: &'static [oxideav_core::OptionField] = &[
        oxideav_core::OptionField {
            name: "codec",
            kind: oxideav_core::OptionKind::Enum(&["hevc", "h265", "av1"]),
            default: oxideav_core::OptionValue::String(String::new()),
            help: "Coded item codec: hevc (heic file) or av1 (avif file, lossless)",
        },
        oxideav_core::OptionField {
            name: "mode",
            kind: oxideav_core::OptionKind::Enum(&["intra", "pcm"]),
            default: oxideav_core::OptionValue::String(String::new()),
            help: "HEVC coding: intra (CABAC at qp) or pcm (lossless)",
        },
        oxideav_core::OptionField {
            name: "qp",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(26),
            help: "HEVC intra quantiser 0..=51 (lower = better)",
        },
        oxideav_core::OptionField {
            name: "grid",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(0),
            help: "Tile the picture into a grid of this size (0 = single item; MIAF 64-px floor)",
        },
        oxideav_core::OptionField {
            name: "thumbnail",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(0),
            help: "Add a thumbnail whose largest dimension is this many pixels (0 = none)",
        },
        oxideav_core::OptionField {
            name: "range",
            kind: oxideav_core::OptionKind::Enum(&["full", "limited"]),
            default: oxideav_core::OptionValue::String(String::new()),
            help: "Sample range written in nclx: full (default) or limited",
        },
        oxideav_core::OptionField {
            name: "quality",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(60),
            help: "AV1 quality 0..=100 for mode=intra (100 = lossless; mode=pcm is lossless too)",
        },
        oxideav_core::OptionField {
            name: "speed",
            kind: oxideav_core::OptionKind::Enum(&["fast", "balanced", "thorough"]),
            default: oxideav_core::OptionValue::String(String::new()),
            help: "AV1 search effort (default fast)",
        },
        oxideav_core::OptionField {
            name: "rd",
            kind: oxideav_core::OptionKind::U32,
            default: oxideav_core::OptionValue::U32(255),
            help: "HEVC intra mode-decision effort 0..=2 (255 = encoder's still default)",
        },
        oxideav_core::OptionField {
            name: "tiles",
            kind: oxideav_core::OptionKind::String,
            default: oxideav_core::OptionValue::String(String::new()),
            help: "HEVC tile layout CxR (e.g. 4x4) for parallel coding; empty = none",
        },
    ];

    fn apply(&mut self, key: &str, value: &oxideav_core::OptionValue) -> CoreResult<()> {
        match key {
            "codec" => self.codec = value.as_str()?.to_string(),
            "mode" => self.mode = value.as_str()?.to_string(),
            "qp" => self.qp = value.as_u32()?,
            "grid" => self.grid = value.as_u32()?,
            "thumbnail" => self.thumbnail = value.as_u32()?,
            "range" => self.range = value.as_str()?.to_string(),
            "quality" => self.quality = value.as_u32()?,
            "speed" => self.speed = value.as_str()?.to_string(),
            "rd" => self.rd = value.as_u32()?,
            "tiles" => self.tiles = value.as_str()?.to_string(),
            other => {
                return Err(CoreError::invalid(format!(
                    "heif: unknown option '{other}'"
                )))
            }
        }
        Ok(())
    }
}

impl HeifEncoderOptions {
    /// The `EncodeOptions` these settings describe.
    pub fn to_encode_options(&self) -> CoreResult<EncodeOptions> {
        let mut opts = EncodeOptions {
            codec: match self.codec.as_str() {
                "hevc" | "h265" => StillCodec::Hevc,
                "av1" => StillCodec::Av1,
                other => {
                    return Err(CoreError::invalid(format!(
                        "heif: unknown codec option '{other}'"
                    )))
                }
            },
            hevc_mode: self.mode.clone(),
            qp: u8::try_from(self.qp.min(51)).unwrap_or(51),
            grid_tile: (self.grid > 0).then_some(self.grid),
            thumbnail_max_dim: (self.thumbnail > 0).then_some(self.thumbnail),
            // AV1: `mode=pcm` is lossless, `mode=intra` codes at `quality`.
            av1_quality: (self.mode != "pcm").then_some(self.quality.min(100) as u8),
            av1_speed: self.speed.clone(),
            hevc_rd: (self.rd <= 2).then_some(self.rd),
            hevc_tiles: (!self.tiles.is_empty()).then_some(self.tiles.clone()),
            ..EncodeOptions::default()
        };
        if let Colr::Nclx { full_range, .. } = &mut opts.colr {
            *full_range = self.range != "limited";
        }
        Ok(opts)
    }
}

/// The `"heif"` framework encoder: every video frame becomes one
/// packet holding a complete HEIF file.
pub struct HeifEncoder {
    params: CodecParameters,
    opts: EncodeOptions,
    queue: std::collections::VecDeque<Packet>,
    flushed: bool,
}

impl HeifEncoder {
    /// Construct from stream parameters; the options are the declared
    /// [`HeifEncoderOptions`] schema (`codec`, `mode`, `qp`, `grid`,
    /// `thumbnail`, `range`); unknown keys are refused.
    pub fn new(params: &CodecParameters) -> CoreResult<Self> {
        let opts = oxideav_core::parse_options::<HeifEncoderOptions>(&params.options)?
            .to_encode_options()?;
        Ok(Self {
            params: params.clone(),
            opts,
            queue: std::collections::VecDeque::new(),
            flushed: false,
        })
    }
}

impl Encoder for HeifEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.params.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.params
    }

    fn send_frame(&mut self, frame: &Frame) -> CoreResult<()> {
        let Frame::Video(vf) = frame else {
            return Err(CoreError::invalid("heif: only video frames can be encoded"));
        };
        let (w, h) = match (self.params.width, self.params.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
            _ => return Err(CoreError::invalid("heif: width / height must be set")),
        };
        let pf = self
            .params
            .pixel_format
            .ok_or_else(|| CoreError::invalid("heif: pixel_format must be set"))?;
        let hf = match HeifFrame::from_core(vf, w, h, pf) {
            Ok(f) => f,
            Err(_) => packed_to_planar(vf, w, h, pf, &self.opts.colr)?,
        };
        let bytes = encode_still(&hf, &self.opts)?;
        let pkt = Packet::new(0, TimeBase::new(1, 1), bytes)
            .with_pts(vf.pts.unwrap_or(0))
            .with_keyframe(true);
        self.queue.push_back(pkt);
        Ok(())
    }

    fn receive_packet(&mut self) -> CoreResult<Packet> {
        match self.queue.pop_front() {
            Some(p) => Ok(p),
            None if self.flushed => Err(CoreError::Eof),
            None => Err(CoreError::NeedMore),
        }
    }

    fn flush(&mut self) -> CoreResult<()> {
        self.flushed = true;
        Ok(())
    }
}

/// Direct factory endpoint for the `"heif"` encoder.
pub fn make_encoder(params: &CodecParameters) -> CoreResult<Box<dyn Encoder>> {
    Ok(Box::new(HeifEncoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_replicates_edges_and_conversion_neutralises_chroma() {
        let mut f = HeifFrame::zeroed(3, 2, HeifPixelFormat::new(Chroma::Mono, 10, false).unwrap())
            .unwrap();
        f.set_sample(0, 2, 1, 1020);
        let p = pad_frame(&f, 4, 4).unwrap();
        assert_eq!(p.sample(0, 3, 3), 1020);
        assert_eq!(p.sample(0, 0, 0), 0);
        let c = to_yuv420_8(&p).unwrap();
        assert_eq!(c.format.chroma, Chroma::Yuv420);
        assert_eq!(c.sample(0, 3, 3), 255);
        assert_eq!(c.sample(1, 1, 1), 128);
        // 4:4:4 → 4:2:0 averages 2x2 chroma blocks.
        let mut q = HeifFrame::zeroed(
            2,
            2,
            HeifPixelFormat::new(Chroma::Yuv444, 8, false).unwrap(),
        )
        .unwrap();
        q.set_sample(1, 0, 0, 100);
        q.set_sample(1, 1, 0, 200);
        q.set_sample(1, 0, 1, 100);
        q.set_sample(1, 1, 1, 200);
        let c = to_yuv420_8(&q).unwrap();
        assert_eq!(c.sample(1, 0, 0), 150);
    }

    #[test]
    fn profile_compat_bits() {
        assert_eq!(profile_compat_flags(1), 1 << 30);
        assert_eq!(profile_compat_flags(3), (1 << 28) | (1 << 30) | (1 << 29));
        assert_eq!(profile_compat_flags(2), (1 << 29) | (1 << 30));
    }
}
