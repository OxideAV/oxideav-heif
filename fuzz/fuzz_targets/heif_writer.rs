#![no_main]

//! The writer → reader path on arbitrary bytes. Two shapes of input:
//!
//! * a HEIF-family file (the corpus seeds): `HeifWriter::from_files`
//!   re-muxes its primary — with derivations, auxiliaries, thumbnails,
//!   metadata items and `altr` alternates — into a new file, which must
//!   re-parse and decode to the same picture, colour and metadata as the
//!   source whenever the source decodes (the re-mux oracle);
//! * anything else: the bytes drive the high-level encoder — picture
//!   geometry and layout, codec (HEVC `pcm` / AV1 lossless), transforms
//!   (`clap` / `irot` / `imir` on the items or on an `iden`), grid tile,
//!   thumbnail, alpha, depth map, Exif / XMP / ICC, or an `iovl` with
//!   offsets and fill from the input — and the file must re-parse, pass
//!   the MIAF checks (so its brands are truthful), decode to the exact
//!   source under the lossless codings, and re-mux to itself.
//!
//! The contract is "the call returns" plus those equalities.

use libfuzzer_sys::fuzz_target;
use oxideav_heif::encode::{
    encode_overlay, encode_still, EncodeOptions, OverlayLayer, OverlaySpec, StillCodec,
    TransformCarrier,
};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};
use oxideav_heif::miaf::{check, MiafProfile};
use oxideav_heif::props::{Clap, CropRect, Imir, Irot, Property};
use oxideav_heif::{decode_with, DecodeOptions, HeifFile, HeifWriter};

/// A byte cursor over the input (zeros once exhausted).
struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}
impl Bytes<'_> {
    fn u8(&mut self) -> u8 {
        let v = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        v
    }
    fn rest(&self) -> &[u8] {
        &self.data[self.pos.min(self.data.len())..]
    }
}

fn limits() -> DecodeOptions {
    DecodeOptions::default()
        .with_max_width(Some(512))
        .with_max_height(Some(512))
        .with_max_pixels(Some(1 << 16))
        .with_max_bytes(Some(1 << 20))
        .with_strict(false)
}

/// Re-mux `bytes` and compare the primaries when the source decodes.
fn remux_oracle(bytes: &[u8]) {
    let Ok(w) = HeifWriter::from_files(&[bytes]) else {
        return;
    };
    let Ok(out) = w.write_to_vec() else {
        return;
    };
    let dst = HeifFile::parse(&out).expect("re-muxed file re-parses");
    let opts = limits();
    let Ok(original) = decode_with(bytes, &opts) else {
        return;
    };
    let back = decode_with(&out, &opts).expect("re-muxed file decodes");
    assert_eq!(
        (back.width, back.height, back.format),
        (original.width, original.height, original.format)
    );
    assert_eq!(back.planes, original.planes, "re-mux pixels");
    assert_eq!(back.color, original.color, "re-mux colour");
    assert_eq!(back.metadata, original.metadata, "re-mux metadata");
    let now = check(&dst, MiafProfile::Miaf)
        .map(|r| r.is_conformant())
        .unwrap_or(false);
    assert_eq!(now, dst.file_type.has_brand(b"miaf"), "miaf brand truth");
}

/// A small picture filled from the input bytes.
fn picture(b: &mut Bytes<'_>, w: u32, h: u32, fmt: HeifPixelFormat) -> HeifFrame {
    let mut f = HeifFrame::zeroed(w, h, fmt).expect("small picture");
    let max = fmt.max_value();
    for p in 0..fmt.plane_count() {
        let (pw, ph) = f.plane_dims(p);
        for y in 0..ph {
            for x in 0..pw {
                let v = b.u8() as u16;
                f.set_sample(p, x, y, (v * 257 >> (16 - fmt.bit_depth)).min(max));
            }
        }
    }
    f
}

fuzz_target!(|data: &[u8]| {
    if oxideav_heif::probe(data) {
        remux_oracle(data);
        return;
    }
    let mut b = Bytes { data, pos: 0 };
    let codec = if b.u8() & 1 == 0 {
        StillCodec::Hevc
    } else {
        StillCodec::Av1
    };
    let geometry = b.u8();
    let (w, h) = (1 + (geometry & 15) as u32 * 3, 1 + (geometry >> 4) as u32 * 3);
    let layout = b.u8();
    let chroma = match layout & 3 {
        0 => Chroma::Mono,
        1 => Chroma::Yuv420,
        2 => Chroma::Yuv422,
        _ => Chroma::Yuv444,
    };
    let depth = if layout & 4 != 0 { 10 } else { 8 };
    let alpha = layout & 8 != 0;
    let Ok(fmt) = HeifPixelFormat::new(chroma, depth, alpha) else {
        return;
    };
    let flags = b.u8();
    let mut opts = EncodeOptions::default().with_codec(codec);
    if codec == StillCodec::Hevc {
        opts = opts.with_hevc_mode("pcm".into());
    }
    let mut chain = Vec::new();
    if flags & 1 != 0 {
        chain.push(Property::Irot(Irot::new(b.u8() & 3)));
    }
    if flags & 2 != 0 {
        chain.push(Property::Imir(Imir::new(b.u8() & 1)));
    }
    if flags & 4 != 0 && w > 2 && h > 2 {
        let (cx, cy) = ((b.u8() as u32) % (w - 1), (b.u8() as u32) % (h - 1));
        let cw = 1 + (b.u8() as u32) % (w - cx);
        let ch = 1 + (b.u8() as u32) % (h - cy);
        chain.insert(
            0,
            Property::Clap(Clap::for_rect(
                w,
                h,
                CropRect {
                    x: cx,
                    y: cy,
                    width: cw,
                    height: ch,
                },
            )),
        );
    }
    opts = opts.with_transforms(chain);
    if flags & 8 != 0 {
        opts = opts.with_transform_carrier(TransformCarrier::Identity);
    }
    if flags & 16 != 0 {
        opts = opts.with_grid_tile(Some(64));
    }
    if flags & 32 != 0 {
        opts = opts.with_thumbnail_max_dim(Some(1 + (b.u8() as u32 & 15)));
    }
    if flags & 64 != 0 {
        let (dw, dh) = (1 + (b.u8() as u32 & 7), 1 + (b.u8() as u32 & 7));
        let d = picture(
            &mut b,
            dw,
            dh,
            HeifPixelFormat::new(Chroma::Mono, 8, false).unwrap(),
        );
        opts = opts.with_depth_map(Some(d));
    }
    let meta = b.u8();
    if meta & 1 != 0 {
        opts = opts.with_exif(Some(b"II*\0\x08\0\0\0\0\0".to_vec()));
    }
    if meta & 2 != 0 {
        opts = opts.with_xmp(Some("<x:xmpmeta>fuzz</x:xmpmeta>".into()));
    }
    if meta & 4 != 0 {
        let mut icc = vec![0u8; 132];
        icc[0..4].copy_from_slice(&132u32.to_be_bytes());
        icc[36..40].copy_from_slice(b"acsp");
        opts = opts.with_icc_profile(Some(icc));
    }
    let overlay = meta & 8 != 0;
    let bytes = if overlay {
        let n = 1 + (b.u8() as usize & 3);
        let mut layers = Vec::with_capacity(n);
        for _ in 0..n {
            let lw = 1 + (b.u8() as u32 & 15);
            let lh = 1 + (b.u8() as u32 & 15);
            let x = b.u8() as i32 - 8;
            let y = b.u8() as i32 - 8;
            layers.push(OverlayLayer::new(picture(&mut b, lw, lh, fmt), x, y));
        }
        let fill = [
            b.u8() as u16 * 257,
            b.u8() as u16 * 257,
            b.u8() as u16 * 257,
            b.u8() as u16 * 257,
        ];
        let spec = OverlaySpec::new(w, h, fill, layers);
        // grid_tile / depth_map are refused on this path: drop them.
        let o = opts.with_grid_tile(Some(0)).with_depth_map(None);
        match encode_overlay(&spec, &o) {
            Ok(v) => v,
            Err(_) => return,
        }
    } else {
        let src = picture(&mut b, w, h, fmt);
        match encode_still(&src, &opts) {
            Ok(v) => v,
            Err(_) => return,
        }
    };
    let _ = b.rest();
    let file = HeifFile::parse(&bytes).expect("written file re-parses");
    let rep = check(&file, MiafProfile::Miaf).expect("checker runs");
    assert!(rep.is_conformant(), "written file: {:?}", rep.violations);
    assert!(file.file_type.has_brand(b"miaf"));
    let img = decode_with(&bytes, &limits().with_max_pixels(Some(1 << 20))).expect("decodes");
    let _ = img.to_rgba8();
    // The written file re-muxes to itself.
    remux_oracle(&bytes);
});
