//! The `tkhd` width / height of a written image sequence is the track's
//! visual presentation size (ISO/IEC 14496-12 §8.3.2.3), not the coded
//! size of its samples: an odd-sized sequence whose pictures are padded
//! to the codec's block alignment presents at the visible size the
//! sample entry's `clap` restores, while the sample entry keeps the
//! coded size.
#![cfg(feature = "registry")]

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

#[test]
fn odd_sized_hevc_sequence_presents_at_its_visible_size() {
    let opts = EncodeOptions::default().with_hevc_mode("pcm".into());
    let bytes = sequence(33, 17, &opts);
    let sizes = track_sizes(&bytes);
    assert_eq!(sizes.len(), 1);
    let ((tw, th), (ew, eh)) = sizes[0];
    assert_eq!((tw, th), (33, 17), "tkhd = presentation size");
    assert!(
        (ew as u32, eh as u32) != (33, 17) && ew >= 33 && eh >= 17,
        "the sample entry keeps the padded coded size ({ew}x{eh})"
    );
    assert_eq!(raw_tkhd_fixed(&bytes), (33 << 16, 17 << 16));
    // The pictures still read back at the visible size.
    let back = decode_all(&bytes).unwrap();
    assert!(
        back.iter()
            .all(|f| (f.image.width, f.image.height) == (33, 17))
    );
}

#[test]
fn odd_sized_av1_sequence_presents_at_its_visible_size() {
    let opts = EncodeOptions::default().with_codec(StillCodec::Av1);
    let bytes = sequence(15, 9, &opts);
    for ((tw, th), (ew, eh)) in track_sizes(&bytes) {
        assert_eq!(
            (tw, th),
            (15, 9),
            "tkhd = presentation size (entry {ew}x{eh})"
        );
    }
    assert_eq!(raw_tkhd_fixed(&bytes), (15 << 16, 9 << 16));
}

#[test]
fn aligned_sequence_tkhd_equals_the_coded_size() {
    let opts = EncodeOptions::default().with_hevc_mode("pcm".into());
    let bytes = sequence(32, 16, &opts);
    assert_eq!(track_sizes(&bytes), vec![((32, 16), (32, 16))]);
}
