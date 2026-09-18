# oxideav-heif

Pure-Rust HEIF / HEIC / MIAF image container (ISO/IEC 23008-12, ISO/IEC
23000-22) for the [oxideav](https://github.com/OxideAV) framework.

This crate owns the **container**: the ISOBMFF box tree, the `meta`
item model, derived images, auxiliaries, thumbnails, metadata, image
sequences, MIAF conformance and a writer. It never decodes an HEVC or
AV1 bitstream itself — coded items are handed to
[`oxideav-h265`](https://github.com/OxideAV/oxideav-h265) and
[`oxideav-av1`](https://github.com/OxideAV/oxideav-av1) through the
registry (the default-on `registry` feature). With
`default-features = false` the crate is a dependency-free container
parser / composer / writer over its own planar frame type.

```rust
use oxideav_heif::{decode_primary, HeifFile, ItemDecoder};

let file = HeifFile::parse(&std::fs::read("photo.heic")?)?;
let image = decode_primary(&file, ItemDecoder::direct())?;
// image.frame: HeifFrame (planar YCbCr / mono, 8–16 bit, optional alpha plane)
// image.nclx / image.icc_profile / image.exif / image.xmp / image.thumbnail_ids
```

Through the framework: `oxideav_heif::register(&mut ctx)` installs the
`"heif"` demuxer (probe on the HEIF-family `ftyp` brands, `.heic` /
`.heif` / `.heics` / `.heifs` / `.hif` / `.avif` / `.avifs` hints) and
the `"heif"` codec (decoder + encoder). A `.heic` opens as stream 0 =
the still image (one `"heif"` packet → one composed `VideoFrame`) plus
one stream per image-sequence track (`"h265"` / `"av1"` packets with
`hvcC` / `av1C` extradata).

## Capability matrix

| Area | Status |
|------|--------|
| ISOBMFF boxes | size 0 / 1 / `largesize` / `uuid`, FullBox, bounded recursive walk |
| Brands | `mif1` `mif2` `msf1` `heic` `heix` `hevc` `hevx` `heim` `heis` `hevm` `hevs` `miaf` `MiHB` `MiHA` `MiHE` `MiAB` `avif` `avis` `avio` `MA1B` `MA1A` `jpeg` `avci` `1pic` `pred`, `styp` |
| `meta` tree | `hdlr`, `pitm` v0/v1, `iinf` v0/v1 + `infe` v2/v3 (`mime` / `uri ` tails, hidden flag), `iloc` v0–v2 (all widths, construction methods 0 / 1 / 2, multi-extent, zero-length "to end"), `iref` v0/v1, `iprp` / `ipco` / `ipma` v0/v1 (7 / 15-bit indices, essential flag, index-0 placeholders), `idat`, `grpl`, `dinf` / `dref`, `ipro` |
| Properties | `ispe` `pixi` `colr` (nclx + `rICC` / `prof`) `pasp` `clap` `irot` `imir` `iscl`\* `auxC` `hvcC` `av1C` `lhvC`\* `avcC`\* `clli` `mdcv` `cclv` `amve` `rloc` `lsel` `a1op` `a1lx` `rref` `crtt` `mdft` `udes` `altt`; §6.5.1 descriptive-before-transformative order, unrecognised-essential refusal, exact rational `clap` |
| Coded items | `hvc1` / `hev1` → oxideav-h265 (`hvcC` extradata, length-prefixed AU); `av01` → oxideav-av1 (`av1C` extradata, temporal unit); 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 at 8–16 bit; via direct factories or a caller `CodecRegistry` |
| Derived images | `grid` (row-major, trim, tile alpha), `iovl` (sRGB fill via the H.273 matrix of the output `colr`, offsets, clipping, §6.9.1 straight / pre-multiplied alpha, translucent canvas → output alpha), `iden`, `tmap`\* (base image only) |
| Transforms | `clap` → `irot` → `imir` in `ipma` order; sub-sample chroma positions promote to 4:4:4 (MIAF §7.3.6.7 rule) |
| Auxiliaries | alpha (`urn:mpeg:mpegB:cicp:systems:auxiliary:alpha` and `urn:mpeg:hevc:2015:auxid:1`, resized / depth-matched, `prem`), depth (both URN families, surfaced as a frame) |
| Metadata | thumbnails (`thmb`), Exif (offset word resolved), XMP, ICC, effective `nclx` (MIAF default when absent), `pixi` / `clli` / `mdcv` / … via the typed property list |
| MIAF | `MiafProfile` + `check`: §7 general requirements, §8 shared constraints, Annex A HEVC / AV1 codec limits — typed `MiafViolation`s with clause numbers |
| Image sequences | `moov` / `trak` / `stbl` (`stts` `ctts` `stsc` `stsz` `stz2` `stco` `co64` `stss` `tref` `elst`), visual sample entries (`hvcC` `av1C` `ccst` `auxi` `colr` `clap` `pasp`), §7.2.1 matrix → rotation / mirror; framework `Demuxer` with pts / dts / sync / seek |
| Writer | `HeifWriter` (coded / grid / overlay / identity items, thumbnails, alpha / depth, Exif / XMP, entity groups, de-duplicated `ipco`, MIAF `mdat` order, brand auto-selection); `SequenceWriter` (`msf1` / `hevc`, `pict` track + `ccst`, cover-image `meta`) |
| Encoder (`registry`) | `encode_still`: HEVC (lossless `pcm` or CABAC `intra` at a QP) or lossless AV1 items, padding + `clap`, grid tiling (MIAF 64-px floor), thumbnails, alpha, Exif / XMP / ICC, transforms as an `iden` item; `"heif"` framework `Encoder` (frame in, file out; options `codec` / `mode` / `qp` / `grid` / `thumbnail`) |
| Fuzz | `fuzz/`: `heif_parse`, `heif_compose`, `heif_sequence` (standalone build), daily workflow |

\* parsed / surfaced, not applied: `iscl` (image scaling) refuses at
composition, `lhvC` / `avcC` items have no decoder here, `tmap` gain
maps decode to the base image.

## Corpus scorecard (`docs/image/heif/fixtures/`, 14 bundles)

Container traces (BOX / PITM / ITEM_INFO / IREF / IPRP_PROP /
IPRP_ASSOC / HVCC / HEVC_FRAME_FOR_ITEM) match byte-for-byte on every
bundle; every bundle is MIAF-conformant under `check`.

| Bundle | Pixels vs `expected.png` | Black-box video decoder |
|--------|--------------------------|-------------------------|
| single-image-512x512-q60 | mean 0.03 / max 1 (8-bit) | byte-exact |
| single-image-1x1 (`clap` 64→1) | exact | — |
| single-image-with-thumbnail | mean 0.03 / max 1 | byte-exact |
| still-image-with-alpha | exact (RGBA) | byte-exact (colour item) |
| still-image-grid-2x2 | exact | byte-exact (composed 256×256) |
| still-image-overlay | mean 0.005 / max 2 | — |
| still-image-with-icc / -exif / -xmp | mean 0.03 / max 1 | byte-exact |
| multi-image-burst-3 | mean 0.03 / max 1 (all three items) | byte-exact |
| still-monochrome | exact | byte-exact |
| still-10bit-main10 | mean 0.13 / max 0.4 (8-bit units) | byte-exact (16-bit LE) |
| still-yuv444 | exact | byte-exact |
| image-sequence-3frame | track frames: max 10 vs per-frame oracles | byte-exact (all three samples) |

"Exact" = sample-exact after this crate's YCbCr→RGB conversion; the
non-exact rows differ from the oracle only by its own chroma
upsampling / rounding. The oracle for the sequence bundle's still is a
different encode than the meta primary (mean 1.1 / max 10).

## Standalone build

```toml
oxideav-heif = { version = "0.0", default-features = false }
```

exposes `HeifFile`, `Meta`, `ItemProperties`, `HevcConfig` /
`Av1Config`, `derived::build_graph`, `miaf::check`, `sequence::parse_movie`,
the `compose` module on `HeifFrame`, and `HeifWriter` /
`SequenceWriter` — no framework or codec dependency. Pair it with any
HEVC / AV1 decoder to get pixels: decode the item bytes
(`HeifFile::item_data`) with the `hvcC` / `av1C` record and feed the
planes to `compose`.

## Where the pieces came from

The crate consolidates this workspace's earlier clean-room HEIF
machinery, rewritten on one model:

* `crates/oxideav-avif/src/{box_parser,meta,parser,derived,grid,
  overlay,transform,alpha,avis}.rs` — the box walker shape, the raw
  `iloc` / `ipma` / `infe` version handling, the construction-method-2
  resolver, the per-pixel §6.9.1 overlay semantics and the 4:4:4
  promotion idea, the `stbl` expansion.
* `crates/oxideav-mov/src/{bmff_meta,iprp,derived,heif_write,styp}.rs`
  — the typed `clli` / `mdcv` / `cclv` / `amve` / `lsel` layouts (both
  plain and FullBox-prefixed shapes are accepted), the grid / overlay
  descriptor parsers, the two-pass `iloc` / `mdat` writer layout and
  the `styp` reading.

Everything was re-derived against the staged ISO/IEC 23008-12 (2017 +
2025), ISO/IEC 23000-22 (2025), ISO/IEC 14496-12 (2015) and H.273
texts. **The AVIF profile (AV1-specific `should`-audits, gain maps,
progressive / layered items) lives in `oxideav-avif`; migrating that
crate onto this container is a follow-up.**

## Limits and rules

`MAX_ITEMS` 2²⁰, `MAX_PROPERTIES` 2¹⁵, `MAX_DERIVATION_DEPTH` 16,
`MAX_DERIVATION_INPUTS` 2¹⁶, `MAX_GRAPH_ITEMS` 2¹⁶, `MAX_CANVAS_PIXELS`
2³⁰, `MAX_ITEM_BYTES` 2³⁰, `MAX_ILOC_DEPTH` 8, `MAX_SAMPLES` 2²⁴,
`MAX_ITEM_DECODES` 4096, demuxer input ≤ 4 GiB in memory. Cycles in
`dimg` / `auxl` / `thmb` graphs and construction-method-2
self-references are rejected.

## License

MIT — see [LICENSE](./LICENSE).
