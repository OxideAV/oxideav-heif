//! Items this crate protects itself (ISO/IEC 23001-7 Amd 1 §8.3 / §8.4)
//! and predictively coded items it writes (HEIF §6.4.9) — the two r485
//! decode paths exercised without the staged conformance files, so they
//! run everywhere:
//!
//! * `HeifWriter::protect_item` encrypts a vendored picture's coded item
//!   under each scheme (`cenc` / `cens` / `cbc1` / `cbcs`; per-item and
//!   constant IVs; the single- and the multiple-key auxiliary data; NAL
//!   subsamples and whole-item protection) through a toy block cipher;
//!   the file must decode to the unprotected picture with the cipher,
//!   refuse typed without it, and never decode to it under a wrong key.
//! * The samples of the vendored inter-coded sequence become image
//!   items — the IRAP one, then each `P` picture with `pred` references
//!   to the pictures before it; every item must decode to the track's
//!   picture.
//!
//! `OXIDEAV_HEIF_WRITE_SEEDS=<dir>` also writes the files (fuzz seeds).
#![cfg(feature = "registry")]

mod common;

use common::cipher::ToyCipher;
use common::{fixture_bytes, vendored_root};
use oxideav_heif::cenc::{
    BlockCipher, CencAuxData, EncryptionKey, ItemEncryption, SCHEME_CBC1, SCHEME_CBCS, SCHEME_CENC,
    SCHEME_CENS, Subsample, nal_subsamples,
};
use oxideav_heif::props::{Ispe, ItemProperties, Property};
use oxideav_heif::writer::HeifWriter;
use oxideav_heif::{DecodeOptions, HeifFile, decode_all, decode_all_partial, decode_with};

fn seed(name: &str, bytes: &[u8]) {
    if let Ok(dir) = std::env::var("OXIDEAV_HEIF_WRITE_SEEDS") {
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(std::path::Path::new(&dir).join(name), bytes).unwrap();
    }
}

/// A cipher that knows another key (every KID bit flipped).
struct WrongKey;
impl BlockCipher for WrongKey {
    fn encrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> oxideav_heif::Result<()> {
        ToyCipher.encrypt_blocks(&kid.map(|b| !b), blocks)
    }
    fn decrypt_blocks(&self, kid: &[u8; 16], blocks: &mut [u8]) -> oxideav_heif::Result<()> {
        ToyCipher.decrypt_blocks(&kid.map(|b| !b), blocks)
    }
}

/// How one case protects the item.
struct Case {
    name: &'static str,
    scheme: [u8; 4],
    ienc: ItemEncryption,
    /// Per-item IVs.
    ivs: Vec<(u16, Vec<u8>)>,
    /// Subsamples over the NAL units (`None`: the whole item).
    nal: bool,
    /// Alternate the NAL units between key entries 1 and 2.
    two_keys: bool,
    param: u32,
}

fn cases() -> Vec<Case> {
    let k1 = || EncryptionKey::new(8, [0x11; 16]);
    let k2 = || EncryptionKey::new(16, [0x22; 16]);
    let constant = || EncryptionKey::new(0, [0x33; 16]).with_constant_iv(vec![0x44; 16]);
    let iv1 = || vec![(1u16, vec![0xa5u8; 8])];
    vec![
        Case {
            name: "cenc-nal",
            scheme: SCHEME_CENC,
            ienc: ItemEncryption::new(vec![k1()]),
            ivs: iv1(),
            nal: true,
            two_keys: false,
            param: 0,
        },
        Case {
            name: "cenc-whole",
            scheme: SCHEME_CENC,
            ienc: ItemEncryption::new(vec![k1()]),
            ivs: iv1(),
            nal: false,
            two_keys: false,
            param: 0,
        },
        Case {
            name: "cenc-two-keys",
            scheme: SCHEME_CENC,
            ienc: ItemEncryption::new(vec![k1(), k2()]),
            ivs: vec![(1, vec![0xa5; 8]), (2, vec![0x5a; 16])],
            nal: true,
            two_keys: true,
            param: 1,
        },
        Case {
            name: "cens-pattern",
            scheme: SCHEME_CENS,
            ienc: ItemEncryption::new(vec![k1()]).with_pattern(1, 2),
            ivs: iv1(),
            nal: true,
            two_keys: false,
            param: 0,
        },
        Case {
            name: "cbc1-nal",
            scheme: SCHEME_CBC1,
            ienc: ItemEncryption::new(vec![k2()]),
            ivs: vec![(1, vec![0x5a; 16])],
            nal: true,
            two_keys: false,
            param: 0,
        },
        Case {
            name: "cbcs-constant-iv",
            scheme: SCHEME_CBCS,
            ienc: ItemEncryption::new(vec![constant()]).with_pattern(1, 1),
            ivs: Vec::new(),
            nal: true,
            two_keys: false,
            param: 0,
        },
        Case {
            name: "cbcs-constant-iv-whole",
            scheme: SCHEME_CBCS,
            ienc: ItemEncryption::new(vec![constant()]),
            ivs: Vec::new(),
            nal: false,
            two_keys: false,
            param: 0,
        },
        Case {
            name: "cbcs-two-keys",
            scheme: SCHEME_CBCS,
            ienc: ItemEncryption::new(vec![constant(), k2()]).with_pattern(2, 1),
            ivs: vec![(2, vec![0x5a; 16])],
            nal: true,
            two_keys: true,
            param: 1,
        },
    ]
}

#[test]
fn protected_items_written_by_this_crate_decrypt_and_decode() {
    let root = vendored_root();
    let mut written = 0usize;
    for bundle in [
        "still-image-with-alpha",
        "still-yuv444",
        "still-10bit-main10",
    ] {
        let clear = fixture_bytes(&root, bundle);
        let want = decode_with(&clear, &DecodeOptions::default()).unwrap();
        let source = HeifFile::parse_borrowed(&clear).unwrap();
        let primary = source.primary_item().unwrap().id;
        for case in cases() {
            let tag = format!("{bundle} {}", case.name);
            let mut w = HeifWriter::new();
            let id = w.import_item(&source, primary).unwrap();
            w.set_primary(id);
            let item = w.items().iter().find(|i| i.id == id).unwrap();
            let oxideav_heif::writer::ItemBody::Coded(body) = &item.body else {
                panic!("{tag}: the primary is one coded body");
            };
            let length_size = item
                .properties
                .iter()
                .find_map(|(p, _)| match p {
                    Property::HvcC(h) => Some(h.length_size as usize),
                    _ => None,
                })
                .unwrap();
            // HEVC: VCL units (nal_unit_type < 32) keep their two-byte
            // header and a few slice-header bytes clear; the rest clear.
            let mut unit = 0u16;
            let subsamples: Option<Vec<Subsample>> = case.nal.then(|| {
                let mut all = Vec::new();
                let mut pos = 0usize;
                while pos < body.len() {
                    let len = body[pos..pos + length_size]
                        .iter()
                        .fold(0usize, |a, &b| (a << 8) | b as usize);
                    let end = pos + length_size + len;
                    let key = if case.two_keys { 1 + unit % 2 } else { 1 };
                    unit += 1;
                    let block_align = case.scheme == SCHEME_CBC1;
                    all.extend(
                        nal_subsamples(&body[pos..end], length_size, key, block_align, |nal| {
                            (nal.first().is_some_and(|b| (b >> 1) & 0x3f < 32)).then_some(6)
                        })
                        .unwrap(),
                    );
                    pos = end;
                }
                all
            });
            let protected_bytes: u64 = subsamples.as_ref().map_or(body.len() as u64, |s| {
                s.iter().map(|x| x.protected as u64).sum()
            });
            assert!(protected_bytes >= 16, "{tag}: nothing to protect");
            let aux = CencAuxData::new(case.ivs.clone(), subsamples);
            let clear_body = body.clone();
            let aux_item = w
                .protect_item(
                    id,
                    case.scheme,
                    case.ienc.clone(),
                    &aux,
                    case.param,
                    &ToyCipher,
                )
                .unwrap_or_else(|e| panic!("{tag}: {e}"));
            // Protecting twice is refused.
            assert!(
                w.protect_item(
                    id,
                    case.scheme,
                    case.ienc.clone(),
                    &aux,
                    case.param,
                    &ToyCipher
                )
                .is_err()
            );
            let bytes = w.write_to_vec().unwrap();
            seed(
                &format!("minimal.protected-{bundle}-{}.heic", case.name),
                &bytes,
            );
            written += 1;

            // The container says what was done.
            let file = HeifFile::parse_borrowed(&bytes).unwrap();
            let meta = file.meta().unwrap();
            let pid = file.primary_item().unwrap().id;
            assert_eq!(
                meta.protection_scheme(pid).and_then(|s| s.scheme_type),
                Some(case.scheme),
                "{tag}"
            );
            let props = ItemProperties::resolve(meta, pid).unwrap();
            assert_eq!(props.ienc(), Some(&case.ienc), "{tag}");
            assert_eq!(
                meta.references_from(pid, b"auxr").len(),
                aux_item.iter().len()
            );
            assert_ne!(&*file.item_data(pid).unwrap(), &clear_body[..], "{tag}");
            assert_eq!(
                file.item_data(pid).unwrap().len(),
                clear_body.len(),
                "{tag}"
            );

            // No cipher: the typed refusal naming item and scheme.
            let e = decode_with(&bytes, &DecodeOptions::default()).unwrap_err();
            let (item_id, scheme) = e
                .encrypted_item_info()
                .unwrap_or_else(|| panic!("{tag}: {e}"));
            assert_eq!(item_id, pid, "{tag}");
            assert_eq!(scheme.as_bytes(), &case.scheme, "{tag}");
            assert!(decode_all(&bytes).is_err(), "{tag}");
            assert!(decode_all_partial(&bytes, &DecodeOptions::default()).is_err());

            // With it: the unprotected picture, sample for sample.
            let keyed = DecodeOptions::default().with_decryptor(ToyCipher);
            let got = decode_with(&bytes, &keyed).unwrap_or_else(|e| panic!("{tag}: {e}"));
            assert_eq!(got, want, "{tag}");
            let all = oxideav_heif::decode_all_with(&bytes, &keyed).unwrap();
            assert_eq!(all[0].image, want, "{tag}");
            // The framework decoder, configured with the same options.
            {
                use oxideav_core::{CodecId, Decoder, Frame, Packet, TimeBase};
                let mut dec = oxideav_heif::HeifCodec::new(CodecId::new("heif"))
                    .with_decode_options(keyed.clone());
                let pkt = Packet::new(0, TimeBase::new(1, 1), bytes.clone());
                dec.send_packet(&pkt)
                    .unwrap_or_else(|e| panic!("{tag}: framework: {e}"));
                let Frame::Video(v) = dec.receive_frame().unwrap() else {
                    panic!("{tag}: a picture");
                };
                let params = dec.output_params().unwrap();
                let image = oxideav_heif::HeifImage::from_video_frame(&v, &params).unwrap();
                assert_eq!(image.to_rgba8(), want.to_rgba8(), "{tag}: framework pixels");
            }
            // A wrong key never yields the picture.
            let wrong = DecodeOptions::default().with_decryptor(WrongKey);
            assert!(
                decode_with(&bytes, &wrong)
                    .map(|g| g != want)
                    .unwrap_or(true),
                "{tag}: wrong key"
            );
        }
    }
    assert_eq!(written, 24);
}

#[test]
fn protect_item_refuses_what_the_schemes_do_not_allow() {
    let root = vendored_root();
    let clear = fixture_bytes(&root, "still-yuv444");
    let source = HeifFile::parse_borrowed(&clear).unwrap();
    let primary = source.primary_item().unwrap().id;
    let fresh = || {
        let mut w = HeifWriter::new();
        let id = w.import_item(&source, primary).unwrap();
        w.set_primary(id);
        (w, id)
    };
    let whole = CencAuxData::new(Vec::new(), None);
    let constant = ItemEncryption::new(vec![
        EncryptionKey::new(0, [3; 16]).with_constant_iv(vec![4; 16]),
    ]);
    // A constant IV under a counter scheme (§8.3.1).
    for scheme in [SCHEME_CENC, SCHEME_CENS] {
        let (mut w, id) = fresh();
        assert!(
            w.protect_item(id, scheme, constant.clone(), &whole, 0, &ToyCipher)
                .is_err()
        );
    }
    // A pattern under a scheme without one.
    let (mut w, id) = fresh();
    let patterned = constant.clone().with_pattern(1, 9);
    assert!(
        w.protect_item(id, SCHEME_CBC1, patterned, &whole, 0, &ToyCipher)
            .is_err()
    );
    // An unknown scheme, an unknown item, subsamples that do not cover
    // the item, a missing IV.
    let (mut w, id) = fresh();
    assert!(
        w.protect_item(id, *b"abcd", constant.clone(), &whole, 0, &ToyCipher)
            .is_err()
    );
    assert!(
        w.protect_item(
            id + 100,
            SCHEME_CBCS,
            constant.clone(),
            &whole,
            0,
            &ToyCipher
        )
        .is_err()
    );
    let short = CencAuxData::new(Vec::new(), Some(vec![Subsample::new(1, 1, 16)]));
    assert!(
        w.protect_item(id, SCHEME_CBCS, constant.clone(), &short, 0, &ToyCipher)
            .is_err()
    );
    let per_item = ItemEncryption::new(vec![EncryptionKey::new(8, [1; 16])]);
    assert!(
        w.protect_item(id, SCHEME_CBC1, per_item, &whole, 0, &ToyCipher)
            .is_err()
    );
    // Every refusal left the item as it was: it still writes and decodes.
    let bytes = w.write_to_vec().unwrap();
    assert_eq!(
        decode_with(&bytes, &DecodeOptions::default()).unwrap(),
        decode_with(&clear, &DecodeOptions::default()).unwrap()
    );
}

#[test]
fn predictively_coded_items_written_from_a_track_decode_to_its_pictures() {
    let root = vendored_root();
    let bytes = fixture_bytes(&root, "image-sequence-3frame");
    let frames = decode_all(&bytes).unwrap();
    let track_frames: Vec<_> = frames.iter().filter(|f| f.track_id.is_some()).collect();
    assert_eq!(track_frames.len(), 3);
    let file = HeifFile::parse_borrowed(&bytes).unwrap();
    let movie = oxideav_heif::sequence::parse_movie(&file).unwrap().unwrap();
    let track = &movie.tracks[0];
    let entry = track.primary_entry().unwrap();
    assert!(track.samples[0].is_sync && !track.samples[1].is_sync);
    let props = || {
        vec![
            (Property::HvcC(entry.hvcc.clone().unwrap()), true),
            (
                Property::Ispe(Ispe::new(entry.width as u32, entry.height as u32)),
                false,
            ),
        ]
    };
    let mut w = HeifWriter::new();
    let mut ids = Vec::new();
    for s in &track.samples {
        let data = oxideav_heif::sequence::sample_bytes(&file, s)
            .unwrap()
            .to_vec();
        let id = w.add_coded_item(*b"hvc1", data, props());
        // HEIF §6.4.9: the items the picture depends on, in decoding order.
        if !ids.is_empty() {
            w.add_reference(*b"pred", id, ids.clone());
        }
        ids.push(id);
    }
    // The last picture — two references deep — is the primary.
    w.set_primary(ids[2]);
    let out = w.write_to_vec().unwrap();
    seed("minimal.pred-items-from-track.heic", &out);
    let parsed = HeifFile::parse_borrowed(&out).unwrap();
    for (k, id) in ids.iter().enumerate() {
        let img = oxideav_heif::decode_item(&parsed, *id, oxideav_heif::ItemDecoder::direct())
            .unwrap_or_else(|e| panic!("item {id}: {e}"));
        let want = track_frames[k].image.clone();
        let got = oxideav_heif::HeifImage::from(img);
        assert_eq!(got.width, want.width);
        assert_eq!(
            got.to_rgba8(),
            want.to_rgba8(),
            "item {id} vs track picture {k}"
        );
    }
    // The whole file: the primary first, then the others by id.
    let all = decode_all(&out).unwrap();
    assert_eq!(
        all.iter().map(|f| f.item_id.unwrap()).collect::<Vec<_>>(),
        [ids[2], ids[0], ids[1]]
    );
    // A reference to an item that is not there is refused, never a panic.
    let mut w2 = HeifWriter::new();
    let data = oxideav_heif::sequence::sample_bytes(&file, &track.samples[1])
        .unwrap()
        .to_vec();
    let lone = w2.add_coded_item(*b"hvc1", data, props());
    w2.set_primary(lone);
    let lone_file = w2.write_to_vec().unwrap();
    assert!(
        decode_all(&lone_file).is_err(),
        "a P picture without its reference"
    );
}
