#![no_main]

//! The registry half of the image-crate contract on arbitrary bytes:
//! `decode_with` / `decode_rgba8` / `decode_all_with` under tight
//! limits (the codec crates have their own harnesses; this one covers
//! the container → codec → composition → RGB path and the limit checks
//! in front of it), plus external `tili` tiles served from the input,
//! protected items (ISO/IEC 23001-7 Amd 1: `ienc` / `iaux` / `auxi`
//! under a toy block cipher, the strict and the compat reading),
//! layered `lhv1` items with the base-layer fallback, predictively
//! coded items, and `decode_all_partial` against `decode_all_with`.
//! The contract is "the call returns".

use libfuzzer_sys::fuzz_target;

/// The toy block "cipher" the protected seeds are written with
/// (`tests/protected_items.rs`): a keyed byte substitution.
struct Toy;
impl oxideav_heif::cenc::BlockCipher for Toy {
    fn encrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> oxideav_heif::Result<()> {
        assert_eq!(blocks.len() % 16, 0, "whole blocks only");
        for (i, b) in blocks.iter_mut().enumerate() {
            *b = (*b ^ kid[i % 16]).wrapping_add(17).rotate_left(3);
        }
        Ok(())
    }
    fn decrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> oxideav_heif::Result<()> {
        assert_eq!(blocks.len() % 16, 0, "whole blocks only");
        for (i, b) in blocks.iter_mut().enumerate() {
            *b = b.rotate_right(3).wrapping_sub(17) ^ kid[i % 16];
        }
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    if !oxideav_heif::probe(data) {
        return;
    }
    // Bound the work per input: small pictures, few bytes, serial.
    let opts = oxideav_heif::DecodeOptions::default()
        .with_max_width(Some(512))
        .with_max_height(Some(512))
        .with_max_pixels(Some(1 << 16))
        .with_max_bytes(Some(1 << 18));
    if let Ok(img) = oxideav_heif::decode_with(data, &opts) {
        let _ = img.validate();
        let _ = img.to_rgba8();
        let _ = img.to_rgb8();
    }
    let _ = oxideav_heif::decode_rgba8(&data[..data.len().min(1 << 18)]);
    if let Ok(frames) = oxideav_heif::decode_all_with(data, &opts) {
        for f in frames.iter().take(8) {
            let _ = f.image.to_rgb8();
            let _ = f.delay;
        }
    }
    let strict = opts.clone().with_strict(true).with_tone_mapped(true);
    let _ = oxideav_heif::decode_with(data, &strict);
    // Protected items: the decryption walk on hostile `ienc` records,
    // auxiliary items and subsample maps, under both readings.
    let keyed = opts.clone().with_decryptor(Toy);
    if let Ok(img) = oxideav_heif::decode_with(data, &keyed) {
        let _ = img.validate();
    }
    let compat = keyed
        .clone()
        .with_cenc_reading(oxideav_heif::cenc::CencReading::compat());
    let _ = oxideav_heif::decode_with(data, &compat);
    // Layered items: the base layer of an `lhv1` item whose
    // enhancement layers do not decode.
    let _ = oxideav_heif::decode_with(data, &keyed.clone().with_base_layer_fallback(true));
    // The partial decode is `decode_all_with` minus what it lists:
    // complete exactly when the whole decode succeeds, and then equal.
    let whole = oxideav_heif::decode_all_with(data, &keyed);
    match oxideav_heif::decode_all_partial(data, &keyed) {
        Ok(p) if p.is_complete() => {
            let w = whole.expect("a complete partial decode is a whole decode");
            assert_eq!(p.frames.len(), w.len());
        }
        Ok(p) => {
            assert!(whole.is_err(), "decode_all succeeded where the partial decode skipped");
            assert!(!p.frames.is_empty());
        }
        Err(_) => assert!(whole.is_err(), "the partial decode refused a file decode_all takes"),
    }
    // External `tili` tiles (Amd 2 §6.11.2): every tile request is
    // answered from the input itself, at an offset picked by the tile
    // index and the input's last byte — the tile-file checks, the
    // per-tile graphs and the composition run on hostile tile files.
    let owned = data[..data.len().min(1 << 18)].to_vec();
    let tiles = opts.with_tile_resolver(
        move |r: &oxideav_heif::tiled::ExternalTileRequest| -> oxideav_heif::Result<Vec<u8>> {
            let step = *owned.last().unwrap_or(&0) as u64 + 1;
            let off = ((r.index * step) as usize).min(owned.len());
            Ok(owned[off..].to_vec())
        },
    );
    let _ = oxideav_heif::decode_with(data, &tiles);
});
