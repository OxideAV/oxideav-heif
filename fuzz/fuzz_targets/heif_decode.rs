#![no_main]

//! The registry half of the image-crate contract on arbitrary bytes:
//! `decode_with` / `decode_rgba8` / `decode_all_with` under tight
//! limits (the codec crates have their own harnesses; this one covers
//! the container → codec → composition → RGB path and the limit checks
//! in front of it). The contract is "the call returns".

use libfuzzer_sys::fuzz_target;

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
});
