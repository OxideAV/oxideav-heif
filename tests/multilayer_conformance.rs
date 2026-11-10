//! The MPEG / Nokia L-HEVC conformance files
//! (`docs/image/heif/conformance/heic/multilayer00{1..5}.heic`, built
//! from the elementary streams under `conformance/nokia/bitstreams/`)
//! through oxideav-h265's multi-layer decoder:
//!
//! * every `hvc1` item is its base layer (HEIF B.2.2.1.2: the
//!   `nuh_layer_id` > 0 NAL units the shared access unit carries are
//!   ignored) — byte-exact against the black-box decoder's output of the
//!   producer's base-layer stream;
//! * the MV-HEVC `lhv1` item of multilayer005 (`lsel` 1, `ster` group)
//!   is the second view, byte-exact against the black-box decoder's
//!   per-view output (`-view_ids`);
//! * the SHVC SNR `lhv1` items of multilayer001 / 002 and the hybrid
//!   item of multilayer004 (an `avc1` base named by `exbl`, H.265
//!   F.8.1.8) decode their enhancement layer. No black-box decoder on
//!   this machine decodes SHVC, so the oracle is the producer's own
//!   elementary stream decoded by the HEVC crate's sequence decoder
//!   (whose SHVC decode is pinned to the official SHVC conformance
//!   MD5s): the item equals that stream's layer-1 picture, which proves
//!   the carriage (`lhvC` parameter sets, operating point, external
//!   base); the enhancement layer's PSNR over the base is reported;
//! * the `lhv1` items of multilayer002 that are not IRAP access units
//!   are a typed refusal (HEIF B.2.2.1.3 NOTE 1, H.265 F.8.1.6), and
//!   their base layer comes back under `base_layer_fallback`.
//!
//! The files are not vendored (the staged set's licence terms); the
//! test skips without the umbrella's `docs/`. Black-box outputs are
//! re-derived when `ffmpeg` runs, else the pinned FNV-1a 64 values
//! taken from it are used.
#![cfg(feature = "registry")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_h265::sequence::{ExternalBasePicture, LayerTarget, SequenceDecoder};
use oxideav_heif::decode::{ItemDecoder, decode_item};
use oxideav_heif::{HeifError, HeifFile, HeifFrame};

/// `docs/image/heif/conformance` when the umbrella checkout has it.
fn conformance() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/image/heif/conformance");
    p.join("heic/multilayer001.heic").is_file().then_some(p)
}

fn open(root: &Path, name: &str) -> HeifFile {
    HeifFile::from_vec(std::fs::read(root.join("heic").join(name)).unwrap()).unwrap()
}

/// The tight planar bytes of an 8-bit frame (`yuv420p` order).
fn planar(f: &HeifFrame) -> Vec<u8> {
    let t = f.tight();
    t.planes
        .iter()
        .flat_map(|p| p.data.iter().copied())
        .collect()
}

fn decode(file: &HeifFile, id: u32) -> Result<HeifFrame, HeifError> {
    decode_item(file, id, ItemDecoder::direct()).map(|i| i.frame)
}

/// Annex B NAL units of `data` (3- and 4-byte start codes).
fn annex_b_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(data.len(), |n| n - 3);
        while e > s && data[e - 1] == 0 && k + 1 < starts.len() {
            e -= 1;
        }
        out.push(&data[s..e]);
    }
    out
}

/// `stream` restricted to `nuh_layer_id` 0, start-coded.
fn base_layer_stream(stream: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in annex_b_units(stream) {
        if n.len() >= 2 && ((n[0] & 1) << 5 | n[1] >> 3) == 0 {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(n);
        }
    }
    out
}

/// The black-box decoder's `yuv420p` output of an elementary stream
/// (`args` before `-i`), split into `frame_len` pictures; `None` when it
/// does not run.
fn black_box(
    dir: &Path,
    tag: &str,
    stream: &[u8],
    args: &[&str],
    frame_len: usize,
) -> Option<Vec<Vec<u8>>> {
    if !common::have_binary("ffmpeg") {
        return None;
    }
    let src = dir.join(format!("{tag}.bin"));
    let raw = dir.join(format!("{tag}.yuv"));
    std::fs::write(&src, stream).unwrap();
    let ok = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "quiet", "-y"])
        .args(args)
        .arg("-i")
        .arg(&src)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg(&raw)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    let bytes = std::fs::read(&raw).ok()?;
    Some(bytes.chunks(frame_len).map(<[u8]>::to_vec).collect())
}

/// FNV-1a 64 of each picture: from the black-box decoder when it runs,
/// else `pinned`; a black-box value that disagrees with the pin is
/// reported (the pin documents the run it came from).
fn oracle(fresh: Option<Vec<Vec<u8>>>, pinned: &[u64], what: &str) -> Vec<u64> {
    match fresh {
        Some(frames) => {
            let got: Vec<u64> = frames.iter().map(|f| common::fnv1a64(f)).collect();
            if got != pinned {
                eprintln!("{what}: black-box output {got:x?} differs from the pin {pinned:x?}");
            }
            got
        }
        None => {
            eprintln!("{what}: black-box decoder absent, using the pinned values");
            pinned.to_vec()
        }
    }
}

/// The output pictures of `layer` the HEVC sequence decoder yields for
/// an elementary stream (decode order), as planar 8-bit bytes.
fn sequence_layer(
    stream: &[u8],
    layer: u8,
    base: Option<Vec<u8>>,
    w: usize,
    h: usize,
) -> Vec<Vec<u8>> {
    let mut seq = SequenceDecoder::new();
    seq.set_layer_target(LayerTarget::Layer(layer));
    if let Some(base) = base {
        let (n, c) = (w * h, w * h / 4);
        let up = |s: &[u8]| s.iter().map(|&v| v as u16).collect::<Vec<u16>>();
        let pic = oxideav_h265::Picture::from_planes(
            w,
            h,
            1,
            8,
            8,
            up(&base[..n]),
            up(&base[n..n + c]),
            up(&base[n + c..n + 2 * c]),
        );
        seq.set_external_base_layer(move |_| Some(ExternalBasePicture::new(pic.clone())));
    }
    seq.push_annexb(stream).unwrap();
    let mut frames = seq.finish().unwrap();
    frames.retain(|f| f.output && f.layer_id == layer);
    frames
        .iter()
        .map(|f| f.output_picture().to_planar_u8().unwrap())
        .collect()
}

/// PSNR (dB) of `a` against `b`, 8-bit samples.
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let se: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (*x as f64 - *y as f64).powi(2))
        .sum();
    if se == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 * a.len() as f64 / se).log10()
    }
}

fn bitstream(root: &Path, name: &str) -> Vec<u8> {
    std::fs::read(root.join("nokia/bitstreams").join(name)).unwrap()
}

/// multilayer001: `hvc1` base + SHVC SNR `lhv1` (tols 1) over one
/// shared access unit (B020), in an `altr` group.
#[test]
fn multilayer001_base_and_snr_enhancement() {
    let Some(root) = conformance() else {
        eprintln!("SKIP: docs/image/heif/conformance not checked out");
        return;
    };
    let dir = common::scratch_dir("ml001");
    let file = open(&root, "multilayer001.heic");
    let b020 = bitstream(&root, "B020.265");
    let (w, h) = (1024, 512);
    let len = w * h * 3 / 2;
    let base_oracle = oracle(
        black_box(
            &dir,
            "b020_l0",
            &base_layer_stream(&b020),
            &["-f", "hevc"],
            len,
        ),
        &[0x48b7_8879_7ba3_202c],
        "B020 base layer",
    );
    let base = planar(&decode(&file, 20003).unwrap());
    assert_eq!(
        common::fnv1a64(&base),
        base_oracle[0],
        "hvc1 item 20003 vs the black-box base layer"
    );
    let enh = planar(&decode(&file, 20004).unwrap());
    let seq = sequence_layer(&b020, 1, None, w, h);
    assert_eq!(seq.len(), 1);
    assert_eq!(enh, seq[0], "lhv1 item 20004 vs B020 layer 1");
    let db = psnr(&base, &enh);
    eprintln!("multilayer001: layer 1 over layer 0 PSNR {db:.2} dB");
    assert!(db > 30.0 && db < 99.0, "{db}");
    // The primary is the base item.
    let p = oxideav_heif::decode::decode_primary(&file, ItemDecoder::direct()).unwrap();
    assert_eq!(p.item_id, 20003);
}

/// multilayer002: two 2x2 grids over the four access units of B021 —
/// the `hvc1` items (one IRAP, three `TRAIL_R`) and the SNR-enhanced
/// `lhv1` items over the same data.
#[test]
fn multilayer002_grids_of_base_and_enhanced_items() {
    let Some(root) = conformance() else {
        eprintln!("SKIP: docs/image/heif/conformance not checked out");
        return;
    };
    let dir = common::scratch_dir("ml002");
    let file = open(&root, "multilayer002.heic");
    let b021 = bitstream(&root, "B021.265");
    let (w, h) = (512, 256);
    let len = w * h * 3 / 2;
    let base_oracle = oracle(
        black_box(
            &dir,
            "b021_l0",
            &base_layer_stream(&b021),
            &["-f", "hevc"],
            len,
        ),
        &[
            0x6dea_8849_8bae_984b,
            0x7eed_f725_1968_ec2a,
            0xb31b_0529_03e6_b919,
            0xf4b9_f139_1d1f_3f7b,
        ],
        "B021 base layer",
    );
    let mut bases = Vec::new();
    for (k, id) in (20010..=20013).enumerate() {
        let f = planar(&decode(&file, id).unwrap());
        assert_eq!(
            common::fnv1a64(&f),
            base_oracle[k],
            "hvc1 item {id} vs black-box picture {k}"
        );
        bases.push(f);
    }
    // The IRAP lhv1 item: SNR enhancement layer of access unit 0.
    let enh = planar(&decode(&file, 20014).unwrap());
    let seq = sequence_layer(&b021, 1, None, w, h);
    assert_eq!(seq.len(), 4);
    assert_eq!(enh, seq[0], "lhv1 item 20014 vs B021 layer 1 picture 0");
    eprintln!(
        "multilayer002: item 20014 layer 1 over layer 0 PSNR {:.2} dB",
        psnr(&bases[0], &enh)
    );
    // The TRAIL_R lhv1 items: the enhancement picture of an
    // uninitialised layer is never output (F.8.1.6) — typed refusal;
    // the base layer under the fallback is the hvc1 item's picture.
    for (k, id) in (20015..=20017).enumerate() {
        let e = decode(&file, id).unwrap_err();
        assert!(e.to_string().contains("not an IRAP picture"), "{id}: {e}");
        let f = decode_item(&file, id, ItemDecoder::direct().base_layer_fallback()).unwrap();
        assert_eq!(planar(&f.frame), bases[k + 1], "item {id} base fallback");
    }
    // Grid 20018 tiles the four base pictures (2x2 of 512x256).
    let grid = decode(&file, 20018).unwrap();
    assert_eq!((grid.width, grid.height), (1024, 512));
    let g = grid.tight();
    for (k, tile) in bases.iter().enumerate() {
        let (x0, y0) = ((k % 2) * w, (k / 2) * h);
        for y in 0..h {
            let row = &g.planes[0].data[(y0 + y) * 1024 + x0..][..w];
            assert_eq!(row, &tile[y * w..][..w], "grid tile {k} luma row {y}");
        }
    }
    // Grid 20019 of the enhanced items refuses with its TRAIL_R tiles.
    assert!(decode(&file, 20019).is_err());
}

/// multilayer003: a `ster` pair of two single-layer `hvc1` views
/// (B022, two pictures).
#[test]
fn multilayer003_stereo_pair_of_single_layer_items() {
    let Some(root) = conformance() else {
        eprintln!("SKIP: docs/image/heif/conformance not checked out");
        return;
    };
    let dir = common::scratch_dir("ml003");
    let file = open(&root, "multilayer003.heic");
    let b022 = bitstream(&root, "B022.265");
    let len = 1024 * 512 * 3 / 2;
    let oracle = oracle(
        black_box(&dir, "b022", &b022, &["-f", "hevc"], len),
        &[0x48b7_8879_7ba3_202c, 0x48f2_ed47_546c_31ee],
        "B022",
    );
    let meta = file.meta().unwrap();
    let ids: Vec<u32> = meta.items.iter().map(|i| i.id).collect();
    assert_eq!(ids.len(), 2);
    for (k, id) in ids.into_iter().enumerate() {
        let f = planar(&decode(&file, id).unwrap());
        assert_eq!(
            common::fnv1a64(&f),
            oracle[k],
            "item {id} vs black-box picture {k}"
        );
    }
}

/// multilayer005: 2-view MV-HEVC (B025) — `hvc1` view (layer 0) +
/// `lhv1` item with `lsel` 1 (layer 1) in a `ster` group, each
/// byte-exact against the black-box decoder's output of that view
/// (`-view_ids` selects by ViewId: layer 0 carries ViewId 1, layer 1
/// ViewId 0 in this stream).
#[test]
fn multilayer005_mv_hevc_views() {
    let Some(root) = conformance() else {
        eprintln!("SKIP: docs/image/heif/conformance not checked out");
        return;
    };
    let dir = common::scratch_dir("ml005");
    let file = open(&root, "multilayer005.heic");
    let b025 = bitstream(&root, "B025.265");
    let len = 512 * 256 * 3 / 2;
    let view0 = oracle(
        black_box(&dir, "b025_v0", &b025, &["-view_ids", "0"], len),
        &[0xebc4_896c_daa9_3473],
        "B025 ViewId 0",
    );
    let view1 = oracle(
        black_box(&dir, "b025_v1", &b025, &["-view_ids", "1"], len),
        &[0x64e0_d4bd_bd94_09cb],
        "B025 ViewId 1",
    );
    let layer0 = planar(&decode(&file, 20003).unwrap());
    let layer1 = planar(&decode(&file, 20004).unwrap());
    assert_eq!(
        common::fnv1a64(&layer0),
        view1[0],
        "hvc1 item 20003 (layer 0, ViewId 1)"
    );
    assert_eq!(
        common::fnv1a64(&layer1),
        view0[0],
        "lhv1 item 20004 (layer 1, ViewId 0)"
    );
}

/// multilayer004: `avc1` base (B024) + SHVC enhancement `lhv1` (B023)
/// whose VPS has no internal base layer; the `exbl` reference names the
/// AVC item, whose decoded picture is the layer-0 picture (HEIF
/// B.2.2.1.3, H.265 F.8.1.8).
#[test]
fn multilayer004_hybrid_avc_base_through_exbl() {
    let Some(root) = conformance() else {
        eprintln!("SKIP: docs/image/heif/conformance not checked out");
        return;
    };
    let dir = common::scratch_dir("ml004");
    let file = open(&root, "multilayer004.heic");
    let (w, h) = (1024, 512);
    let len = w * h * 3 / 2;
    let b024 = bitstream(&root, "B024.264");
    let fresh = black_box(&dir, "b024", &b024, &["-f", "h264"], len);
    let base_oracle = oracle(fresh.clone(), &[0x6d1b_31f8_5323_eb4b], "B024");
    let base = planar(&decode(&file, 20003).unwrap());
    assert_eq!(
        common::fnv1a64(&base),
        base_oracle[0],
        "avc1 item 20003 vs the black-box AVC decode"
    );
    let enh = planar(&decode(&file, 20004).unwrap());
    // The producer's enhancement stream over the black-box AVC picture
    // (this crate's own when the black box is absent — pinned equal).
    let ext = fresh.map(|f| f[0].clone()).unwrap_or_else(|| base.clone());
    let seq = sequence_layer(&bitstream(&root, "B023.265"), 1, Some(ext), w, h);
    assert_eq!(seq.len(), 1);
    assert_eq!(enh, seq[0], "lhv1 item 20004 vs B023 layer 1 over B024");
    let db = psnr(&base, &enh);
    eprintln!("multilayer004: layer 1 over the AVC base PSNR {db:.2} dB");
    assert!(db > 30.0 && db < 99.0, "{db}");
    // Base-layer-only decode of an exbl item names the base item.
    let e = decode_item(&file, 20004, ItemDecoder::direct().base_layer_fallback()).unwrap_err();
    assert!(e.to_string().contains("exbl"), "{e}");
}

/// Re-mux: `HeifWriter::import_item` of every image item of the five
/// files carries the `lhv1` items with their `lhvC` / `oinf` / `tols` /
/// `lsel` (and every other property) byte for byte, pulls the `exbl`
/// base along, keeps the `ster` / `altr` groups whose members came
/// along, and every re-muxed item decodes to the original's pixels (or
/// refuses as the original does).
#[test]
fn layered_items_remux_byte_for_byte() {
    use oxideav_heif::HeifWriter;
    let Some(root) = conformance() else {
        eprintln!("SKIP: docs/image/heif/conformance not checked out");
        return;
    };
    for n in 1..=5 {
        let name = format!("multilayer00{n}.heic");
        let file = open(&root, &name);
        let meta = file.meta().unwrap();
        let images: Vec<u32> = meta
            .items
            .iter()
            .filter(|i| i.is_image())
            .map(|i| i.id)
            .collect();
        for &id in &images {
            let mut w = HeifWriter::new();
            let new_id = w.import_item(&file, id).unwrap();
            w.set_primary(new_id);
            let out = HeifFile::from_vec(w.write_to_vec().unwrap()).unwrap();
            let om = out.meta().unwrap();
            let raw = |m: &oxideav_heif::meta::Meta, i: u32| -> Vec<(Vec<u8>, bool)> {
                m.properties_of(i)
                    .into_iter()
                    .map(|(_, p, e)| {
                        let mut b = p.box_type.to_vec();
                        b.extend_from_slice(&p.body);
                        (b, e)
                    })
                    .collect()
            };
            assert_eq!(
                raw(om, new_id),
                raw(meta, id),
                "{name} item {id}: properties"
            );
            let src_type = meta.item(id).unwrap().item_type;
            assert_eq!(om.item(new_id).unwrap().item_type, src_type);
            for r in [*b"exbl", *b"pred"] {
                assert_eq!(
                    om.references_from(new_id, &r).len(),
                    meta.references_from(id, &r).len(),
                    "{name} item {id}: {} references",
                    String::from_utf8_lossy(&r)
                );
            }
            match (decode(&file, id), decode(&out, new_id)) {
                (Ok(a), Ok(b)) => assert_eq!(planar(&a), planar(&b), "{name} item {id}: pixels"),
                (Err(_), Err(_)) => {}
                (a, b) => panic!("{name} item {id}: {:?} vs {:?}", a.err(), b.err()),
            }
        }
    }
}
