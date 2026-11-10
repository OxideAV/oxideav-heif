//! AES-128-ECB through the `openssl` binary — a black-box cipher for
//! the encrypted conformance files (the crate ships none), behind the
//! crate's `BlockCipher` hook — and the `drm_cfg` test-key reader.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use oxideav_heif::HeifError;
use oxideav_heif::cenc::BlockCipher;

pub fn hex(s: &str) -> Vec<u8> {
    let s = s.trim_start_matches("0x");
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// `KID → key` from a drm_cfg file's `<key KID=".." value=".."/>` lines.
pub fn keys(xml: &str) -> HashMap<[u8; 16], Vec<u8>> {
    let attr = |line: &str, name: &str| -> Option<String> {
        let at = line.find(&format!("{name}=\""))? + name.len() + 2;
        Some(line[at..at + line[at..].find('"')?].to_string())
    };
    xml.lines()
        .filter(|l| l.trim_start().starts_with("<key "))
        .filter_map(|l| {
            let kid: [u8; 16] = hex(&attr(l, "KID")?).try_into().ok()?;
            Some((kid, hex(&attr(l, "value")?)))
        })
        .collect()
}

/// AES-128-ECB through the `openssl` binary (a black-box validator).
pub struct OpenSsl(pub HashMap<[u8; 16], Vec<u8>>);

impl OpenSsl {
    fn run(&self, kid: &[u8; 16], blocks: &mut [u8], decrypt: bool) -> oxideav_heif::Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        let key = self
            .0
            .get(kid)
            .ok_or_else(|| HeifError::invalid(format!("no key for KID {kid:02x?}")))?;
        let key_hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        let mut cmd = Command::new("openssl");
        cmd.args(["enc", "-aes-128-ecb", "-nopad", "-K", &key_hex]);
        if decrypt {
            cmd.arg("-d");
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| HeifError::invalid(format!("openssl: {e}")))?;
        let input = blocks.to_vec();
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let out = child
            .wait_with_output()
            .map_err(|e| HeifError::invalid(format!("openssl: {e}")))?;
        writer.join().unwrap().unwrap();
        if out.stdout.len() != blocks.len() {
            return Err(HeifError::invalid("openssl returned a short output"));
        }
        blocks.copy_from_slice(&out.stdout);
        Ok(())
    }
}

impl BlockCipher for OpenSsl {
    fn encrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> oxideav_heif::Result<()> {
        self.run(kid, blocks, false)
    }
    fn decrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> oxideav_heif::Result<()> {
        self.run(kid, blocks, true)
    }
}

/// A toy block "cipher" for the tests that need no real secrecy — a
/// keyed byte substitution whose two directions differ, so a chain
/// walked the wrong way shows. The fuzz targets carry the same one
/// (their encrypted seeds are written with it).
pub struct ToyCipher;

impl BlockCipher for ToyCipher {
    fn encrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> oxideav_heif::Result<()> {
        if !blocks.len().is_multiple_of(16) {
            return Err(HeifError::invalid("not a whole number of blocks"));
        }
        for (i, b) in blocks.iter_mut().enumerate() {
            *b = (*b ^ kid[i % 16]).wrapping_add(17).rotate_left(3);
        }
        Ok(())
    }
    fn decrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> oxideav_heif::Result<()> {
        if !blocks.len().is_multiple_of(16) {
            return Err(HeifError::invalid("not a whole number of blocks"));
        }
        for (i, b) in blocks.iter_mut().enumerate() {
            *b = b.rotate_right(3).wrapping_sub(17) ^ kid[i % 16];
        }
        Ok(())
    }
}
