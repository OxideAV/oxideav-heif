//! Minimal PNG writer for test sources: 8- or 16-bit grey / RGB / RGBA,
//! stored (uncompressed) deflate blocks — no compression dependency.

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *t = c;
    }
    let mut crc = 0xffff_ffffu32;
    for b in data {
        crc = table[((crc ^ *b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let mut c = tag.to_vec();
    c.extend_from_slice(body);
    out.extend_from_slice(&c);
    out.extend_from_slice(&crc32(&c).to_be_bytes());
}

/// Encode `samples` (row-major, `channels` per pixel, big-endian u16
/// for `bit_depth` 16, one byte per sample for 8) as a PNG.
pub fn write_png(
    width: u32,
    height: u32,
    channels: usize,
    bit_depth: u8,
    samples: &[u16],
) -> Vec<u8> {
    assert!((1..=4).contains(&channels));
    let colour_type = match channels {
        1 => 0u8,
        2 => 4,
        3 => 2,
        _ => 6,
    };
    let bps = if bit_depth == 16 { 2 } else { 1 };
    let stride = width as usize * channels * bps;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0); // filter none
        for x in 0..width as usize * channels {
            let v = samples[y * width as usize * channels + x];
            if bps == 2 {
                raw.extend_from_slice(&v.to_be_bytes());
            } else {
                raw.push(v as u8);
            }
        }
    }
    // zlib: header, stored blocks of ≤ 65535, adler32.
    let mut z = vec![0x78, 0x01];
    let mut pos = 0;
    while pos < raw.len() || raw.is_empty() {
        let n = (raw.len() - pos).min(65535);
        let last = pos + n >= raw.len();
        z.push(last as u8);
        z.extend_from_slice(&(n as u16).to_le_bytes());
        z.extend_from_slice(&(!(n as u16)).to_le_bytes());
        z.extend_from_slice(&raw[pos..pos + n]);
        pos += n;
        if raw.is_empty() {
            break;
        }
    }
    let (mut a, mut b) = (1u32, 0u32);
    for c in &raw {
        a = (a + *c as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[bit_depth, colour_type, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}
