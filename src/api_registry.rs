//! The registry-gated half of the image-crate contract: `decode*`,
//! `decode_all`, `encode*` and the bridges to the framework's
//! `VideoFrame` / `PixelFormat`.
//!
//! HEIF pixels come from the HEVC / AV1 / AVC codec crates, which are
//! framework-only, so everything here needs the default-on `registry`
//! feature. The functions are the *one* implementation: the `"heif"`
//! framework decoder / encoder ([`crate::demux::HeifCodec`],
//! [`crate::encode::HeifEncoder`]) are thin adapters over them.

use std::io::{Read, Write};
use std::time::Duration;

use oxideav_core::{
    CodecId, CodecParameters, Error as CoreError, Frame as CoreFrame, Packet, TimeBase,
};

use crate::api::{
    sequence_tracks, ColorInfo, ColorRange, DecodeOptions, Frame, HeifImage, Metadata, PixelFormat,
    RgbImage, RgbaImage,
};
use crate::compose::{apply_clap, attach_alpha};
use crate::decode::{decode_item, frame_from_planes, DecodedImage, ItemDecoder, ToneMapOutput};
use crate::derived::build_graph;
use crate::encode::{
    coded_depth, encode_sequence, encode_still_into, encode_still_owned, packed_bytes_to_planar,
    EncodeOptions, SequenceInput, StillCodec,
};
use crate::error::{HeifError, Result};
use crate::file::HeifFile;
use crate::image::{Chroma, HeifFrame, HeifPixelFormat, Plane};
use crate::layout::entry_layout;
use crate::props::Colr;
use crate::sequence::{parse_movie, sample_bytes, Movie, SampleEntry, Track};
use crate::writer::HeifWriter;

// ---------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------

/// Decode the primary image to its native layout, colour and metadata
/// filled from the file ([`DecodeOptions::default`]).
pub fn decode(bytes: &[u8]) -> Result<HeifImage> {
    decode_with(bytes, &DecodeOptions::default())
}

/// [`decode`] with limits, strictness and the HEIF selections of
/// `opts` (another item, the tone-mapped rendition, …).
pub fn decode_with(bytes: &[u8], opts: &DecodeOptions) -> Result<HeifImage> {
    opts.check_input(bytes.len())?;
    let file = HeifFile::parse_borrowed(bytes)?;
    decode_file(&file, opts)
}

/// [`decode_with`] over an already parsed file.
pub fn decode_file<D: AsRef<[u8]>>(file: &HeifFile<D>, opts: &DecodeOptions) -> Result<HeifImage> {
    Ok(HeifImage::from(decode_file_item(file, opts)?))
}

/// The item decode behind [`decode_file`], keeping the whole
/// [`DecodedImage`] (depth map, thumbnails, gain map, layers, typed
/// properties): limits and strictness of `opts` applied, the item
/// `opts.item_id` or the primary.
pub fn decode_file_item<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    opts: &DecodeOptions,
) -> Result<DecodedImage> {
    check_strict(file, opts)?;
    let id = match opts.item_id {
        Some(id) => id,
        None => file.primary_item()?.id,
    };
    decode_checked_item(file, id, opts)
}

/// Decode `id` with the geometry limits of `opts` checked against the
/// item's announced output size before any pixel is produced.
fn decode_checked_item<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    id: u32,
    opts: &DecodeOptions,
) -> Result<DecodedImage> {
    let node = build_graph(file, id)?;
    let (w, h) = node.output_size()?;
    opts.check_dims(w, h)?;
    let img = decode_item(file, id, item_decoder(opts))?;
    // A tone-mapped reconstruction / layered output may differ from the
    // announced size (the tmap's own geometry): re-check what came out.
    opts.check_dims(img.frame.width, img.frame.height)?;
    Ok(img)
}

/// The [`ItemDecoder`] `opts` describe (direct codec factories).
pub fn item_decoder(opts: &DecodeOptions) -> ItemDecoder<'static> {
    let mut d = ItemDecoder::direct().with_threads(opts.threads.unwrap_or(1));
    if opts.tone_mapped {
        d = d.with_tone_map(ToneMapOutput::Applied);
    }
    if opts.base_layer_fallback {
        d = d.base_layer_fallback();
    }
    if let Some(n) = opts.reference_white_nits {
        d = d.with_reference_white(n);
    }
    d
}

/// `strict`: a HEIF-family brand is required, and a MIAF-branded file
/// must pass the MIAF checks.
fn check_strict<D: AsRef<[u8]>>(file: &HeifFile<D>, opts: &DecodeOptions) -> Result<()> {
    if !opts.strict {
        return Ok(());
    }
    if !file.file_type.is_heif_family() {
        return Err(HeifError::invalid(
            "strict: ftyp declares no HEIF-family brand",
        ));
    }
    if file.file_type.has_brand(&crate::ftyp::BRAND_MIAF) {
        let report = crate::miaf::check(file, crate::miaf::MiafProfile::Miaf)?;
        if !report.is_conformant() {
            let first = &report.violations[0];
            return Err(HeifError::invalid(format!(
                "strict: {} MIAF violation(s); first: {} ({})",
                report.violations.len(),
                first.message,
                first.clause
            )));
        }
    }
    Ok(())
}

/// Decode the primary image straight to tightly packed 8-bit RGB
/// (alpha dropped).
pub fn decode_rgb8(bytes: &[u8]) -> Result<RgbImage> {
    let img = decode(bytes)?;
    Ok(RgbImage::new(img.width, img.height, img.to_rgb8()))
}

/// Decode the primary image straight to tightly packed 8-bit RGBA (the
/// alpha auxiliary when the item has one, opaque otherwise).
pub fn decode_rgba8(bytes: &[u8]) -> Result<RgbaImage> {
    let img = decode(bytes)?;
    Ok(RgbaImage::new(img.width, img.height, img.to_rgba8()))
}

/// Read a HEIF file to its end and [`decode`] it.
pub fn decode_from<R: Read>(mut r: R) -> Result<HeifImage> {
    let mut bytes = Vec::new();
    r.read_to_end(&mut bytes)?;
    decode(&bytes)
}

/// Every image of the file: the displayable image items first (the
/// primary, then the others by ascending item id; an `altr` group
/// yields its first decodable member), then every sample of every
/// image-sequence track in composition order, each with its display
/// duration from the track timing. Alpha auxiliaries (items and
/// tracks) are composed into their master's frame.
pub fn decode_all(bytes: &[u8]) -> Result<Vec<Frame>> {
    decode_all_with(bytes, &DecodeOptions::default())
}

/// [`decode_all`] with the limits / strictness / thread budget of
/// `opts` (`item_id` and `tone_mapped` are ignored: every displayable
/// item comes back as its base rendition).
pub fn decode_all_with(bytes: &[u8], opts: &DecodeOptions) -> Result<Vec<Frame>> {
    opts.check_input(bytes.len())?;
    let file = HeifFile::parse_borrowed(bytes)?;
    check_strict(&file, opts)?;
    let mut frames = Vec::new();
    let mut first_err = None;
    if let Ok(meta) = file.meta() {
        let primary = file.primary_item().ok().map(|i| i.id);
        let mut entries = meta.display_order();
        entries.sort_by_key(|e| {
            (
                !primary.map(|p| e.contains(&p)).unwrap_or(false),
                e.first().copied().unwrap_or(u32::MAX),
            )
        });
        for alternatives in entries {
            let mut decoded = false;
            for id in alternatives {
                match decode_checked_item(&file, id, opts) {
                    Ok(img) => {
                        frames.push(Frame::new(HeifImage::from(img), None, Some(id), None));
                        decoded = true;
                        break;
                    }
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
            if !decoded {
                if let Some(e) = first_err {
                    return Err(e);
                }
            }
        }
    }
    if let Some(movie) = parse_movie(&file)? {
        for track in sequence_tracks(&movie) {
            frames.extend(decode_track(&file, &movie, track, opts)?);
        }
    }
    if frames.is_empty() {
        return Err(first_err.unwrap_or_else(|| {
            HeifError::invalid("file carries neither image items nor image-sequence tracks")
        }));
    }
    Ok(frames)
}

/// Codec parameters of a visual sample entry (`hvcC` / `av1C` / `avcC`
/// as `extradata`).
fn track_params(entry: &SampleEntry, layout: HeifPixelFormat) -> Result<CodecParameters> {
    let (id, extradata) = match &entry.entry_type {
        b"hvc1" | b"hev1" => (
            crate::decode::CODEC_ID_HEVC,
            entry.hvcc.as_ref().map(|h| h.raw.clone()),
        ),
        b"av01" => (
            crate::decode::CODEC_ID_AV1,
            entry.av1c.as_ref().map(|a| a.raw.clone()),
        ),
        b"avc1" | b"avc3" => (
            crate::decode::CODEC_ID_AVC,
            entry.avcc.as_ref().map(|a| a.raw.clone()),
        ),
        other => {
            return Err(HeifError::unsupported(format!(
                "sample entry type {}",
                crate::boxes::fourcc_str(other)
            )))
        }
    };
    let mut params = CodecParameters::video(CodecId::new(id));
    params.extradata = extradata.unwrap_or_default();
    params.width = Some(entry.width as u32);
    params.height = Some(entry.height as u32);
    params.pixel_format = layout.to_core();
    Ok(params)
}

/// Decode every sample of `track` through one codec instance; the
/// pictures come back in output (composition) order with the sample
/// durations (in track timescale units) they display for.
fn decode_samples<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    track: &Track,
    opts: &DecodeOptions,
) -> Result<(Vec<HeifFrame>, Vec<u32>, HeifPixelFormat)> {
    let entry = track
        .primary_entry()
        .ok_or_else(|| HeifError::invalid(format!("track {}: no sample entry", track.track_id)))?;
    let layout = entry_layout(entry).ok_or_else(|| {
        HeifError::unsupported(format!(
            "track {}: sample entry without a known decoder configuration",
            track.track_id
        ))
    })?;
    let (w, h) = (entry.width as u32, entry.height as u32);
    opts.check_dims(w, h)?;
    let params = track_params(entry, layout)?;
    let mut dec = item_decoder(opts).instantiate(&params)?;
    let time_base = TimeBase::new(1, track.timescale.max(1) as i64);
    let mut out: Vec<HeifFrame> = Vec::with_capacity(track.samples.len());
    let drain = |dec: &mut Box<dyn oxideav_core::Decoder>, out: &mut Vec<HeifFrame>| loop {
        match dec.receive_frame() {
            Ok(CoreFrame::Video(v)) => {
                out.push(frame_from_planes(v, layout, Some((w, h)), track.track_id)?)
            }
            Ok(_) => {}
            Err(CoreError::NeedMore) | Err(CoreError::Eof) => return Ok::<(), HeifError>(()),
            Err(e) => {
                return Err(HeifError::invalid(format!(
                    "track {}: decode failed: {e}",
                    track.track_id
                )))
            }
        }
    };
    for (i, s) in track.samples.iter().enumerate() {
        let data = sample_bytes(file, s)?.to_vec();
        let pkt = Packet::new(0, time_base, data)
            .with_pts(s.pts() as i64)
            .with_keyframe(s.is_sync);
        dec.send_packet(&pkt).map_err(|e| {
            HeifError::invalid(format!(
                "track {} sample {i}: decoder rejected the sample: {e}",
                track.track_id
            ))
        })?;
        drain(&mut dec, &mut out)?;
    }
    dec.flush()
        .map_err(|e| HeifError::invalid(format!("track {}: flush: {e}", track.track_id)))?;
    drain(&mut dec, &mut out)?;
    // Durations in composition order.
    let mut by_pts: Vec<(u64, u32)> = track
        .samples
        .iter()
        .map(|s| (s.pts(), s.duration))
        .collect();
    by_pts.sort_by_key(|(pts, _)| *pts);
    let durations = by_pts.into_iter().map(|(_, d)| d).collect();
    Ok((out, durations, layout))
}

/// The frames of one image-sequence track (its alpha track composed
/// in, its sample entry's `clap` applied).
fn decode_track<D: AsRef<[u8]>>(
    file: &HeifFile<D>,
    movie: &Movie,
    track: &Track,
    opts: &DecodeOptions,
) -> Result<Vec<Frame>> {
    let entry = track
        .primary_entry()
        .ok_or_else(|| HeifError::invalid(format!("track {}: no sample entry", track.track_id)))?;
    let (pictures, durations, _) = decode_samples(file, track, opts)?;
    let alpha = match movie.alpha_track_of(track.track_id) {
        Some(a) => Some(decode_samples(file, a, opts)?.0),
        None => None,
    };
    let colr = entry
        .colr
        .iter()
        .find(|c| matches!(c, Colr::Nclx { .. }))
        .cloned()
        .unwrap_or(Colr::MIAF_DEFAULT);
    let color = ColorInfo::from_colr(&colr);
    let metadata = Metadata::new(
        entry.colr.iter().find_map(|c| match c {
            Colr::Icc { profile, .. } => Some(profile.clone()),
            _ => None,
        }),
        None,
        None,
        None,
    );
    let timescale = track.timescale.max(1) as u64;
    let mut frames = Vec::with_capacity(pictures.len());
    for (i, mut pic) in pictures.into_iter().enumerate() {
        if let Some(alpha) = alpha.as_ref().and_then(|a| a.get(i)) {
            pic = attach_alpha(&pic, alpha)?;
        }
        if let Some(clap) = &entry.clap {
            pic = apply_clap(&pic, clap)?;
        }
        let delay = durations.get(i).map(|d| {
            let d = *d as u64;
            Duration::new(
                d / timescale,
                ((d % timescale) * 1_000_000_000 / timescale) as u32,
            )
        });
        frames.push(Frame::new(
            HeifImage::from_frame(pic, color, metadata.clone()),
            delay,
            None,
            Some(track.track_id),
        ));
    }
    Ok(frames)
}

impl From<DecodedImage> for HeifImage {
    /// The output image with the item's colour information and
    /// metadata (depth map, thumbnails, gain map and layers are left
    /// behind: hold the [`DecodedImage`] for those).
    fn from(d: DecodedImage) -> Self {
        let color = ColorInfo::from_colr(&d.nclx);
        let metadata = Metadata::new(d.icc_profile, d.exif, d.xmp.map(String::into_bytes), None);
        HeifImage::from_frame(d.frame, color, metadata)
    }
}

// ---------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------

/// Encode `image` as a complete HEIF (or AVIF, per
/// [`EncodeOptions::codec`]) file. Planar YCbCr / grey images are coded
/// in the codec's layout (HEVC: 4:2:0 at the coded depth; AV1: the
/// image's own chroma layout); the image's [`HeifImage::color`] is
/// written as the `colr` (code points left unspecified fall back to
/// `opts.colr`). Packed `Rgb24` / `Rgba` and planar `Gbrp*` images are
/// converted to YCbCr through `opts.colr`'s matrix (an AV1 `Gbrp*`
/// image at ≤ 12 bits is coded as-is with the identity matrix); the
/// alpha channel becomes the alpha auxiliary item. [`HeifImage::metadata`]
/// is embedded unless `opts` carries its own.
pub fn encode(image: &HeifImage, opts: &EncodeOptions) -> Result<Vec<u8>> {
    image.validate()?;
    if image.format.is_packed() {
        let plane = &image.planes[0];
        return encode_packed_bytes(
            image.width,
            image.height,
            image.format,
            plane.stride,
            &plane.data,
            &image.color,
            &image.metadata,
            opts,
        );
    }
    encode_owned(image.clone(), opts)
}

/// Encode `frames` as one file — the mirror of [`decode_all`]. Frames
/// without a `delay` become image items (the first is the primary, the
/// rest a burst in order; one delay-less frame is exactly [`encode`]);
/// frames with a `delay` become the samples of an image-sequence track
/// (timescale 1000, every sample a sync sample, an alpha track when the
/// pictures carry alpha), all sharing frame 0's geometry, codec and
/// coding layout. Both kinds may be mixed, as [`decode_all`] returns
/// them. Each image's `color` / `metadata` are written as for a still
/// (item frames: per item; sequence frames: the first picture's `colr`
/// / ICC on the sample entry).
///
/// `decode_all(encode_all(frames)) == frames` (planes, colour, delays)
/// holds for planar `Yuv420P` frames coded with the lossless HEVC mode
/// (`with_hevc_mode("pcm")`); the lossy defaults preserve geometry,
/// delays and colour. Sequence frames read back with `track_id`
/// `Some(1)` and item frames with their `item_id`.
pub fn encode_all(frames: &[Frame], opts: &EncodeOptions) -> Result<Vec<u8>> {
    if frames.is_empty() {
        return Err(HeifError::invalid("encode_all needs at least one frame"));
    }
    if let [single] = frames {
        if single.delay.is_none() {
            return encode(&single.image, opts);
        }
    }
    let mut still: Option<HeifWriter> = None;
    for f in frames.iter().filter(|f| f.delay.is_none()) {
        let (frame, o) = prepare_item(&f.image, opts)?;
        let first = still.is_none();
        let w = still.get_or_insert_with(HeifWriter::new);
        let id = encode_still_into(w, &frame, &o)?;
        if first {
            w.set_primary(id);
        }
    }
    let timed: Vec<SequenceInput> = frames
        .iter()
        .filter_map(|f| f.delay.map(|d| (f, d)))
        .map(|(f, d)| {
            let (frame, o) = prepare_item(&f.image, opts)?;
            Ok(SequenceInput {
                frame,
                opts: o,
                duration_ms: u32::try_from(d.as_millis()).unwrap_or(u32::MAX),
            })
        })
        .collect::<Result<_>>()?;
    match (timed.is_empty(), still) {
        (true, Some(w)) => w.write_to_vec(),
        (true, None) => unreachable!("frames are non-empty"),
        (false, still) => encode_sequence(timed, still),
    }
}

/// The planar frame [`encode`] codes for `image` (packed RGB converted
/// through the effective `colr`, planar RGB converted or kept per the
/// codec, YCbCr / grey copied) plus the options carrying the image's
/// colour and metadata.
fn prepare_item(image: &HeifImage, opts: &EncodeOptions) -> Result<(HeifFrame, EncodeOptions)> {
    image.validate()?;
    let colr = effective_colr(&image.color, image.format, opts);
    let opts = effective_options(&image.metadata, colr.clone(), opts);
    if image.format.is_packed() {
        let plane = &image.planes[0];
        let bpp = image.format.packed_bytes_per_pixel().unwrap_or(4);
        let target = packed_target(&opts, false, 8, image.format == PixelFormat::Rgba)?;
        let frame = packed_bytes_to_planar(
            &plane.data,
            plane.stride,
            image.width,
            image.height,
            bpp,
            [0, 1, 2, 3],
            false,
            false,
            &colr,
            target,
        )?;
        return Ok((frame, opts));
    }
    let planar = image.to_frame()?;
    if image.format.is_planar_rgb() {
        let (p, t) = match &colr {
            Colr::Nclx {
                primaries,
                transfer,
                ..
            } => (*primaries, *transfer),
            _ => (1, 13),
        };
        let identity = Colr::Nclx {
            primaries: p,
            transfer: t,
            matrix: 0,
            full_range: true,
        };
        if opts.codec == StillCodec::Av1 && planar.format.bit_depth <= 12 {
            let opts = EncodeOptions {
                colr: identity,
                ..opts
            };
            return Ok((planar, opts));
        }
        let rgb = crate::rgb::to_rgb(&planar, Some(&identity))?;
        drop(planar);
        let ycc = crate::rgb::from_rgb(&rgb, Some(&opts.colr), Chroma::Yuv444)?;
        return Ok((ycc, opts));
    }
    Ok((planar, opts))
}

/// [`encode`] taking the image by value: a planar image already in the
/// coding layout is handed to the codec without a copy of its planes.
pub fn encode_owned(image: HeifImage, opts: &EncodeOptions) -> Result<Vec<u8>> {
    image.validate()?;
    let colr = effective_colr(&image.color, image.format, opts);
    let opts = effective_options(&image.metadata, colr.clone(), opts);
    if image.format.is_packed() {
        return encode(&image, &opts);
    }
    if image.format.is_planar_rgb() {
        // Planar RGB in: AV1 codes the G, B, R planes as a 4:4:4 item
        // with `matrix_coefficients = 0` (H.273 identity, no conversion
        // — lossless stays lossless); the HEVC path, which codes 4:2:0,
        // converts through the configured matrix first.
        let planar = image.into_frame()?;
        let (p, t) = match &colr {
            Colr::Nclx {
                primaries,
                transfer,
                ..
            } => (*primaries, *transfer),
            _ => (1, 13),
        };
        let identity = Colr::Nclx {
            primaries: p,
            transfer: t,
            matrix: 0,
            full_range: true,
        };
        return if opts.codec == StillCodec::Av1 && planar.format.bit_depth <= 12 {
            let opts = EncodeOptions {
                colr: identity,
                ..opts
            };
            encode_still_owned(planar, &opts)
        } else {
            let rgb = crate::rgb::to_rgb(&planar, Some(&identity))?;
            drop(planar);
            let ycc = crate::rgb::from_rgb(&rgb, Some(&opts.colr), Chroma::Yuv444)?;
            drop(rgb);
            encode_still_owned(ycc, &opts)
        };
    }
    encode_still_owned(image.into_frame()?, &opts)
}

/// Encode tightly packed 8-bit RGB (`3 × width × height` bytes) at the
/// production defaults of `opts`: HEVC 4:2:0 (AV1 keeps
/// [`EncodeOptions::chroma`]) through `opts.colr`'s matrix and range.
pub fn encode_rgb8(width: u32, height: u32, rgb: &[u8], opts: &EncodeOptions) -> Result<Vec<u8>> {
    encode_packed_bytes(
        width,
        height,
        PixelFormat::Rgb24,
        width as usize * 3,
        rgb,
        &ColorInfo::srgb(),
        &Metadata::default(),
        opts,
    )
}

/// Encode tightly packed 8-bit RGBA (`4 × width × height` bytes); the
/// alpha channel becomes the alpha auxiliary item.
pub fn encode_rgba8(width: u32, height: u32, rgba: &[u8], opts: &EncodeOptions) -> Result<Vec<u8>> {
    encode_packed_bytes(
        width,
        height,
        PixelFormat::Rgba,
        width as usize * 4,
        rgba,
        &ColorInfo::srgb(),
        &Metadata::default(),
        opts,
    )
}

/// [`encode`] into a writer.
pub fn encode_to<W: Write>(image: &HeifImage, opts: &EncodeOptions, mut w: W) -> Result<()> {
    let bytes = encode(image, opts)?;
    w.write_all(&bytes)?;
    Ok(())
}

/// The packed path shared by [`encode`], [`encode_rgb8`] /
/// [`encode_rgba8`] and the framework encoder: packed 8-bit RGB(A)
/// rows straight into the coding layout, row pair by row pair.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_packed_bytes(
    width: u32,
    height: u32,
    format: PixelFormat,
    stride: usize,
    data: &[u8],
    color: &ColorInfo,
    metadata: &Metadata,
    opts: &EncodeOptions,
) -> Result<Vec<u8>> {
    let bpp = format.packed_bytes_per_pixel().ok_or_else(|| {
        HeifError::unsupported(format!("{format:?} is not a packed 8-bit RGB layout"))
    })?;
    if width == 0 || height == 0 {
        return Err(HeifError::invalid("zero-sized image"));
    }
    let row = width as usize * bpp;
    if stride < row || data.len() < stride * (height as usize - 1) + row {
        return Err(HeifError::invalid(format!(
            "{} bytes at stride {stride} for {width}x{height}x{bpp}",
            data.len()
        )));
    }
    let colr = effective_colr(color, format, opts);
    let opts = effective_options(metadata, colr.clone(), opts);
    let target = packed_target(&opts, false, 8, format == PixelFormat::Rgba)?;
    let frame = packed_bytes_to_planar(
        data,
        stride,
        width,
        height,
        bpp,
        [0, 1, 2, 3],
        false,
        false,
        &colr,
        target,
    )?;
    encode_still_owned(frame, &opts)
}

/// The coding layout a packed source of `depth` bits is converted to
/// for the configured codec: 4:2:0 for HEVC (monochrome rides 4:2:0
/// too), [`EncodeOptions::chroma`] (default 4:2:0; monochrome for grey
/// sources) for AV1, at the coded depth, with the source's alpha.
pub(crate) fn packed_target(
    opts: &EncodeOptions,
    grey: bool,
    depth: u8,
    alpha: bool,
) -> Result<HeifPixelFormat> {
    let coded = coded_depth(depth, opts.hevc_depth)?;
    let chroma = match opts.codec {
        StillCodec::Hevc => Chroma::Yuv420,
        StillCodec::Av1 if grey => Chroma::Mono,
        StillCodec::Av1 => opts.chroma.unwrap_or(Chroma::Yuv420),
    };
    HeifPixelFormat::new(chroma, coded, alpha)
}

/// The `colr` an image is written with: `opts.colr` (the MIAF default
/// unless configured) refined by the image's colour description — code
/// points that are not "unspecified" (2) replace the defaults, a
/// specified range replaces the default range. For packed / planar RGB
/// sources the image's matrix describes RGB, not the YCbCr the encoder
/// derives, so only primaries / transfer / range are taken.
pub(crate) fn effective_colr(color: &ColorInfo, format: PixelFormat, opts: &EncodeOptions) -> Colr {
    let rgb_source = format.is_packed() || format.is_planar_rgb();
    let Colr::Nclx {
        primaries,
        transfer,
        matrix,
        full_range,
    } = &opts.colr
    else {
        return opts.colr.clone();
    };
    let pick = |code: u8, default: u16| if code == 2 { default } else { code as u16 };
    let range = match color.range {
        ColorRange::Full => true,
        ColorRange::Limited => false,
        _ => *full_range,
    };
    Colr::Nclx {
        primaries: pick(color.primaries, *primaries),
        transfer: pick(color.transfer, *transfer),
        matrix: if rgb_source || color.matrix == 0 {
            *matrix
        } else {
            pick(color.matrix, *matrix)
        },
        full_range: range,
    }
}

/// `opts` with the effective `colr` and the image's metadata where
/// `opts` carries none.
fn effective_options(metadata: &Metadata, colr: Colr, opts: &EncodeOptions) -> EncodeOptions {
    let mut o = opts.clone();
    o.colr = colr;
    if o.icc_profile.is_none() {
        o.icc_profile = metadata.icc.clone();
    }
    if o.exif.is_none() {
        o.exif = metadata.exif.clone();
    }
    if o.xmp.is_none() {
        o.xmp = metadata
            .xmp
            .as_ref()
            .map(|x| String::from_utf8_lossy(x).into_owned());
    }
    o
}

// ---------------------------------------------------------------------
// Framework bridges
// ---------------------------------------------------------------------

impl From<PixelFormat> for oxideav_core::PixelFormat {
    /// One to one by name.
    fn from(f: PixelFormat) -> Self {
        use oxideav_core::PixelFormat as C;
        use PixelFormat::*;
        match f {
            Gray8 => C::Gray8,
            Gray10Le => C::Gray10Le,
            Gray12Le => C::Gray12Le,
            Gray16Le => C::Gray16Le,
            Yuv420P => C::Yuv420P,
            Yuv420P10Le => C::Yuv420P10Le,
            Yuv420P12Le => C::Yuv420P12Le,
            Yuv420P16Le => C::Yuv420P16Le,
            Yuva420P => C::Yuva420P,
            Yuva420P10Le => C::Yuva420P10Le,
            Yuv422P => C::Yuv422P,
            Yuv422P10Le => C::Yuv422P10Le,
            Yuv422P12Le => C::Yuv422P12Le,
            Yuv422P16Le => C::Yuv422P16Le,
            Yuva422P => C::Yuva422P,
            Yuva422P10Le => C::Yuva422P10Le,
            Yuva422P12Le => C::Yuva422P12Le,
            Yuva422P16Le => C::Yuva422P16Le,
            Yuv444P => C::Yuv444P,
            Yuv444P10Le => C::Yuv444P10Le,
            Yuv444P12Le => C::Yuv444P12Le,
            Yuv444P16Le => C::Yuv444P16Le,
            Yuva444P => C::Yuva444P,
            Yuva444P10Le => C::Yuva444P10Le,
            Yuva444P12Le => C::Yuva444P12Le,
            Yuva444P16Le => C::Yuva444P16Le,
            Gbrp8 => C::Gbrp8,
            Gbrap8 => C::Gbrap8,
            Gbrp10Le => C::Gbrp10Le,
            Gbrap10Le => C::Gbrap10Le,
            Gbrp12Le => C::Gbrp12Le,
            Gbrap12Le => C::Gbrap12Le,
            Gbrp14Le => C::Gbrp14Le,
            Gbrap14Le => C::Gbrap14Le,
            Gbrp16Le => C::Gbrp16Le,
            Gbrap16Le => C::Gbrap16Le,
            Rgb24 => C::Rgb24,
            Rgba => C::Rgba,
        }
    }
}

impl TryFrom<oxideav_core::PixelFormat> for PixelFormat {
    type Error = HeifError;

    /// The variant of the same name; the framework's full-range
    /// `YuvJ*` labels map to the plain YCbCr variants (range lives in
    /// [`ColorInfo`]). `Unsupported` for layouts this crate has no
    /// variant for.
    fn try_from(f: oxideav_core::PixelFormat) -> Result<Self> {
        use oxideav_core::PixelFormat as C;
        use PixelFormat::*;
        Ok(match f {
            C::Gray8 => Gray8,
            C::Gray10Le => Gray10Le,
            C::Gray12Le => Gray12Le,
            C::Gray16Le => Gray16Le,
            C::Yuv420P | C::YuvJ420P => Yuv420P,
            C::Yuv420P10Le => Yuv420P10Le,
            C::Yuv420P12Le => Yuv420P12Le,
            C::Yuv420P16Le => Yuv420P16Le,
            C::Yuva420P => Yuva420P,
            C::Yuva420P10Le => Yuva420P10Le,
            C::Yuv422P | C::YuvJ422P => Yuv422P,
            C::Yuv422P10Le => Yuv422P10Le,
            C::Yuv422P12Le => Yuv422P12Le,
            C::Yuv422P16Le => Yuv422P16Le,
            C::Yuva422P => Yuva422P,
            C::Yuva422P10Le => Yuva422P10Le,
            C::Yuva422P12Le => Yuva422P12Le,
            C::Yuva422P16Le => Yuva422P16Le,
            C::Yuv444P | C::YuvJ444P => Yuv444P,
            C::Yuv444P10Le => Yuv444P10Le,
            C::Yuv444P12Le => Yuv444P12Le,
            C::Yuv444P16Le => Yuv444P16Le,
            C::Yuva444P => Yuva444P,
            C::Yuva444P10Le => Yuva444P10Le,
            C::Yuva444P12Le => Yuva444P12Le,
            C::Yuva444P16Le => Yuva444P16Le,
            C::Gbrp8 => Gbrp8,
            C::Gbrap8 => Gbrap8,
            C::Gbrp10Le => Gbrp10Le,
            C::Gbrap10Le => Gbrap10Le,
            C::Gbrp12Le => Gbrp12Le,
            C::Gbrap12Le => Gbrap12Le,
            C::Gbrp14Le => Gbrp14Le,
            C::Gbrap14Le => Gbrap14Le,
            C::Gbrp16Le => Gbrp16Le,
            C::Gbrap16Le => Gbrap16Le,
            C::Rgb24 => Rgb24,
            C::Rgba => Rgba,
            other => {
                return Err(HeifError::unsupported(format!(
                    "framework pixel format {other:?} has no HEIF image layout"
                )))
            }
        })
    }
}

impl PixelFormat {
    /// The framework label the `"heif"` decoder emits for this layout
    /// under `color`: the 8-bit YCbCr variants carry the full-range
    /// `YuvJ*` label when the range is full.
    pub fn to_core_labelled(self, color: &ColorInfo) -> oxideav_core::PixelFormat {
        let pf = oxideav_core::PixelFormat::from(self);
        if color.range == ColorRange::Full && !self.is_planar_rgb() {
            crate::image::core_bridge::full_range_variant(pf)
        } else {
            pf
        }
    }
}

impl ColorInfo {
    /// The framework colour-signal record.
    pub fn to_color_signal(&self) -> oxideav_core::ColorSignal {
        let mut s = oxideav_core::ColorSignal::from_code_points(
            self.primaries,
            self.transfer,
            self.matrix,
            self.is_full_range(),
        );
        if self.range == ColorRange::Unspecified {
            s.range = oxideav_core::ColorRange::Unspecified;
        }
        s
    }

    /// From the framework colour-signal record (code points as they
    /// are; an unspecified range stays unspecified).
    pub fn from_color_signal(s: &oxideav_core::ColorSignal) -> Self {
        Self::new(
            match s.range {
                oxideav_core::ColorRange::Full => ColorRange::Full,
                oxideav_core::ColorRange::Limited => ColorRange::Limited,
                _ => ColorRange::Unspecified,
            },
            s.primaries.code_point(),
            s.transfer.code_point(),
            s.matrix.code_point(),
        )
    }
}

impl HeifImage {
    /// Into a framework frame plus the label it carries (the planes
    /// move; the colour description rides as the frame's
    /// `ColorSignal`).
    pub fn into_video_frame(self) -> (oxideav_core::VideoFrame, oxideav_core::PixelFormat) {
        let pf = self.format.to_core_labelled(&self.color);
        let planes = self
            .planes
            .into_iter()
            .map(|p| oxideav_core::VideoPlane {
                stride: p.stride,
                data: p.data,
            })
            .collect();
        (
            oxideav_core::VideoFrame { pts: None, planes }
                .with_color_signal(self.color.to_color_signal()),
            pf,
        )
    }

    /// From a framework frame and the stream parameters that describe
    /// it (`width`, `height`, `pixel_format`): the planar YCbCr / grey /
    /// `Gbrp*` layouts and packed `Rgb24` / `Rgba` (planes copied). The
    /// colour description comes from the frame's `ColorSignal`, else the
    /// parameters' (unspecified when neither says).
    pub fn from_video_frame(
        frame: &oxideav_core::VideoFrame,
        params: &CodecParameters,
    ) -> Result<Self> {
        let (width, height) = match (params.width, params.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
            _ => return Err(HeifError::invalid("codec parameters without a geometry")),
        };
        let pf = params
            .pixel_format
            .ok_or_else(|| HeifError::invalid("codec parameters without a pixel format"))?;
        let mut img = Self::from_video_frame_parts(frame, width, height, pf)?;
        if frame.color_signal().is_none() {
            img.color = ColorInfo::from_color_signal(&params.color_signal);
        }
        Ok(img)
    }

    /// [`HeifImage::from_video_frame`] with the geometry and label given
    /// directly; the colour description is the frame's signal
    /// (unspecified without one).
    pub fn from_video_frame_parts(
        frame: &oxideav_core::VideoFrame,
        width: u32,
        height: u32,
        pf: oxideav_core::PixelFormat,
    ) -> Result<Self> {
        let format = PixelFormat::try_from(pf)?;
        let need = format.plane_count();
        let planes = frame.image_planes();
        if planes.len() < need {
            return Err(HeifError::invalid(format!(
                "framework frame has {} planes, {pf:?} needs {need}",
                planes.len()
            )));
        }
        let mut img = HeifImage::new(
            width,
            height,
            format,
            planes[..need]
                .iter()
                .map(|p| Plane::new(p.stride, p.data.clone()))
                .collect(),
        )?;
        img.color = match frame.color_signal() {
            Some(s) => ColorInfo::from_color_signal(&s),
            None => ColorInfo::unspecified(),
        };
        Ok(img)
    }
}

impl TryFrom<(&oxideav_core::VideoFrame, &CodecParameters)> for HeifImage {
    type Error = HeifError;

    fn try_from((frame, params): (&oxideav_core::VideoFrame, &CodecParameters)) -> Result<Self> {
        Self::from_video_frame(frame, params)
    }
}

impl From<HeifImage> for oxideav_core::VideoFrame {
    fn from(img: HeifImage) -> Self {
        img.into_video_frame().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_colr_follows_the_framework_rule() {
        let opts = EncodeOptions::default();
        // Unspecified image colour → the options' colr.
        let c = effective_colr(&ColorInfo::unspecified(), PixelFormat::Yuv420P, &opts);
        assert_eq!(c, Colr::MIAF_DEFAULT);
        // A decoded limited-range BT.709 image keeps its description.
        let bt709 = ColorInfo::new(ColorRange::Limited, 1, 1, 1);
        assert_eq!(
            effective_colr(&bt709, PixelFormat::Yuv420P, &opts),
            Colr::Nclx {
                primaries: 1,
                transfer: 1,
                matrix: 1,
                full_range: false
            }
        );
        // RGB sources: the matrix is the options' (it describes the
        // YCbCr the encoder derives), range / primaries the image's.
        let rgb = ColorInfo::new(ColorRange::Limited, 9, 16, 0);
        assert_eq!(
            effective_colr(&rgb, PixelFormat::Rgb24, &opts),
            Colr::Nclx {
                primaries: 9,
                transfer: 16,
                matrix: 6,
                full_range: false
            }
        );
        // An ICC colr in the options is kept verbatim.
        let icc = EncodeOptions::default().with_colr(Colr::Icc {
            restricted: false,
            profile: vec![1, 2, 3],
        });
        assert!(effective_colr(&bt709, PixelFormat::Yuv420P, &icc).is_icc());
    }

    #[test]
    fn pixel_format_maps_one_to_one_with_core() {
        use oxideav_core::PixelFormat as C;
        for f in [
            PixelFormat::Gray8,
            PixelFormat::Yuv420P10Le,
            PixelFormat::Yuva444P16Le,
            PixelFormat::Gbrap14Le,
            PixelFormat::Rgba,
        ] {
            let c = C::from(f);
            assert_eq!(format!("{c:?}"), format!("{f:?}"), "same name");
            assert_eq!(PixelFormat::try_from(c).unwrap(), f);
        }
        assert_eq!(
            PixelFormat::try_from(C::YuvJ420P).unwrap(),
            PixelFormat::Yuv420P
        );
        assert!(PixelFormat::try_from(C::Nv12).is_err());
        let full = ColorInfo::default();
        assert_eq!(PixelFormat::Yuv420P.to_core_labelled(&full), C::YuvJ420P);
        assert_eq!(PixelFormat::Yuva420P.to_core_labelled(&full), C::Yuva420P);
        assert_eq!(PixelFormat::Gbrp8.to_core_labelled(&full), C::Gbrp8);
        let limited = ColorInfo::new(ColorRange::Limited, 1, 1, 1);
        assert_eq!(PixelFormat::Yuv420P.to_core_labelled(&limited), C::Yuv420P);
    }

    #[test]
    fn video_frame_round_trip_keeps_planes_and_signal() {
        let img = HeifImage::from_rgba8(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8])
            .unwrap()
            .with_color(ColorInfo::new(ColorRange::Limited, 9, 16, 9));
        let (vf, pf) = img.clone().into_video_frame();
        assert_eq!(pf, oxideav_core::PixelFormat::Rgba);
        let mut params = CodecParameters::video(CodecId::new("heif"));
        params.width = Some(2);
        params.height = Some(1);
        params.pixel_format = Some(pf);
        let back = HeifImage::from_video_frame(&vf, &params).unwrap();
        assert_eq!(back, img);
        assert_eq!(HeifImage::try_from((&vf, &params)).unwrap(), img);
        // Without a frame signal the parameters' colour is taken.
        let bare = oxideav_core::VideoFrame {
            pts: None,
            planes: vf.planes.clone(),
        };
        let p2 = params
            .clone()
            .with_color_signal(ColorInfo::new(ColorRange::Limited, 9, 16, 9).to_color_signal());
        assert_eq!(
            HeifImage::from_video_frame(&bare, &p2).unwrap().color,
            ColorInfo::new(ColorRange::Limited, 9, 16, 9)
        );
        // Unspecified range survives the signal.
        let u = HeifImage::from_rgb8(1, 1, vec![0, 0, 0])
            .unwrap()
            .with_color(ColorInfo::unspecified());
        let (vf, _) = u.into_video_frame();
        assert_eq!(
            vf.color_signal().unwrap().range,
            oxideav_core::ColorRange::Unspecified
        );
        assert!(
            HeifImage::from_video_frame_parts(&vf, 4, 4, oxideav_core::PixelFormat::Rgb24).is_err()
        );
        let mut p3 = CodecParameters::video(CodecId::new("heif"));
        p3.pixel_format = Some(oxideav_core::PixelFormat::Rgb24);
        assert!(
            HeifImage::from_video_frame(&vf, &p3).is_err(),
            "no geometry"
        );
    }
}
