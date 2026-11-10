//! Common Encryption of image items (ISO/IEC 23001-7:2016 with
//! Amd 1:2019 §8.3 / §8.4): the `ItemEncryptionBox` property (`ienc`),
//! the `ItemAuxiliaryInformationBox` property (`iaux`), the
//! `CencSampleAuxiliaryDataFormat` carried by an `auxi` item the
//! protected item names through `auxr`, and the decryption of an item's
//! data under the `cenc` / `cbc1` / `cens` / `cbcs` schemes (§9, §10):
//! subsample ranges, protection patterns, per-sample and constant IVs,
//! several keys per item (Amd 1 `aux_info_type_parameter` 1).
//!
//! The block cipher itself (AES-128, §9.3 / §9.4.3) is the caller's: the
//! crate never holds key material. A [`BlockCipher`] maps a KID to the
//! forward and inverse cipher over whole 16-byte blocks; this module
//! builds the counter blocks (CTR) and the chaining (CBC) around it.
//!
//! ```text
//! aligned(8) class ItemEncryptionBox extends ItemFullProperty('ienc', version, flags = 0) {
//!   unsigned int(8) reserved = 0;
//!   if (version == 0) unsigned int(8) reserved = 0;
//!   else { unsigned int(4) crypt_byte_block; unsigned int(4) skip_byte_block; }
//!   unsigned int(8) num_keys;
//!   for (i = 1; i <= num_keys; i++) {
//!     unsigned int(8) Per_Sample_IV_Size; unsigned int(8)[16] KID;
//!     if (Per_Sample_IV_Size == 0) {
//!       unsigned int(8) constant_IV_size; unsigned int(8)[constant_IV_size] constant_IV;
//!     }
//!   }
//! }
//! ```

use crate::boxes::{FourCc, Reader, parse_full_box};
use crate::error::{HeifError, Result};

/// The four Common Encryption schemes (§10).
pub const SCHEME_CENC: FourCc = *b"cenc";
/// `cbc1`: AES-CBC, chained per sample (§10.2).
pub const SCHEME_CBC1: FourCc = *b"cbc1";
/// `cens`: AES-CTR with a protection pattern (§10.3).
pub const SCHEME_CENS: FourCc = *b"cens";
/// `cbcs`: AES-CBC with a pattern, chained per subsample (§10.4).
pub const SCHEME_CBCS: FourCc = *b"cbcs";

/// How strictly the protection records are read. The default is the
/// ISO/IEC 23001-7 Amd 1 syntax and the §10 scheme rules; `compat`
/// additionally accepts the layout the staged encrypted conformance
/// files (`docs/image/heif/conformance/cenc/`, "under consideration"
/// upstream) were written with, where it departs from that text:
///
/// * the `ienc` pattern byte holds `skip_byte_block` in its high nibble
///   and `crypt_byte_block` in its low one (§8.3.2 has crypt first —
///   the files' own encryption parameters, e.g. `cens_64iv.xml`
///   `crypt_byte_block="10" skip_byte_block="1"` over a byte `0x1a`,
///   show the swap);
/// * `multi_subindex_IV` is 16 bits wide (Amd 1 §7.1: 8), taken when
///   the 8-bit reading does not parse the auxiliary data exactly;
/// * a `cbcs` item with per-sample IVs (§10.4 requires constant IVs)
///   chains its cipher blocks across the item's subsamples, as `cbc1`
///   does, instead of restarting the chain in every subsample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CencReading {
    /// Accept the departures listed above.
    pub compat: bool,
}

impl CencReading {
    /// The Amd 1 syntax only (the default).
    pub fn strict() -> Self {
        Self::default()
    }

    /// Also accept the staged files' layout.
    pub fn compat() -> Self {
        Self { compat: true }
    }
}

/// One key definition of an `ienc` property.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EncryptionKey {
    /// `Per_Sample_IV_Size` (0, 8 or 16; 0 = constant IV).
    pub per_sample_iv_size: u8,
    /// `KID`.
    pub kid: [u8; 16],
    /// `constant_IV` when `Per_Sample_IV_Size` is 0.
    pub constant_iv: Option<Vec<u8>>,
}

impl EncryptionKey {
    /// A key with a per-sample IV of `per_sample_iv_size` bytes.
    pub fn new(per_sample_iv_size: u8, kid: [u8; 16]) -> Self {
        Self {
            per_sample_iv_size,
            kid,
            constant_iv: None,
        }
    }

    /// Use a constant IV (sets `Per_Sample_IV_Size` to 0).
    pub fn with_constant_iv(mut self, iv: Vec<u8>) -> Self {
        self.per_sample_iv_size = 0;
        self.constant_iv = Some(iv);
        self
    }
}

/// `ienc` (23001-7 Amd 1 §8.3).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ItemEncryption {
    /// Box version (1 when pattern encryption is in use).
    pub version: u8,
    /// `crypt_byte_block` (version ≥ 1).
    pub crypt_byte_block: u8,
    /// `skip_byte_block` (version ≥ 1).
    pub skip_byte_block: u8,
    /// The key definitions (1-based in the auxiliary data).
    pub keys: Vec<EncryptionKey>,
}

impl ItemEncryption {
    /// A version-0 (no pattern) property over `keys`.
    pub fn new(keys: Vec<EncryptionKey>) -> Self {
        Self {
            version: 0,
            crypt_byte_block: 0,
            skip_byte_block: 0,
            keys,
        }
    }

    /// Pattern encryption (version 1).
    pub fn with_pattern(mut self, crypt_byte_block: u8, skip_byte_block: u8) -> Self {
        self.version = 1;
        self.crypt_byte_block = crypt_byte_block & 0x0f;
        self.skip_byte_block = skip_byte_block & 0x0f;
        self
    }

    /// Parse the property box body (FullBox header included).
    pub fn parse(b: &[u8]) -> Result<Self> {
        let (version, _flags, body) = parse_full_box(b)?;
        let mut r = Reader::new(body);
        r.u8("ienc reserved")?;
        let pattern = r.u8("ienc pattern")?;
        let (crypt_byte_block, skip_byte_block) = if version == 0 {
            (0, 0)
        } else {
            (pattern >> 4, pattern & 0x0f)
        };
        let n = r.u8("ienc num_keys")? as usize;
        let mut keys = Vec::with_capacity(n);
        for _ in 0..n {
            let per_sample_iv_size = r.u8("ienc Per_Sample_IV_Size")?;
            if !matches!(per_sample_iv_size, 0 | 8 | 16) {
                return Err(HeifError::invalid(format!(
                    "ienc Per_Sample_IV_Size {per_sample_iv_size} (§9.1: 0, 8 or 16)"
                )));
            }
            let mut kid = [0u8; 16];
            kid.copy_from_slice(r.bytes(16, "ienc KID")?);
            let constant_iv = if per_sample_iv_size == 0 {
                let len = r.u8("ienc constant_IV_size")? as usize;
                if !matches!(len, 8 | 16) {
                    return Err(HeifError::invalid(format!(
                        "ienc constant_IV_size {len} (§9.1: 8 or 16)"
                    )));
                }
                Some(r.bytes(len, "ienc constant_IV")?.to_vec())
            } else {
                None
            };
            keys.push(EncryptionKey {
                per_sample_iv_size,
                kid,
                constant_iv,
            });
        }
        Ok(Self {
            version,
            crypt_byte_block,
            skip_byte_block,
            keys,
        })
    }

    /// The property box body (FullBox header included).
    pub fn to_body(&self) -> Vec<u8> {
        let mut b = vec![self.version, 0, 0, 0, 0];
        b.push(if self.version == 0 {
            0
        } else {
            (self.crypt_byte_block << 4) | (self.skip_byte_block & 0x0f)
        });
        b.push(self.keys.len().min(255) as u8);
        for k in self.keys.iter().take(255) {
            b.push(k.per_sample_iv_size);
            b.extend_from_slice(&k.kid);
            if k.per_sample_iv_size == 0 {
                let iv = k.constant_iv.as_deref().unwrap_or(&[]);
                b.push(iv.len() as u8);
                b.extend_from_slice(iv);
            }
        }
        b
    }

    /// The pattern with its nibbles exchanged (see [`CencReading`]).
    pub fn with_swapped_pattern(mut self) -> Self {
        std::mem::swap(&mut self.crypt_byte_block, &mut self.skip_byte_block);
        self
    }

    /// Key entry `index` (1-based, as the auxiliary data names it).
    pub fn key(&self, index: u16) -> Result<&EncryptionKey> {
        index
            .checked_sub(1)
            .and_then(|i| self.keys.get(i as usize))
            .ok_or_else(|| {
                HeifError::invalid(format!(
                    "key entry {index} of an ienc with {} keys",
                    self.keys.len()
                ))
            })
    }
}

/// `iaux` (23001-7 Amd 1 §8.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ItemAuxInfo {
    /// `aux_info_type`.
    pub aux_info_type: FourCc,
    /// `aux_info_type_parameter`.
    pub aux_info_type_parameter: u32,
}

impl ItemAuxInfo {
    /// Both fields.
    pub fn new(aux_info_type: FourCc, aux_info_type_parameter: u32) -> Self {
        Self {
            aux_info_type,
            aux_info_type_parameter,
        }
    }

    /// Parse the property box body (FullBox header included).
    pub fn parse(b: &[u8]) -> Result<Self> {
        let (_v, _f, body) = parse_full_box(b)?;
        let mut r = Reader::new(body);
        Ok(Self {
            aux_info_type: r.fourcc("iaux aux_info_type")?,
            aux_info_type_parameter: r.u32("iaux aux_info_type_parameter")?,
        })
    }

    /// The property box body (FullBox header included).
    pub fn to_body(&self) -> Vec<u8> {
        let mut b = vec![0, 0, 0, 0];
        b.extend_from_slice(&self.aux_info_type);
        b.extend_from_slice(&self.aux_info_type_parameter.to_be_bytes());
        b
    }
}

/// One subsample of a protected item (§9.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Subsample {
    /// The key entry its protected bytes use (1-based; 0 = no key, only
    /// valid with no protected bytes).
    pub key_index: u16,
    /// `BytesOfClearData`.
    pub clear: u16,
    /// `BytesOfProtectedData`.
    pub protected: u32,
}

impl Subsample {
    /// A subsample under key entry `key_index`.
    pub fn new(key_index: u16, clear: u16, protected: u32) -> Self {
        Self {
            key_index,
            clear,
            protected,
        }
    }
}

/// A parsed `CencSampleAuxiliaryDataFormat` (§7.1, Amd 1 §7.1).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CencAuxData {
    /// `(key entry, IV)` pairs (the single-key form names entry 1).
    pub ivs: Vec<(u16, Vec<u8>)>,
    /// The subsamples; `None`: the whole item is protected under key
    /// entry 1 (full sample encryption, §9.4 / §9.7).
    pub subsamples: Option<Vec<Subsample>>,
}

impl CencAuxData {
    /// Parse `b` for an item whose `ienc` is `ienc`;
    /// `aux_info_type_parameter` 1 selects the multiple-key form.
    pub fn parse(b: &[u8], ienc: &ItemEncryption, aux_info_type_parameter: u32) -> Result<Self> {
        Self::parse_with(b, ienc, aux_info_type_parameter, CencReading::strict())
    }

    /// [`CencAuxData::parse`] under `reading` (the 16-bit
    /// `multi_subindex_IV` fallback of [`CencReading::compat`]).
    pub fn parse_with(
        b: &[u8],
        ienc: &ItemEncryption,
        aux_info_type_parameter: u32,
        reading: CencReading,
    ) -> Result<Self> {
        match Self::parse_layout(b, ienc, aux_info_type_parameter, 1) {
            Err(e) if reading.compat && aux_info_type_parameter == 1 => {
                Self::parse_layout(b, ienc, aux_info_type_parameter, 2).map_err(|_| e)
            }
            r => r,
        }
    }

    /// One reading, `subindex_width` bytes per `multi_subindex_IV`; the
    /// multiple-key form must consume the data exactly.
    fn parse_layout(
        b: &[u8],
        ienc: &ItemEncryption,
        aux_info_type_parameter: u32,
        subindex_width: usize,
    ) -> Result<Self> {
        let mut r = Reader::new(b);
        match aux_info_type_parameter {
            0 => {
                let iv_size = ienc.key(1)?.per_sample_iv_size as usize;
                let iv = r.bytes(iv_size, "InitializationVector")?.to_vec();
                let ivs = if iv_size > 0 {
                    vec![(1, iv)]
                } else {
                    Vec::new()
                };
                let subsamples = if r.is_empty() {
                    None
                } else {
                    let n = r.u16("subsample_count")? as usize;
                    let mut v = Vec::with_capacity(n.min(4096));
                    for _ in 0..n {
                        let clear = r.u16("BytesOfClearData")?;
                        let protected = r.u32("BytesOfProtectedData")?;
                        v.push(Subsample::new(1, clear, protected));
                    }
                    Some(v)
                };
                Ok(Self { ivs, subsamples })
            }
            1 => {
                let n = r.u16("multi_IV_count")? as usize;
                let mut ivs = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    let idx = r.uint(subindex_width, "multi_subindex_IV")? as u16;
                    let size = ienc.key(idx)?.per_sample_iv_size as usize;
                    if size == 0 {
                        return Err(HeifError::invalid(format!(
                            "multi_subindex_IV {idx} names a constant-IV key (Amd 1 §7.1)"
                        )));
                    }
                    ivs.push((idx, r.bytes(size, "IV")?.to_vec()));
                }
                let n = r.u32("subsample_count")? as usize;
                let mut v = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    let key_index = r.u16("multi_subindex")?;
                    let clear = r.u16("BytesOfClearData")?;
                    let protected = r.u32("BytesOfProtectedData")?;
                    v.push(Subsample::new(key_index, clear, protected));
                }
                if !r.is_empty() {
                    return Err(HeifError::invalid(format!(
                        "{} bytes after the auxiliary information",
                        r.remaining()
                    )));
                }
                Ok(Self {
                    ivs,
                    subsamples: Some(v),
                })
            }
            p => Err(HeifError::unsupported(format!(
                "CencSampleAuxiliaryDataFormat for aux_info_type_parameter {p}"
            ))),
        }
    }
}

/// The AES-128 block cipher keyed by KID (FIPS-197; the caller's — the
/// crate holds no key material). Both calls work in place on a whole
/// number of 16-byte blocks; an unknown KID is an error the decode
/// returns as is.
pub trait BlockCipher: Sync {
    /// Forward cipher on every block of `blocks` (CTR keystream).
    fn encrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> Result<()>;
    /// Inverse cipher on every block of `blocks` (CBC decryption).
    fn decrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> Result<()>;
}

/// A shareable [`BlockCipher`] (the form
/// [`DecodeOptions::decryptor`](crate::DecodeOptions::decryptor) holds).
/// Two handles compare equal when they share one cipher.
#[derive(Clone)]
pub struct SharedBlockCipher(std::sync::Arc<dyn BlockCipher + Send>);

impl SharedBlockCipher {
    /// Wrap `cipher`.
    pub fn new(cipher: impl BlockCipher + Send + 'static) -> Self {
        Self(std::sync::Arc::new(cipher))
    }

    /// The cipher.
    pub fn cipher(&self) -> &dyn BlockCipher {
        &*self.0
    }
}

impl std::fmt::Debug for SharedBlockCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedBlockCipher(..)")
    }
}

impl PartialEq for SharedBlockCipher {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The 16-byte IV of key entry `key`: its IV in the auxiliary data, or
/// its constant IV; an 8-byte IV fills bytes 0..8, bytes 8..16 zero
/// (§9.1).
fn iv_of(ienc: &ItemEncryption, aux: &CencAuxData, key: u16) -> Result<[u8; 16]> {
    let k = ienc.key(key)?;
    let iv = match aux.ivs.iter().find(|(i, _)| *i == key) {
        Some((_, iv)) => iv.as_slice(),
        None => k.constant_iv.as_deref().ok_or_else(|| {
            HeifError::invalid(format!(
                "no IV for key entry {key} (neither per-item nor constant)"
            ))
        })?,
    };
    let mut out = [0u8; 16];
    let n = iv.len().min(16);
    out[..n].copy_from_slice(&iv[..n]);
    Ok(out)
}

/// The §9.3 counter block `n` blocks after `iv`: the low 8 bytes count
/// as a 64-bit block counter wrapping without carrying into bytes 0..8.
fn counter(iv: &[u8; 16], n: u64) -> [u8; 16] {
    let mut c = *iv;
    let low = u64::from_be_bytes(iv[8..].try_into().expect("8 bytes")).wrapping_add(n);
    c[8..].copy_from_slice(&low.to_be_bytes());
    c
}

/// Per-key decryption state over one item.
struct KeyChain {
    key: u16,
    kid: [u8; 16],
    iv: [u8; 16],
    /// Protected byte ranges `(offset, len)` of this key, in order.
    ranges: Vec<(usize, usize)>,
}

/// Decrypt `data`, the item data of an item protected under `scheme`
/// with `ienc` and auxiliary data `aux` (23001-7 §9 / §10), through
/// `cipher`. The ranges, IVs, counters, chaining and patterns follow
/// the scheme:
///
/// * `cenc` (§9.3, §9.5.1): AES-CTR over the protected bytes of each key
///   as one logically continuous byte sequence (a block may straddle a
///   subsample boundary), the counter restarting at the item's IV;
/// * `cens` (§9.6): AES-CTR with the `crypt_byte_block` /
///   `skip_byte_block` pattern restarting in every subsample; the
///   counter moves per encrypted block, continuous over the item; a
///   partial block stays clear;
/// * `cbc1` (§9.4.3, §9.5.1): AES-CBC chained over the item's protected
///   blocks of each key; trailing partial blocks clear;
/// * `cbcs` (§9.5.1, §9.6, §10.4): AES-CBC with the pattern, the chain
///   restarting from the IV in every subsample; a partial block clear.
///
/// Without subsamples the whole item is the protected range of key
/// entry 1 (§9.4 / §9.7).
pub fn decrypt(
    scheme: FourCc,
    ienc: &ItemEncryption,
    aux: &CencAuxData,
    data: &[u8],
    cipher: &dyn BlockCipher,
) -> Result<Vec<u8>> {
    decrypt_with(scheme, ienc, aux, data, cipher, CencReading::strict())
}

/// [`decrypt`] under `reading` (the `cbcs` per-sample-IV chaining of
/// [`CencReading::compat`]; the pattern nibbles are the caller's — see
/// [`ItemEncryption::with_swapped_pattern`]).
pub fn decrypt_with(
    scheme: FourCc,
    ienc: &ItemEncryption,
    aux: &CencAuxData,
    data: &[u8],
    cipher: &dyn BlockCipher,
    reading: CencReading,
) -> Result<Vec<u8>> {
    let per_sample_ivs = !aux.ivs.is_empty();
    let (ctr, per_subsample_chain) = match &scheme {
        b"cenc" | b"cens" => (true, false),
        b"cbc1" => (false, false),
        b"cbcs" => (false, !(reading.compat && per_sample_ivs)),
        other => {
            return Err(HeifError::unsupported(format!(
                "protection scheme '{}' (cenc / cbc1 / cens / cbcs are defined)",
                crate::boxes::fourcc_str(other)
            )));
        }
    };
    let pattern = (
        ienc.crypt_byte_block as usize,
        ienc.skip_byte_block as usize,
    );
    let patterned = pattern.0 + pattern.1 > 0 && matches!(&scheme, b"cens" | b"cbcs");
    let whole = [Subsample::new(1, 0, data.len() as u32)];
    let subsamples = aux.subsamples.as_deref().unwrap_or(&whole);
    let total: u64 = subsamples
        .iter()
        .map(|s| s.clear as u64 + s.protected as u64)
        .sum();
    if total != data.len() as u64 {
        return Err(HeifError::invalid(format!(
            "subsamples cover {total} bytes of a {}-byte item (§9.1)",
            data.len()
        )));
    }
    let mut out = data.to_vec();
    let mut chains: Vec<KeyChain> = Vec::new();
    let mut pos = 0usize;
    // Every subsample's protected range, per key, in order.
    let mut per_subsample: Vec<(u16, usize, usize)> = Vec::new();
    for s in subsamples {
        pos += s.clear as usize;
        let len = s.protected as usize;
        if len > 0 {
            if s.key_index == 0 {
                return Err(HeifError::invalid(
                    "protected bytes without a key entry (multi_subindex 0)",
                ));
            }
            let ix = match chains.iter().position(|c| c.key == s.key_index) {
                Some(ix) => ix,
                None => {
                    chains.push(KeyChain {
                        key: s.key_index,
                        kid: ienc.key(s.key_index)?.kid,
                        iv: iv_of(ienc, aux, s.key_index)?,
                        ranges: Vec::new(),
                    });
                    chains.len() - 1
                }
            };
            chains[ix].ranges.push((pos, len));
            per_subsample.push((s.key_index, pos, len));
        }
        pos += len;
    }
    // The encrypted 16-byte block positions of a protected range under
    // the pattern (or every whole block without one); `partial` adds a
    // trailing partial block (CTR without a pattern only).
    let blocks_of = |off: usize, len: usize, partial: bool| -> Vec<(usize, usize)> {
        let mut v = Vec::with_capacity(len / 16 + 1);
        let mut i = 0usize;
        let mut k = 0usize;
        while i + 16 <= len {
            let encrypted = !patterned || pattern.0 == 0 || k % (pattern.0 + pattern.1) < pattern.0;
            if encrypted {
                v.push((off + i, 16));
            }
            i += 16;
            k += 1;
        }
        if partial && i < len {
            v.push((off + i, len - i));
        }
        v
    };
    if ctr {
        for c in &chains {
            // The pieces of this key's keystream, in order.
            let mut pieces: Vec<(usize, usize)> = Vec::new();
            if patterned {
                for &(o, l) in &c.ranges {
                    pieces.extend(blocks_of(o, l, false));
                }
            } else {
                pieces.extend(c.ranges.iter().copied());
            }
            let bytes: usize = pieces.iter().map(|p| p.1).sum();
            let nblocks = bytes.div_ceil(16);
            let mut ks = Vec::with_capacity(nblocks * 16);
            for n in 0..nblocks as u64 {
                ks.extend_from_slice(&counter(&c.iv, n));
            }
            cipher.encrypt_blocks(&c.kid, &mut ks)?;
            // cens: one counter per encrypted block (pieces are whole
            // blocks); cenc: the protected bytes as one byte stream.
            let mut k = 0usize;
            for (o, l) in pieces {
                for (dst, key) in out[o..o + l].iter_mut().zip(&ks[k..k + l]) {
                    *dst ^= key;
                }
                k += l;
            }
        }
    } else {
        let cbc = |out: &mut [u8], kid: &[u8; 16], iv: [u8; 16], blocks: &[(usize, usize)]| {
            let mut buf = Vec::with_capacity(blocks.len() * 16);
            for &(o, _) in blocks {
                buf.extend_from_slice(&out[o..o + 16]);
            }
            let cipher_text = buf.clone();
            cipher.decrypt_blocks(kid, &mut buf)?;
            let mut prev = iv;
            for (i, &(o, _)) in blocks.iter().enumerate() {
                for j in 0..16 {
                    out[o + j] = buf[i * 16 + j] ^ prev[j];
                }
                prev.copy_from_slice(&cipher_text[i * 16..i * 16 + 16]);
            }
            Ok::<(), HeifError>(())
        };
        if per_subsample_chain {
            for &(key, o, l) in &per_subsample {
                let c = chains.iter().find(|c| c.key == key).expect("key chain");
                let blocks = blocks_of(o, l, false);
                cbc(&mut out, &c.kid, c.iv, &blocks)?;
            }
        } else {
            for c in &chains {
                let mut blocks = Vec::new();
                for &(o, l) in &c.ranges {
                    blocks.extend(blocks_of(o, l, false));
                }
                cbc(&mut out, &c.kid, c.iv, &blocks)?;
            }
        }
    }
    Ok(out)
}

/// Parse a `sinf` box payload: `(original_format, scheme_type,
/// scheme_version)` from its `frma` / `schm` children.
pub fn parse_sinf(payload: &[u8]) -> Result<ProtectionScheme> {
    let mut s = ProtectionScheme::default();
    for h in crate::boxes::iter_boxes(payload) {
        let h = h?;
        let body = crate::boxes::payload(payload, &h);
        match &h.box_type {
            b"frma" => s.original_format = Some(Reader::new(body).fourcc("frma data_format")?),
            b"schm" => {
                let (_v, _f, b) = parse_full_box(body)?;
                let mut r = Reader::new(b);
                s.scheme_type = Some(r.fourcc("schm scheme_type")?);
                s.scheme_version = r.u32("schm scheme_version")?;
            }
            _ => {}
        }
    }
    s.raw = payload.to_vec();
    Ok(s)
}

/// One `sinf` of an `ipro` box (ISO/IEC 14496-12 §8.12).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProtectionScheme {
    /// `frma` original format, when present.
    pub original_format: Option<FourCc>,
    /// `schm` scheme type (`cenc`, `cbcs`, …).
    pub scheme_type: Option<FourCc>,
    /// `schm` scheme version.
    pub scheme_version: u32,
    /// The `sinf` payload as found.
    pub raw: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A toy "cipher" (XOR with the KID) — enough to check the ranges,
    /// counters, chaining and patterns this module builds around it.
    struct Xor;
    impl BlockCipher for Xor {
        fn encrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> Result<()> {
            for (i, b) in blocks.iter_mut().enumerate() {
                *b ^= kid[i % 16];
            }
            Ok(())
        }
        fn decrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> Result<()> {
            self.encrypt_blocks(kid, blocks)
        }
    }

    fn ienc(iv: u8) -> ItemEncryption {
        ItemEncryption::new(vec![EncryptionKey::new(iv, [7; 16])])
    }

    #[test]
    fn ienc_and_iaux_round_trip() {
        let e = ItemEncryption::new(vec![
            EncryptionKey::new(8, [1; 16]),
            EncryptionKey::new(0, [2; 16]).with_constant_iv(vec![3; 16]),
        ])
        .with_pattern(1, 9);
        assert_eq!(ItemEncryption::parse(&e.to_body()).unwrap(), e);
        let a = ItemAuxInfo::new(*b"cenc", 1);
        assert_eq!(ItemAuxInfo::parse(&a.to_body()).unwrap(), a);
        assert!(ItemEncryption::parse(&[0, 0, 0, 0, 0, 0, 1, 5]).is_err());
    }

    #[test]
    fn ctr_counter_wraps_the_low_64_bits_only() {
        let mut iv = [0u8; 16];
        iv[7] = 1;
        iv[8..].copy_from_slice(&u64::MAX.to_be_bytes());
        let c = counter(&iv, 1);
        assert_eq!(c[7], 1);
        assert_eq!(&c[8..], &[0; 8]);
    }

    #[test]
    fn cenc_keystream_is_one_byte_stream_across_subsamples() {
        // Two subsamples of 3 clear + 10 protected: the second range
        // continues the first one's keystream (block 0 bytes 10..16).
        let e = ienc(8);
        let aux = CencAuxData {
            ivs: vec![(1, vec![0; 8])],
            subsamples: Some(vec![Subsample::new(1, 3, 10), Subsample::new(1, 3, 10)]),
        };
        let data = vec![0u8; 26];
        let out = decrypt(*b"cenc", &e, &aux, &data, &Xor).unwrap();
        // Counter block 0 is all zero → keystream = KID (7s): stream
        // bytes 0..16 are 7, block 1 (low byte 1) starts at stream byte
        // 16, the item's byte 22.
        assert_eq!(&out[..3], &[0; 3]);
        assert!(out[3..13].iter().all(|&b| b == 7));
        assert_eq!(&out[13..16], &[0; 3]);
        assert!(out[16..26].iter().all(|&b| b == 7));
        // 36 protected bytes in one range: stream byte 31 is block 1's
        // byte 15, keyed 7 ^ 1.
        let aux = CencAuxData {
            ivs: vec![(1, vec![0; 8])],
            subsamples: Some(vec![Subsample::new(1, 0, 36)]),
        };
        let out = decrypt(*b"cenc", &e, &aux, &[0u8; 36], &Xor).unwrap();
        assert_eq!(out[31], 7 ^ 1);
        assert_eq!(out[32], 7);
    }

    #[test]
    fn cbcs_pattern_restarts_per_subsample_and_leaves_partial_blocks_clear() {
        let e = ItemEncryption::new(vec![
            EncryptionKey::new(0, [0; 16]).with_constant_iv(vec![9; 16]),
        ])
        .with_pattern(1, 1);
        // 40 protected bytes: blocks 0 (crypt), 1 (skip), partial 8 clear.
        let aux = CencAuxData {
            ivs: Vec::new(),
            subsamples: Some(vec![Subsample::new(1, 2, 40), Subsample::new(1, 0, 16)]),
        };
        let data = vec![1u8; 58];
        let out = decrypt(*b"cbcs", &e, &aux, &data, &Xor).unwrap();
        // Zero KID → D(C) = C; P = C ^ IV (9) for the first encrypted
        // block of every subsample.
        assert_eq!(&out[..2], &[1, 1]);
        assert!(out[2..18].iter().all(|&b| b == 1 ^ 9));
        assert!(out[18..42].iter().all(|&b| b == 1));
        assert!(out[42..58].iter().all(|&b| b == 1 ^ 9));
    }

    #[test]
    fn multi_key_aux_data_strict_and_compat_widths() {
        let e = ItemEncryption::new(vec![
            EncryptionKey::new(8, [1; 16]),
            EncryptionKey::new(8, [2; 16]),
        ]);
        // Amd 1 §7.1: 8-bit multi_subindex_IV.
        let mut strict = vec![0, 2, 1];
        strict.extend([0xaa; 8]);
        strict.push(2);
        strict.extend([0xbb; 8]);
        strict.extend([0, 0, 0, 1, 0, 2, 0, 3, 0, 0, 0, 16]);
        let a = CencAuxData::parse(&strict, &e, 1).unwrap();
        assert_eq!(a.ivs, vec![(1, vec![0xaa; 8]), (2, vec![0xbb; 8])]);
        assert_eq!(a.subsamples, Some(vec![Subsample::new(2, 3, 16)]));
        // The 16-bit layout: refused strictly, read under compat.
        let mut wide = vec![0, 2, 0, 1];
        wide.extend([0xaa; 8]);
        wide.extend([0, 2]);
        wide.extend([0xbb; 8]);
        wide.extend([0, 0, 0, 1, 0, 2, 0, 3, 0, 0, 0, 16]);
        assert!(CencAuxData::parse(&wide, &e, 1).is_err());
        let b = CencAuxData::parse_with(&wide, &e, 1, CencReading::compat()).unwrap();
        assert_eq!(a, b);
        // Swapped pattern nibbles.
        let p = ItemEncryption::new(vec![])
            .with_pattern(1, 9)
            .with_swapped_pattern();
        assert_eq!((p.crypt_byte_block, p.skip_byte_block), (9, 1));
    }

    #[test]
    fn subsamples_must_cover_the_item() {
        let aux = CencAuxData {
            ivs: vec![(1, vec![0; 8])],
            subsamples: Some(vec![Subsample::new(1, 1, 1)]),
        };
        assert!(decrypt(*b"cenc", &ienc(8), &aux, &[0; 3], &Xor).is_err());
        assert!(decrypt(*b"abcd", &ienc(8), &aux, &[0; 2], &Xor).is_err());
    }
}
