//! Registry glue (`registry` feature): installs the HEIF demuxer, the
//! container probe, the file-extension hints and the `"heif"`
//! still-image codec into an [`oxideav_core::RuntimeContext`], and
//! exposes the `__oxideav_entry` dispatch point that
//! `oxideav-meta::register_all` calls.
//!
//! # Probe priority
//!
//! The MP4 / MOV demuxers also score an `ftyp` box at 100. HEIF-family
//! brands (`heic`, `heix`, `mif1`, `msf1`, `miaf`, `avif`, …) make
//! [`crate::demux::probe`] return 100 too, so the tie is broken by
//! resolution priority: this crate registers at [`PROBE_PRIORITY`],
//! below the default, and therefore wins for HEIF brands while files
//! with only generic (`isom`, `qt  `, `mp42`) brands score 0 here and
//! stay with MP4 / MOV.

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, RuntimeContext};

use crate::demux::{make_decoder, open, probe, CODEC_ID, CONTAINER_NAME};
use crate::encode::make_encoder;
use crate::mux::open_muxer;

/// Resolution priority of the container probe (lower wins ties).
pub const PROBE_PRIORITY: i32 = oxideav_core::DEFAULT_PRIORITY - 50;

/// File extensions claimed for the container (MIAF §10.1 Table 5 plus
/// the AVIF family, which this container walks too).
pub const EXTENSIONS: &[&str] = &["heic", "heif", "heics", "heifs", "hif", "avif", "avifs"];

/// Register the container (demuxer + sequence muxer + probe +
/// extensions) and the `"heif"` still-image codec.
pub fn register(ctx: &mut RuntimeContext) {
    register_containers(&mut ctx.containers);
    register_codecs(&mut ctx.codecs);
}

/// Container-only registration.
pub fn register_containers(reg: &mut oxideav_core::ContainerRegistry) {
    reg.register_demuxer(CONTAINER_NAME, open);
    reg.register_muxer(CONTAINER_NAME, open_muxer);
    reg.register_probe_with_priority(CONTAINER_NAME, probe, PROBE_PRIORITY);
    for ext in EXTENSIONS {
        reg.register_extension_with_priority(ext, CONTAINER_NAME, PROBE_PRIORITY);
    }
}

/// Codec-only registration: the `"heif"` still-image decoder + encoder.
pub fn register_codecs(reg: &mut oxideav_core::CodecRegistry) {
    let caps = CodecCapabilities::video("heif_container")
        .with_decode()
        .with_encode()
        .with_intra_only(true)
        .with_pixel_formats(crate::encode::ENCODER_PIXEL_FORMATS.to_vec());
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID))
            .capabilities(caps)
            .decoder(make_decoder)
            .encoder(make_encoder),
    );
}

oxideav_core::register!("heif", register);

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{ContainerRegistry, ProbeData};

    fn ftyp(major: &[u8; 4], compat: &[&[u8; 4]]) -> Vec<u8> {
        crate::ftyp::FileType {
            box_type: *b"ftyp",
            major_brand: *major,
            minor_version: 0,
            compatible_brands: compat.iter().map(|b| **b).collect(),
        }
        .to_box()
    }

    fn always_100(_: &ProbeData) -> u8 {
        100
    }

    #[test]
    fn register_installs_container_and_codec() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        assert_eq!(
            ctx.containers.container_for_extension("heic"),
            Some(CONTAINER_NAME)
        );
        assert_eq!(
            ctx.containers.container_for_extension("HEIF"),
            Some(CONTAINER_NAME)
        );
        assert_eq!(
            ctx.containers.container_for_extension("avif"),
            Some(CONTAINER_NAME)
        );
        assert!(ctx.containers.demuxer_names().any(|n| n == CONTAINER_NAME));
        assert!(ctx.containers.muxer_names().any(|n| n == CONTAINER_NAME));
        assert!(ctx.codecs.has_decoder(&CodecId::new(CODEC_ID)));
        assert_eq!(
            ctx.containers.probe_priority(CONTAINER_NAME),
            Some(PROBE_PRIORITY)
        );
        // The macro-generated entry point installs the same claims.
        let mut ctx2 = RuntimeContext::new();
        __oxideav_entry(&mut ctx2);
        assert!(ctx2.codecs.has_decoder(&CodecId::new(CODEC_ID)));
    }

    #[test]
    fn heif_brands_beat_a_generic_ftyp_probe_registered_earlier() {
        let mut reg = ContainerRegistry::new();
        // A stand-in for the MP4 demuxer: scores every ftyp at 100 at
        // the default priority and was registered first.
        reg.register_probe("mp4-like", always_100);
        register_containers(&mut reg);
        let heic = ftyp(b"heic", &[b"mif1", b"heic", b"miaf"]);
        let c = reg.probe_candidates(&ProbeData {
            buf: &heic,
            ext: None,
        });
        assert_eq!(c[0].name, CONTAINER_NAME);
        assert_eq!(c[0].score, 100);
        // Generic brands: this crate scores 0, the other demuxer wins.
        let isom = ftyp(b"isom", &[b"mp42", b"iso2"]);
        let c = reg.probe_candidates(&ProbeData {
            buf: &isom,
            ext: None,
        });
        assert_eq!(c[0].name, "mp4-like");
        assert!(c.iter().all(|x| x.name != CONTAINER_NAME));
        let qt = ftyp(b"qt  ", &[b"qt  "]);
        let c = reg.probe_candidates(&ProbeData {
            buf: &qt,
            ext: Some("mov"),
        });
        assert!(c.iter().all(|x| x.name != CONTAINER_NAME));
    }
}
