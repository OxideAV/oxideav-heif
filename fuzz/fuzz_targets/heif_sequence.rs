#![no_main]

//! Arbitrary bytes through the image-sequence walk: `moov` / `trak` /
//! `stbl`, sample-table expansion (stsc / stsz / stz2 / stco / co64 /
//! stts / ctts / stss), sample byte resolution and the §7.2.1 matrix
//! decode. The contract is "the call returns".

use libfuzzer_sys::fuzz_target;
use oxideav_heif::sequence::{parse_movie, sample_bytes};
use oxideav_heif::HeifFile;

fuzz_target!(|data: &[u8]| {
    let Ok(file) = HeifFile::parse(data) else {
        return;
    };
    let Ok(Some(movie)) = parse_movie(&file) else {
        return;
    };
    for t in movie.tracks.iter().take(64) {
        let _ = t.orientation();
        let _ = t.is_visual();
        let _ = t.primary_entry();
        let _ = t.sample_duration_total();
        let _ = t.references_of(b"auxl");
        for s in t.samples.iter().take(1024) {
            let _ = sample_bytes(&file, s);
            let _ = s.pts();
        }
    }
    let _ = movie.visual_tracks().count();
});
