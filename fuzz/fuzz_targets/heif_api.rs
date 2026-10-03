#![no_main]

//! The standalone half of the image-crate contract on arbitrary bytes:
//! `probe` (never panics, no allocation), `info` (header read through
//! the item tree and the sequence tables, the layout prediction), and
//! `info_of` over the parsed file. The contract is "the call returns".

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = oxideav_heif::probe(data);
    if let Ok(i) = oxideav_heif::info(data) {
        // The header's own invariants.
        let _ = (i.width, i.height, i.frames, i.format.plane_count());
        let _ = i.format.layout();
        let _ = i.color.to_colr();
    }
    if let Ok(file) = oxideav_heif::HeifFile::parse_borrowed(data) {
        let _ = oxideav_heif::info_of(&file);
    }
    // Limits are enforced before any allocation.
    let opts = oxideav_heif::DecodeOptions::default()
        .with_max_width(4096)
        .with_max_height(4096)
        .with_max_pixels(1 << 22)
        .with_max_bytes(1 << 20);
    let _ = opts.check_input(data.len());
});
