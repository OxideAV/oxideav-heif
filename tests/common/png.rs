//! Minimal PNG reader for the corpus `expected*.png` oracles
//! (8/16-bit grey / grey+alpha / RGB / RGBA, non-interlaced, all five
//! scanline filters). Test-only; inflate comes from `compcol`.
#![allow(dead_code)]

/// A decoded PNG: samples unpacked to `u16`, `channels` per pixel.
pub struct Png {
    pub width: u32,
    pub height: u32,
    pub channels: usize,
    pub bit_depth: u8,
    pub samples: Vec<u16>,
}

impl Png {
    pub fn sample(&self, x: u32, y: u32, c: usize) -> u16 {
        self.samples[(y as usize * self.width as usize + x as usize) * self.channels + c]
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

pub fn read_png(bytes: &[u8]) -> Png {
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
    let mut pos = 8;
    let mut width = 0;
    let mut height = 0;
    let mut bit_depth = 0u8;
    let mut colour_type = 0u8;
    let mut idat = Vec::new();
    while pos + 8 <= bytes.len() {
        let len = be32(&bytes[pos..]) as usize;
        let kind = &bytes[pos + 4..pos + 8];
        let data = &bytes[pos + 8..pos + 8 + len];
        match kind {
            b"IHDR" => {
                width = be32(data);
                height = be32(&data[4..]);
                bit_depth = data[8];
                colour_type = data[9];
                assert_eq!(
                    data[12], 0,
                    "interlaced PNG not supported by the test reader"
                );
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        pos += 12 + len;
    }
    let channels = match colour_type {
        0 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        t => panic!("unsupported PNG colour type {t}"),
    };
    assert!(bit_depth == 8 || bit_depth == 16, "bit depth {bit_depth}");
    let bps = (bit_depth / 8) as usize;
    let bpp = channels * bps;
    let stride = width as usize * bpp;
    let raw = compcol::vec::decompress_to_vec_capped::<compcol::zlib::Zlib>(
        &idat,
        ((stride + 1) * height as usize) as u64,
    )
    .expect("inflate");
    assert_eq!(raw.len(), (stride + 1) * height as usize, "inflated length");
    let mut prev = vec![0u8; stride];
    let mut cur = vec![0u8; stride];
    let mut samples = Vec::with_capacity(width as usize * height as usize * channels);
    for y in 0..height as usize {
        let line = &raw[y * (stride + 1)..(y + 1) * (stride + 1)];
        let filter = line[0];
        let src = &line[1..];
        for i in 0..stride {
            let a = if i >= bpp { cur[i - bpp] } else { 0 };
            let b = prev[i];
            let c = if i >= bpp { prev[i - bpp] } else { 0 };
            let x = src[i];
            cur[i] = match filter {
                0 => x,
                1 => x.wrapping_add(a),
                2 => x.wrapping_add(b),
                3 => x.wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => {
                    let p = a as i16 + b as i16 - c as i16;
                    let pa = (p - a as i16).abs();
                    let pb = (p - b as i16).abs();
                    let pc = (p - c as i16).abs();
                    let pred = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    x.wrapping_add(pred)
                }
                f => panic!("bad PNG filter {f}"),
            };
        }
        for px in cur.chunks_exact(bps) {
            samples.push(if bps == 1 {
                px[0] as u16
            } else {
                u16::from_be_bytes([px[0], px[1]])
            });
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    Png {
        width,
        height,
        channels,
        bit_depth,
        samples,
    }
}
