//! The extents of a written image sequence (ISO/IEC 14496-12 §8.3.2 /
//! §8.5.2, ISO/IEC 14496-15 §4.5):
//!
//! * the sample entry documents the *cropped* frame size — block
//!   padding is the codec's business (its conformance window), never
//!   the track's;
//! * a `clap` is written only for what a codec cannot crop itself (the
//!   odd last column / row of a subsampled HEVC / VVC picture, the
//!   macroblock padding of AVC);
//! * the `tkhd` width / height is the presentation size: the clean
//!   aperture when there is one, the sample entry's size otherwise —
//!   so the §8.3.2 scale factor (`tkhd` / clean aperture, or `tkhd` /
//!   sample entry) is 1 on both axes for every sequence this crate
//!   writes.
#![cfg(feature = "registry")]

mod common;

use std::time::Duration;

use oxideav_heif::sequence::parse_movie;
use oxideav_heif::{
    EncodeOptions, Frame, HeifFile, HeifImage, PixelFormat, Plane, StillCodec, decode_all,
    encode_all,
};

fn picture(w: u32, h: u32, seed: u8) -> HeifImage {
    let (wu, hu) = (w as usize, h as usize);
    let (cw, ch) = (wu.div_ceil(2), hu.div_ceil(2));
    let y: Vec<u8> = (0..wu * hu)
        .map(|i| 16 + ((i * 7 + seed as usize * 31) % 220) as u8)
        .collect();
    let c: Vec<u8> = (0..cw * ch)
        .map(|i| 16 + ((i * 3 + seed as usize) % 224) as u8)
        .collect();
    HeifImage::new(
        w,
        h,
        PixelFormat::Yuv420P,
        vec![
            Plane::new(wu, y),
            Plane::new(cw, c.clone()),
            Plane::new(cw, c),
        ],
    )
    .unwrap()
}

fn sequence(w: u32, h: u32, opts: &EncodeOptions) -> Vec<u8> {
    let frames: Vec<Frame> = (0..2)
        .map(|i| {
            Frame::new(
                picture(w, h, i),
                Some(Duration::from_millis(40)),
                None,
                None,
            )
        })
        .collect();
    encode_all(&frames, opts).unwrap()
}

/// `(tkhd size, sample entry size)` of every track.
fn track_sizes(bytes: &[u8]) -> Vec<((u32, u32), (u16, u16))> {
    let f = HeifFile::parse(bytes).unwrap();
    let mv = parse_movie(&f).unwrap().expect("moov");
    mv.tracks
        .iter()
        .map(|t| {
            let e = t.primary_entry().unwrap();
            ((t.width, t.height), (e.width, e.height))
        })
        .collect()
}

/// The 16.16 `tkhd` width / height fields, read raw from the first
/// `tkhd` box (version 0: 72 bytes into the body, after the matrix).
fn raw_tkhd_fixed(bytes: &[u8]) -> (u32, u32) {
    let at = bytes
        .windows(4)
        .position(|w| w == b"tkhd")
        .expect("tkhd box");
    let body = &bytes[at + 4..];
    assert_eq!(body[0], 0, "version 0 tkhd");
    let field = |o: usize| u32::from_be_bytes(body[o..o + 4].try_into().unwrap());
    (field(4 + 72), field(4 + 76))
}

/// `(clean aperture size, when the entry has a clap)` per track.
fn clean_apertures(bytes: &[u8]) -> Vec<Option<(u32, u32)>> {
    let f = HeifFile::parse(bytes).unwrap();
    let mv = parse_movie(&f).unwrap().expect("moov");
    mv.tracks
        .iter()
        .map(|t| {
            let e = t.primary_entry().unwrap();
            e.clap.as_ref().map(|c| {
                let r = c.resolve(e.width as u32, e.height as u32).unwrap();
                (r.width, r.height)
            })
        })
        .collect()
}

/// §8.3.2: the scale a reader applies is `tkhd` / clean aperture with a
/// `clap`, `tkhd` / sample entry without — 1 on both axes here.
fn assert_unit_scale(bytes: &[u8], visible: (u32, u32), what: &str) {
    let sizes = track_sizes(bytes);
    let claps = clean_apertures(bytes);
    assert!(!sizes.is_empty());
    for (((tw, th), (ew, eh)), clap) in sizes.into_iter().zip(claps) {
        assert_eq!((tw, th), visible, "{what}: tkhd = presentation size");
        let reference = clap.unwrap_or((ew as u32, eh as u32));
        assert_eq!(
            reference, visible,
            "{what}: the size tkhd is measured against (entry {ew}x{eh}, clap {clap:?})"
        );
    }
    let back = decode_all(bytes).unwrap();
    assert!(
        back.iter()
            .all(|f| (f.image.width, f.image.height) == visible),
        "{what}: decoded size"
    );
}

#[test]
fn odd_sized_hevc_sequence_presents_at_its_visible_size() {
    let opts = EncodeOptions::default().with_hevc_mode("pcm".into());
    let bytes = sequence(33, 17, &opts);
    let sizes = track_sizes(&bytes);
    assert_eq!(sizes.len(), 1);
    // A 4:2:0 picture cannot be cropped to an odd size by the codec's
    // conformance window (it counts chroma samples): the entry is the
    // even rounding, the clap removes the last column / row.
    assert_eq!(sizes[0], ((33, 17), (34, 18)));
    assert_eq!(clean_apertures(&bytes), vec![Some((33, 17))]);
    assert_eq!(raw_tkhd_fixed(&bytes), (33 << 16, 17 << 16));
    assert_unit_scale(&bytes, (33, 17), "hevc 33x17");
}

/// A size that is not a multiple of the codec's block size but lies on
/// the chroma grid: the codec crops, the sample entry says the visible
/// size, no `clap`, and nothing is left for a reader to rescale. (The
/// entry used to carry the block-padded size with a `clap`; a reader
/// that normalises to `tkhd` / sample entry without consulting the
/// `clap` then presented a slightly squeezed picture.)
#[test]
fn unaligned_even_hevc_sequence_needs_no_clap() {
    for mode in ["pcm", "intra"] {
        let opts = EncodeOptions::default().with_hevc_mode(mode.into());
        for (w, h) in [(60, 44), (18, 34), (130, 70)] {
            let bytes = sequence(w, h, &opts);
            assert_eq!(
                track_sizes(&bytes),
                vec![((w, h), (w as u16, h as u16))],
                "{mode} {w}x{h}"
            );
            assert_eq!(clean_apertures(&bytes), vec![None], "{mode} {w}x{h}");
            assert_eq!(raw_tkhd_fixed(&bytes), (w << 16, h << 16));
            assert_unit_scale(&bytes, (w, h), &format!("hevc {mode} {w}x{h}"));
            // Lossless coding returns the pictures sample for sample.
            if mode == "pcm" {
                let back = decode_all(&bytes).unwrap();
                let n = back.len();
                for (i, f) in back[n - 2..].iter().enumerate() {
                    assert_eq!(
                        f.image.planes,
                        picture(w, h, i as u8).planes,
                        "{w}x{h} frame {i}"
                    );
                }
            }
        }
    }
}

#[test]
fn odd_sized_av1_sequence_presents_at_its_visible_size() {
    // AV1 codes any extent: the entry is the picture, odd or not.
    let opts = EncodeOptions::default().with_codec(StillCodec::Av1);
    for (w, h) in [(15, 9), (60, 44)] {
        let bytes = sequence(w, h, &opts);
        assert_eq!(track_sizes(&bytes), vec![((w, h), (w as u16, h as u16))]);
        assert_eq!(clean_apertures(&bytes), vec![None]);
        assert_eq!(raw_tkhd_fixed(&bytes), (w << 16, h << 16));
        assert_unit_scale(&bytes, (w, h), &format!("av1 {w}x{h}"));
    }
}

/// AVC: the encoder writes no frame cropping, so the macroblock
/// padding stays in the coded picture and the `clap` carries the
/// visible size; VVC evens the size and leaves an odd edge to `clap`.
#[test]
fn avc_and_vvc_sequences_keep_a_unit_scale() {
    let avc = EncodeOptions::default().with_codec(StillCodec::Avc);
    let bytes = sequence(60, 44, &avc);
    assert_eq!(track_sizes(&bytes), vec![((60, 44), (64, 48))]);
    assert_eq!(clean_apertures(&bytes), vec![Some((60, 44))]);
    assert_unit_scale(&bytes, (60, 44), "avc 60x44");
    let vvc = EncodeOptions::default().with_codec(StillCodec::Vvc);
    let bytes = sequence(60, 44, &vvc);
    assert_eq!(track_sizes(&bytes), vec![((60, 44), (60, 44))]);
    assert_eq!(clean_apertures(&bytes), vec![None]);
    assert_unit_scale(&bytes, (60, 44), "vvc 60x44");
    let bytes = sequence(61, 45, &vvc);
    assert_eq!(track_sizes(&bytes), vec![((61, 45), (62, 46))]);
    assert_unit_scale(&bytes, (61, 45), "vvc 61x45");
}

/// An alpha track follows the same rule as its master.
#[test]
fn alpha_track_extents_follow_the_master() {
    let frames: Vec<Frame> = (0..2u8)
        .map(|i| {
            let p = picture(60, 44, i);
            let mut planes = p.planes.clone();
            planes.push(Plane::new(
                60,
                (0..60 * 44).map(|j| (j as u8).wrapping_mul(3)).collect(),
            ));
            Frame::new(
                HeifImage::new(60, 44, PixelFormat::Yuva420P, planes).unwrap(),
                Some(Duration::from_millis(40)),
                None,
                None,
            )
        })
        .collect();
    let opts = EncodeOptions::default().with_hevc_mode("pcm".into());
    let bytes = encode_all(&frames, &opts).unwrap();
    assert_eq!(
        track_sizes(&bytes),
        vec![((60, 44), (60, 44)), ((60, 44), (60, 44))]
    );
    assert_eq!(clean_apertures(&bytes), vec![None, None]);
    assert_unit_scale(&bytes, (60, 44), "hevc + alpha 60x44");
}

#[test]
fn aligned_sequence_tkhd_equals_the_coded_size() {
    let opts = EncodeOptions::default().with_hevc_mode("pcm".into());
    let bytes = sequence(32, 16, &opts);
    assert_eq!(track_sizes(&bytes), vec![((32, 16), (32, 16))]);
    assert_eq!(clean_apertures(&bytes), vec![None]);
}

/// A black-box reader presents every stream of an unaligned sequence
/// at the intended size with square pixels (it reported the padded
/// size and a non-square sample aspect ratio for the padded entries).
#[test]
fn black_box_reader_presents_the_intended_size() {
    if !common::have_binary("ffprobe") {
        eprintln!("SKIP: ffprobe is absent");
        return;
    }
    let dir = common::scratch_dir("tkhd");
    for (name, opts, (w, h)) in [
        (
            "hevc",
            EncodeOptions::default().with_hevc_mode("intra".into()),
            (60u32, 44u32),
        ),
        (
            "av1",
            EncodeOptions::default().with_codec(StillCodec::Av1),
            (61, 45),
        ),
    ] {
        let path = dir.join(format!("{name}-{w}x{h}.heic"));
        std::fs::write(&path, sequence(w, h, &opts)).unwrap();
        let out = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v",
                "-show_entries",
                "stream=width,height,sample_aspect_ratio",
                "-of",
                "csv=p=0",
            ])
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let rows: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(!rows.is_empty(), "{name}: no stream reported");
        for row in rows {
            let mut f = row.split(',');
            let (rw, rh) = (f.next().unwrap(), f.next().unwrap());
            assert_eq!(
                (rw, rh),
                (w.to_string().as_str(), h.to_string().as_str()),
                "{name}: {row}"
            );
            let sar = f.next().unwrap_or("N/A").trim();
            assert!(
                matches!(sar, "N/A" | "1:1" | "0:1" | ""),
                "{name}: sample aspect ratio {sar}"
            );
        }
    }
}
