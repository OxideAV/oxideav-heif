//! `heifenc` — write a HEIF / AVIF still with this crate's encoder from
//! a synthetic test picture, for feeding third-party readers.
//!
//! ```text
//! heifenc out.heic [--size WxH] [--codec hevc|av1] [--mode pcm|intra]
//!         [--qp N] [--grid TILE] [--thumb MAXDIM] [--alpha] [--gray]
//!         [--irot ANGLE] [--imir AXIS] [--clap] [--exif] [--xmp] [--icc]
//!         [--framework] [--png reference.png]
//! ```
//!
//! `--png` also writes the source picture (after the encoder's own
//! 8-bit 4:2:0 conversion, with the requested transforms applied) as a
//! PNG so a reader's rendering can be compared against what was
//! encoded. `--framework` goes through the registry `"heif"` encoder.

use oxideav_heif::encode::{encode_still, EncodeOptions, StillCodec};
use oxideav_heif::image::{Chroma, HeifFrame, HeifPixelFormat};
use oxideav_heif::props::{Clap, CropRect, Imir, Irot, Property};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: heifenc out.heic [options]");
        std::process::exit(2);
    }
    let out = args[0].clone();
    let mut w = 96u32;
    let mut h = 80u32;
    let mut opts = EncodeOptions::default();
    let mut alpha = false;
    let mut gray = false;
    let mut framework = false;
    let mut png = None;
    let mut clap = false;
    let mut i = 1;
    let next = |i: &mut usize| -> String {
        *i += 1;
        args.get(*i).cloned().unwrap_or_default()
    };
    while i < args.len() {
        match args[i].as_str() {
            "--size" => {
                let s = next(&mut i);
                let (a, b) = s.split_once('x').expect("WxH");
                w = a.parse().unwrap();
                h = b.parse().unwrap();
            }
            "--codec" => {
                opts.codec = match next(&mut i).as_str() {
                    "av1" => StillCodec::Av1,
                    _ => StillCodec::Hevc,
                }
            }
            "--mode" => opts.hevc_mode = next(&mut i),
            "--qp" => opts.qp = next(&mut i).parse().unwrap(),
            "--grid" => opts.grid_tile = Some(next(&mut i).parse().unwrap()),
            "--thumb" => opts.thumbnail_max_dim = Some(next(&mut i).parse().unwrap()),
            "--alpha" => alpha = true,
            "--gray" => gray = true,
            "--irot" => opts.transforms.push(Property::Irot(Irot {
                angle: next(&mut i).parse().unwrap(),
            })),
            "--imir" => opts.transforms.push(Property::Imir(Imir {
                axis: next(&mut i).parse().unwrap(),
            })),
            "--clap" => clap = true,
            "--exif" => {
                // Minimal little-endian TIFF header + one IFD with an
                // ImageDescription tag.
                let mut t = b"II*\0\x08\0\0\0".to_vec();
                t.extend_from_slice(&1u16.to_le_bytes());
                t.extend_from_slice(&0x010eu16.to_le_bytes());
                t.extend_from_slice(&2u16.to_le_bytes());
                t.extend_from_slice(&8u32.to_le_bytes());
                t.extend_from_slice(&26u32.to_le_bytes());
                t.extend_from_slice(&0u32.to_le_bytes());
                t.extend_from_slice(b"OxideAV\0");
                opts.exif = Some(t);
            }
            "--xmp" => {
                opts.xmp = Some(
                    "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?><x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>OxideAV heif</dc:title></rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>".into(),
                );
            }
            "--icc" => opts.icc_profile = Some(tiny_icc()),
            "--framework" => framework = true,
            "--png" => png = Some(next(&mut i)),
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if clap {
        // Keep the centre 3/4 of the picture (integer aperture).
        let cw = (w * 3 / 4).max(1);
        let ch = (h * 3 / 4).max(1);
        opts.transforms.insert(
            0,
            Property::Clap(Clap::for_rect(
                w,
                h,
                CropRect {
                    x: (w - cw) / 2,
                    y: (h - ch) / 2,
                    width: cw,
                    height: ch,
                },
            )),
        );
    }
    let src = picture(w, h, gray, alpha);
    let bytes = if framework {
        encode_via_framework(&src, &opts)
    } else {
        encode_still(&src, &opts).expect("encode")
    };
    std::fs::write(&out, &bytes).expect("write");
    println!("OK {} bytes", bytes.len());
    if let Some(p) = png {
        // What a reader must show: the 8-bit 4:2:0 conversion the
        // encoder performs, then the transform chain.
        let conv = oxideav_heif::encode::to_yuv420_8(&src.without_alpha()).unwrap();
        let conv = match src.alpha_as_frame() {
            Some(a) => conv
                .with_alpha_plane(
                    &oxideav_heif::encode::to_yuv420_8(&a)
                        .unwrap()
                        .without_alpha()
                        .alpha_free_luma(),
                )
                .unwrap(),
            None => conv,
        };
        let entries: Vec<oxideav_heif::props::PropertyEntry> = opts
            .transforms
            .iter()
            .map(|t| oxideav_heif::props::PropertyEntry {
                property: t.clone(),
                essential: true,
                index: 0,
            })
            .collect();
        let shown = oxideav_heif::compose::apply_transforms(&conv, entries.iter(), false).unwrap();
        let rgb = oxideav_heif::rgb::to_rgb(&shown, Some(&opts.colr)).unwrap();
        std::fs::write(p, png_bytes(&rgb)).expect("png");
    }
}

/// Helper trait so the alpha plane of a 4:2:0-converted alpha picture
/// comes back as a mono frame.
trait AlphaFreeLuma {
    fn alpha_free_luma(&self) -> HeifFrame;
}

impl AlphaFreeLuma for HeifFrame {
    fn alpha_free_luma(&self) -> HeifFrame {
        HeifFrame {
            width: self.width,
            height: self.height,
            format: HeifPixelFormat::new(Chroma::Mono, self.format.bit_depth, false).unwrap(),
            planes: vec![self.planes[0].clone()],
        }
    }
}

fn encode_via_framework(src: &HeifFrame, opts: &EncodeOptions) -> Vec<u8> {
    use oxideav_core::{CodecId, CodecOptions, CodecParameters, Frame, RuntimeContext};
    let mut ctx = RuntimeContext::new();
    oxideav_h265::register(&mut ctx);
    oxideav_av1::register(&mut ctx);
    oxideav_heif::register(&mut ctx);
    let mut params = CodecParameters::video(CodecId::new("heif"));
    let (vf, pf) = src.to_core().unwrap();
    params.width = Some(src.width);
    params.height = Some(src.height);
    params.pixel_format = Some(pf);
    let mut o = CodecOptions::new()
        .set(
            "codec",
            match opts.codec {
                StillCodec::Hevc => "hevc",
                StillCodec::Av1 => "av1",
            },
        )
        .set("mode", opts.hevc_mode.as_str())
        .set("qp", opts.qp.to_string());
    if let Some(g) = opts.grid_tile {
        o = o.set("grid", g.to_string());
    }
    if let Some(t) = opts.thumbnail_max_dim {
        o = o.set("thumbnail", t.to_string());
    }
    params.options = o;
    let mut enc = ctx.codecs.first_encoder(&params).expect("heif encoder");
    enc.send_frame(&Frame::Video(vf)).unwrap();
    enc.flush().unwrap();
    enc.receive_packet().unwrap().data
}

/// A deterministic picture: smooth gradients, a hard-edged box, a
/// diagonal and a coloured disc, in 4:4:4 8-bit (so the encoder's own
/// 4:2:0 conversion is exercised), optional alpha ramp.
fn picture(w: u32, h: u32, gray: bool, alpha: bool) -> HeifFrame {
    let chroma = if gray { Chroma::Mono } else { Chroma::Yuv444 };
    let mut f = HeifFrame::zeroed(w, h, HeifPixelFormat::new(chroma, 8, alpha).unwrap()).unwrap();
    let (cx, cy, r) = (w as f64 * 0.7, h as f64 * 0.65, (w.min(h) as f64) * 0.18);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f64 / w.max(2) as f64;
            let fy = y as f64 / h.max(2) as f64;
            let mut rgb = [
                (fx * 255.0) as u16,
                (fy * 255.0) as u16,
                (((fx + fy) * 0.5 * 255.0) as u16).min(255),
            ];
            if x >= w / 4 && x < w / 2 && y >= h / 4 && y < h / 2 {
                rgb = [240, 240, 240];
            }
            if (x as i64 - y as i64 * w as i64 / h.max(1) as i64).abs() < 2 {
                rgb = [10, 10, 10];
            }
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            if dx * dx + dy * dy < r * r {
                rgb = [230, 30, 30];
            }
            let [rr, gg, bb] = [rgb[0] as f64, rgb[1] as f64, rgb[2] as f64];
            let yv = 0.299 * rr + 0.587 * gg + 0.114 * bb;
            if gray {
                f.set_sample(0, x, y, yv.round() as u16);
            } else {
                let cb = (bb - yv) / 1.772 + 128.0;
                let cr = (rr - yv) / 1.402 + 128.0;
                f.set_sample(0, x, y, yv.round().clamp(0.0, 255.0) as u16);
                f.set_sample(1, x, y, cb.round().clamp(0.0, 255.0) as u16);
                f.set_sample(2, x, y, cr.round().clamp(0.0, 255.0) as u16);
            }
            if alpha {
                let a = if x < w / 3 {
                    255
                } else {
                    ((fy * 255.0) as u16).min(255)
                };
                let ap = f.format.alpha_plane().unwrap();
                f.set_sample(ap, x, y, a);
            }
        }
    }
    f
}

/// A minimal but well-formed ICC v2 profile (sRGB-like tags: wtpt,
/// rXYZ/gXYZ/bXYZ, one shared curv), 8-bit little payloads.
fn tiny_icc() -> Vec<u8> {
    fn tag(sig: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = sig.to_vec();
        v.extend_from_slice(body);
        v
    }
    fn xyz(x: f64, y: f64, z: f64) -> Vec<u8> {
        let mut b = b"XYZ \0\0\0\0".to_vec();
        for v in [x, y, z] {
            b.extend_from_slice(&((v * 65536.0).round() as i32).to_be_bytes());
        }
        b
    }
    let curv = b"curv\0\0\0\0\0\0\0\x01\x02\x33\0\0".to_vec();
    let elements: Vec<([u8; 4], Vec<u8>)> = vec![
        (
            *b"desc",
            tag(
                b"desc",
                &[
                    0, 0, 0, 0, 0, 0, 0, 8, b'O', b'x', b'i', b'd', b'e', b'A', b'V', 0,
                ],
            ),
        ),
        (*b"wtpt", xyz(0.9505, 1.0, 1.089)),
        (*b"rXYZ", xyz(0.4361, 0.2225, 0.0139)),
        (*b"gXYZ", xyz(0.3851, 0.7169, 0.0971)),
        (*b"bXYZ", xyz(0.1431, 0.0606, 0.7141)),
        (*b"rTRC", curv.clone()),
        (*b"gTRC", curv.clone()),
        (*b"bTRC", curv),
    ];
    let mut header = vec![0u8; 128];
    header[4..8].copy_from_slice(b"none");
    header[8..12].copy_from_slice(&0x0210_0000u32.to_be_bytes());
    header[12..16].copy_from_slice(b"mntr");
    header[16..20].copy_from_slice(b"RGB ");
    header[20..24].copy_from_slice(b"XYZ ");
    header[36..40].copy_from_slice(b"acsp");
    header[68..80].copy_from_slice(&xyz(0.9642, 1.0, 0.8249)[8..20]);
    let mut table = (elements.len() as u32).to_be_bytes().to_vec();
    let mut data = Vec::new();
    let base = 128 + 4 + elements.len() * 12;
    for (sig, body) in &elements {
        while (base + data.len()) % 4 != 0 {
            data.push(0);
        }
        table.extend_from_slice(sig);
        table.extend_from_slice(&((base + data.len()) as u32).to_be_bytes());
        table.extend_from_slice(&(body.len() as u32).to_be_bytes());
        data.extend_from_slice(body);
    }
    let mut out = header;
    out.extend_from_slice(&table);
    out.extend_from_slice(&data);
    let len = out.len() as u32;
    out[0..4].copy_from_slice(&len.to_be_bytes());
    out
}

// --- minimal PNG writer (stored deflate blocks) ---------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    c ^ 0xFFFF_FFFF
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn png_bytes(img: &oxideav_heif::rgb::RgbImage) -> Vec<u8> {
    let colour_type = if img.channels == 4 { 6 } else { 2 };
    let mut raw = Vec::new();
    let row = img.width as usize * img.channels;
    for y in 0..img.height as usize {
        raw.push(0);
        raw.extend(img.data[y * row..(y + 1) * row].iter().map(|v| *v as u8));
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65535).peekable();
    while let Some(c) = chunks.next() {
        z.push(chunks.peek().is_none() as u8);
        let len = c.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut push = |kind: &[u8; 4], body: &[u8]| {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(body);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&img.width.to_be_bytes());
    ihdr.extend_from_slice(&img.height.to_be_bytes());
    ihdr.extend_from_slice(&[8, colour_type, 0, 0, 0]);
    push(b"IHDR", &ihdr);
    push(b"IDAT", &z);
    push(b"IEND", &[]);
    out
}
