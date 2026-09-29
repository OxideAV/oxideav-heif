# oxideav-heif

Pure-Rust HEIF / HEIC / MIAF image container (ISO/IEC 23008-12, ISO/IEC
23000-22) for the [oxideav](https://github.com/OxideAV) framework.

This crate owns the **container**: the ISOBMFF box tree, the `meta`
item model, derived images, auxiliaries, thumbnails, metadata, image
sequences, MIAF conformance and a writer. It never decodes an HEVC,
AV1 or AVC bitstream itself — coded items are handed to
[`oxideav-h265`](https://github.com/OxideAV/oxideav-h265),
[`oxideav-av1`](https://github.com/OxideAV/oxideav-av1) and
[`oxideav-h264`](https://github.com/OxideAV/oxideav-h264) through the
registry (the default-on `registry` feature). With
`default-features = false` the crate is a dependency-free container
parser / composer / writer over its own planar frame type.

```rust
use oxideav_heif::{decode_primary, HeifFile, ItemDecoder};

let bytes = std::fs::read("photo.heic")?;
let file = HeifFile::parse_borrowed(&bytes)?; // zero-copy view; `HeifFile::parse` copies, `from_vec` takes ownership
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
| Brands | `mif1` `mif2` `mif3` `msf1` `heic` `heix` `hevc` `hevx` `heim` `heis` `hevm` `hevs` `miaf` `MiHB` `MiHA` `MiHE` `MiAB` `avif` `avis` `avio` `MA1B` `MA1A` `jpeg` `avci` `avcs` `tmap` `1pic` `pred`, `styp` |
| Low-overhead files | Amd 2:2026 Annex O `mini` box (O.3.2 in full: bit-packed header, chunk sizes, HDR block with gain map) expanded to the normative O.4 equivalent `meta` + `mdat` on parse (`HeifFile::minimized`); `encode_still_minimized` writes `mif3` files (HEVC / AV1, alpha, ICC, Exif, XMP, gain map, Exif orientation) that libheif renders identically to the regular file; `image/hif2`, `.hmg`; `dExf` / deflated XMP inflate (`deflate` feature) |
| `meta` tree | `hdlr`, `pitm` v0/v1, `iinf` v0/v1 + `infe` v2/v3 (`mime` / `uri ` tails, hidden flag), `iloc` v0–v2 (all widths, construction methods 0 / 1 / 2, multi-extent, zero-length "to end"), `iref` v0/v1, `iprp` / `ipco` / `ipma` v0/v1 (7 / 15-bit indices, essential flag, index-0 placeholders), `idat`, `grpl`, `dinf` / `dref`, `ipro` |
| Properties | `ispe` `pixi` `colr` (nclx + `rICC` / `prof`) `pasp` `clap` `irot` `imir` `iscl` `auxC` `hvcC` `av1C` `lhvC` `oinf` `tols` `avcC` `clli` `mdcv` `cclv` `amve` (14496-12 8th ed. §12.1.6–9 plain-Box syntax, FullBox-prefixed writers tolerated) `rloc` `lsel` `a1op` `a1lx` `rref` `crtt` `mdft` `udes` `altt` `reve` `ndwt` `cexg` `dadj` `stag` (Amd 1:2025 §6.5.41–45) `tilC` + `tipa` and the per-channel `pixi` (`px_flags & 1`, Amd 2:2026); §6.5.1 descriptive-before-transformative order, unrecognised-essential refusal, exact rational `clap` |
| Coded items | `hvc1` / `hev1` → oxideav-h265 (`hvcC` extradata, length-prefixed AU); `av01` → oxideav-av1 (`av1C` extradata, temporal unit); `avc1` → oxideav-h264 (`avcC` extradata, length-prefixed AU; byte-exact against a black-box AVC decoder on Baseline / Main / High / 4:2:2 / 4:4:4 / 10-bit); `lhv1` → oxideav-h265's multi-layer decoder (`hvcC` ++ `lhvC` extradata, `layer=<lsel>` / `ols=<tols>`): the stereo shape (two `lhv1` items with `lsel` 0 / 1 in a `ster` group) yields each view, an item without `lsel` every output layer (`DecodedImage::layers`), both views byte-exact against a black-box decoder's per-view output; `cexg` items decode extent by extent; 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 at 8–16 bit; via direct factories or a caller `CodecRegistry`; grid / `tili` / `cexg` tiles decode in parallel under `ItemDecoder::with_execution_context` (byte-identical for every budget) |
| Derived images | `grid` (row-major, trim, tile alpha), `iovl` (sRGB fill via the H.273 matrix of the output `colr`, offsets, clipping, §6.9.1 straight / pre-multiplied alpha, translucent canvas → output alpha), `iden`, `tmap` (23008-12:2025/Amd 1:2025 §6.6.2.4: base by default, normative tone-mapped reconstruction on request or as an input of another derived item; see Gain maps), `cfen` (Amd 1 §6.6.2.5: the inputs' luma planes become Y / Cb / Cr or R / G / B and alpha; packed inputs refused — the Amd 2 region formula is a docs question), `tili` tiled items (Amd 2 §6.11: `deti` offset tables, empty tiles, `ispe` crop; external tiles refused) |
| Transforms | `clap` → `irot` → `imir` → `iscl` in `ipma` order; `iscl` (§6.5.13) resizes by the exact ceil-ratio with an area/bilinear resampler; sub-sample chroma positions promote to 4:4:4 (MIAF §7.3.6.7) |
| Auxiliaries | alpha (`urn:mpeg:mpegB:cicp:systems:auxiliary:alpha` and `urn:mpeg:hevc:2015:auxid:1`, resized / depth-matched, `prem`), depth (both URN families, surfaced as a frame); alpha *tracks* (§7.5.3 `auxv` + `auxl` + `auxi`, plain-Box `auxi` tolerated) composed into a `"heif"` stream of frames with alpha by the demuxer, written by `SequenceWriter::alpha` |
| Metadata | thumbnails (`thmb`), Exif (offset word resolved), XMP, ICC, effective `nclx` (MIAF default when absent), `pixi` / `clli` / `mdcv` / … via the typed property list |
| MIAF | `MiafProfile` + `check`: §7 general requirements, §8 shared constraints, Annex A HEVC / AV1 codec limits, the HEIF Amd 1 `tmap` / `cfen` shalls (`"HEIF-A1 …"`), MIAF Amd 1:2025 §7.3.11.5 (`tmap` in an `altr` with a master image, `"MIAF-A1 …"`), Amd 2 `pixi` / `mif3` / `tili` rules (`"HEIF-A2 …"`) — typed `MiafViolation`s with clause numbers, should-level rules as `advisories`; MIAF Amd 1 Annex A practices: `Meta::display_order` (`altr` collapse), `Track::loop_behaviour` (`elst` `RepeatEdits`) |
| Image sequences | `moov` / `trak` / `stbl` (`stts` `ctts` `stsc` `stsz` `stz2` `stco` `co64` `stss` `sbgp` `csgp` `sgpd` `tref` `elst`), top-level `prft` / `ssix`, visual sample entries (`hvcC` `av1C` `avcC` `lhvC` `ccst` `auxi` `colr` `clap` `pasp` `clli` `mdcv` `cclv` `amve`; QuickTime zero terminators tolerated), `elst` `RepeatEdits` + `tkhd` duration, §7.2.1 matrix → rotation / mirror; framework `Demuxer` with pts / dts / sync / seek |
| Writer | `HeifWriter` (coded / grid / overlay / identity / `tmap` / `cfen` / `tili` / `cexg` items, thumbnails, alpha / depth, Exif / XMP (raw bodies, any encoding) / arbitrary `cdsc` items, item names, entity groups with flags and payloads (`pymd` pyramids, `stem` stereo with fallback), de-duplicated `ipco`, MIAF `mdat` order, `deti` data references, brand auto-selection); `SequenceWriter` (`msf1` / `hevc`, brand override, `pict` track + `ccst`, HDR sample-entry boxes, `stco` / `co64` per §8.7.5, looping via `elst`, cover-image `meta` that can alias a track sample; opens in libheif / ImageMagick / ffmpeg / Apple ImageIO) |
| Gain maps | ISO 21496-1 + HEIF Amd 1:2025 §6.6.2.4 (the published text; "fully applied" = weight ±1.0): `ToneMapImage` body (`version` 0 + C.2 `GainMapMetadata`), `dimg` = [base, gain map] (count 2 enforced), the three `colr` placements and the `tmap` brand (§10.2.6) checked; `DecodedImage::gain_map` (decoded gain-map item + metadata + alternate `colr`), `apply_gain_map(h_target)` → linear RGB in the application space (Formulas 1–3, §6.2.2 resampling, Annex B primaries conversion, single↔multi-channel rules, limited-range clip); `ItemDecoder::tone_mapped()` → the reconstruction in the `tmap` item's `colr` at its `pixi` depth (alpha carried over); writer authors `tmap` items (`HeifWriter::add_tone_map`, `EncodeOptions::gain_map`) with the hidden map, the brand and the `altr` [tmap, base] fallback; matches a black-box tone-mapper within 1 code at every headroom (2 on the half-size map, 7 after a BT.2020→709 conversion of the 16-bit PQ reconstruction), and the tool tone-maps our authored file identically to its own |
| Colour | `rgb::to_rgb`: YCbCr → RGB(A) with the item `colr` matrix / range (H.273), identity (GBR), monochrome, alpha carried; the renderer step over the composed frame. Framework: the effective `nclx` rides as the core `ColorSignal` on streams and frames; identity-matrix 4:4:4 items are planar RGB (`Gbrp*` / `Gbrap*`) end to end |
| Encoder (`registry`) | `encode_still`: HEVC (lossless `pcm` or CABAC `intra` at a QP; VUI range + colour description, Main Still Picture, `rd` / `tiles`) or AV1 stills (lossless or quality 0..=100, `speed`, native 8/10/12-bit 4:0:0–4:4:4, `av1C` from the codec configuration) items, padding + `clap`, grid tiling (MIAF 64-px floor), thumbnails, alpha (per-codec `auxC` URN, single-channel `pixi`, own parameter-set ids; AV1 alpha as a monochrome still), Exif / XMP / ICC, transforms as essential properties on the coded item; `rd` / `tiles` pick the HEVC quadtree coder's CTB size automatically (`auto_ctb`, or `hevc_ctb`); `"heif"` framework `Encoder` (frame in — planar YCbCr / grey / planar RGB (`Gbrp*`, coded identity-matrix 4:4:4 by AV1) or packed RGB / RGBA / BGR(A) / 16-bit / grey+alpha — file out; declared options `codec` / `mode` / `qp` / `grid` / `thumbnail` / `range` / `quality` / `speed` / `rd` / `tiles` / `ctb`); the `"heif"` muxer passes its whole-file packets through, so `oxideav convert in.png out.heic` writes a HEIC |
| Fuzz | `fuzz/`: `heif_parse`, `heif_compose`, `heif_sequence`, `heif_records` (standalone build; `mini`, `deti`, `cfen` and every typed property with round-trip oracles), daily workflow |

`iscl` is applied at composition (§6.5.13); `tmap` gain maps are
applied on request (`DecodedImage::apply_gain_map` /
`ItemDecoder::tone_mapped()`), the default output stays the base.

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

## Real-world interop (`tests/fixtures/`, runs on CI)

A compact fixture set is vendored so the pixel / structure tests run
everywhere, not only where `docs/` is checked out. `corpus/` holds 13
of the staged bundles; `interop/` holds 39 files from three
independent black-box producers — Apple ImageIO (`sips`), libheif
(`heif-enc`, x265 / aom) and ImageMagick — across sizes 1×1 … 4032×3024
(odd sizes included), 8 / 10 / 12-bit, 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4,
alpha, lossless, thumbnails and Apple's 512-px `grid` tiling.

**Reader** (`tests/interop.rs`): every producer file decodes — directly
and through the framework demuxer + `"heif"` codec — to the manifest
geometry / layout; where the layouts coincide the composed planes are
**byte-exact** against the black-box video decoder's raw output
(fingerprinted in `manifest.tsv`, 25+ files across all three
producers); every `expected.png` matches within the reader's own
rounding with exact alpha; every file is MIAF-conformant.

**Writer** (`tests/writer_interop.rs`): every shape `encode_still`
writes re-parses, is MIAF-conformant, round-trips, and — when the
binary is present — is opened by `sips`, `heif-convert`, `magick`,
`ffmpeg` and `heif-info`, alpha files included. ImageMagick (its own
libheif decode) reproduces our decode within 1 code, transforms
included. Apple ImageIO refuses a file whose two items carry
byte-identical HEVC parameter sets, so the alpha auxiliary carries its
own parameter-set ids. The item's range and colour description are
signalled in the HEVC VUI, so `sips` — which takes the sample range
from the bitstream — renders our alpha exactly and our colour within
its own colour management. Lossy AV1 (quality 60 / 30, with alpha)
opens in all four readers and magick reproduces our decode within 1
code.

**Gain maps** (`tests/gainmap.rs`, `tests/fixtures/gainmap/`): four
AVIF files from a black-box gain-map tool (monochrome, RGB, half-size
and BT.2020-application-space gain maps, base headroom 0 → alternate
4) decode with the `tmap` metadata matching the tool's own report, and
`apply_gain_map` at headrooms 0 / 2 / 4 matches the tool's tone-mapped
renditions within 1 code (2 on the half-size resample).

**Layered items** (`tests/layered.rs`, `tests/fixtures/layered/`): a
three-frame MV-HEVC stereo movie from a black-box producer (Apple
VideoToolbox through `AVAssetWriter`; the producer source is vendored)
has its first access unit wrapped by `HeifWriter` as the HEIF stereo
shape — two `lhv1` items over the same access unit with `lsel` 0 / 1
in a `ster` group — and as a bare two-output-layer item. Each view
decodes **byte-exact** against the black-box decoder's per-view output
(`ffmpeg -view_ids 0` / `1`); the framework still stream announces the
two views (`CodecParameters::layers`) and the `"heif"` decoder emits
both frames tagged with the core `LayerIdentity`. No third-party reader
on the machine opens `lhv1` items (reported, not asserted).

**Low-overhead files** (`tests/mini.rs`): `encode_still_minimized`'s
`mif3` files (HEVC / AV1, alpha, ICC, Exif, XMP, gain map, orientation)
decode byte-identically to the regular writer's file of the same
picture, and `heif-convert` renders both identically; it keys its codec
choice on the equivalent brand this writer puts in `minor_version`.

**Tiled / enhanced items** (`tests/tiled.rs`): `tili` items with a
padded last row / column and an empty tile, `cexg` items with one
extent per tile and `cfen` items (Y + Cb + Cr at full or half
resolution, R + G + B under an identity matrix) round-trip through the
writer and the reader (serial and parallel) to the expected
composition; no third-party producer of these exists here.

`tmap` carriage is ISO/IEC 23008-12:2025/Amd 1:2025 §6.6.2.4 (the
published amendment, reconciled in r463 against the MPEG draft it was
built from — the one normative addition is the "fully applied"
definition, a weight of +1.0 / −1.0, which the reconstruction already
met): `dimg` = [base, gain map] with `reference_count` 2, the
item body is a `ToneMapImage` (`version` 0, then the C.2 metadata — any
other version is refused), the base carries the baseline `colr`, the
gain map an `nclx` with primaries = transfer = 2 (its matrix / range
decode the stored map; limited range clips to 0..1), the `tmap` item
the alternate `colr`; `ispe` on all three (the output is the base's
size), `pixi` on the `tmap` as the applied colour-resolution hint, the
gain map hidden, the `tmap` compatible brand (§10.2.6), and an `altr`
[tmap, base] group for readers without tone-map support — under MIAF
Amd 1:2025 §7.3.11.5 that grouping with a valid master image is
mandatory (`check` reports `"MIAF-A1 7.3.11.5"`). Decoding the
`tmap` item yields the base by default (an SDR pipeline's choice) and
the normative reconstruction — the map fully applied, re-encoded in
the `tmap` item's `colr` at the `pixi` depth — with
`ItemDecoder::tone_mapped()`; a `tmap` feeding another derived item is
always the applied image (§6.6.2.4.1). The published Amd 1 says
nothing about the other channels; the base's alpha is carried over to
the reconstruction as this crate's reading of §6.9.1 (the 4th-edition
working draft proposes the same). ISO 21496-1 anchors
the application space at HDR reference white = 1.0 but names no
absolute luminance for a PQ alternate; the reconstruction uses
203 cd/m² (the example in its 3.6) unless `with_reference_white`
overrides it, and relative transfers put the peak signal at
2^H_alternate × reference white (3.6 / 3.10).

## Conformance matrix (r462, this machine: macOS 26.6 / Apple M4 Max)

Generated by `tests/conformance_matrix.rs` (every cell is a
measurement from that run; rerun it with `OXIDEAV_HEIF_MATRIX_12MP=1`
in a release build to refresh — it prints both tables and writes
`matrix.md` to its scratch directory; CI runs it without the 12 MP
rows, which take a 12 MP encode per codec). Producers: Apple
ImageIO `sips`, libheif 1.23.4 `heif-enc` with x265 4.3 and with aom
3.15, ImageMagick 7 (libheif 1.23.1), ffmpeg 8 with libsvtav1 4.2
(AVIF only — it has no HEIF muxer). The black-box reference for the
reader direction is ffmpeg's own HEIF / AVIF demux + decode
(libdav1d / native hevc), compared plane by plane on the region it
emits; readers for the writer direction are `sips`, `heif-convert`,
`magick`, `ffmpeg` and `heif-info`.

Verdicts: **exact** = byte-exact planes (alpha included where the
black box exposes it); **Δ** = measured difference in code units;
**no producer** = the tool has no such option; **dropped** = the
producer wrote the file without the feature (measured, e.g. `sips` and
`magick` code a grey PNG as 4:2:0; `sips` bakes mirrors / crops into
pixels); **decodes; no black-box reference** = ffmpeg refuses the
file (every producer's 1×1 file) while this crate decodes it.

Reader-side behaviours the run measured (they explain every non-exact
cell below and are not this crate's defects): ffmpeg's HEIF demuxer
refuses every 1×1 file, emits the *coded* picture for files whose
`clap` it does not apply (heif-enc / magick odd sizes: the overlap is
compared), crops odd Apple / sips sizes to even, and exposes Apple's
4032×3024 grid as its first 512×512 tile only; Apple ImageIO (`sips`)
applies neither `irot` nor `imir` when rendering — third-party files
included (libheif's rotated AVIF renders 1024×722 there, 722×1024
everywhere else) — and runs its own colour pipeline (a few hundred
saturated pixels differ by up to ~100 codes, mean ≈ 1.5, on files
every other reader renders within 1 code; its 10-bit renders differ
by up to 5). `heif-enc -L` writes RGB (matrix 0) items, compared as
`gbrp`.

#### Reader direction (producer → this crate; verdict vs the black-box video decoder)

| Feature | sips (Apple ImageIO) | heif-enc x265 | heif-enc aom | magick | ffmpeg (libsvtav1) |
|---|---|---|---|---|---|
| 1×1 | decodes; no reference (ffmpeg refuses) · 4:4:4 8b | decodes; no reference (ffmpeg refuses) · 4:4:4 8b | exact · 4:2:0 8b | decodes; no reference (ffmpeg refuses) · 4:4:4 8b | producer refused (encoder: picture too small) |
| 7×5 | exact (ffmpeg even-crops to 6×4; overlap) · 4:4:4 8b | exact (ffmpeg even-crops to 6×4; overlap) · 4:4:4 8b | exact · 4:2:0 8b | exact (ffmpeg even-crops to 6×4; overlap) · 4:4:4 8b | exact · 4:2:0 8b |
| 63×61 | Δ max 50 mean 0.86 (ffmpeg even-crops to 62×60; overlap) · 4:4:4 8b | Δ max 41 mean 0.80 (ffmpeg even-crops to 62×60; overlap) · 4:4:4 8b | exact · 4:2:0 8b | Δ max 41 mean 0.79 (ffmpeg even-crops to 62×60; overlap) · 4:4:4 8b | exact · 4:2:0 8b |
| 96×80 | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b |
| 4032×3024 (12 MP) | exact (ffmpeg emits the first 512×512 tile; that region) · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b |
| 8-bit | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b |
| 10-bit | exact · 4:2:0 10b | exact · 4:2:0 10b | exact · 4:2:0 10b | exact · 4:2:0 10b | exact · 4:2:0 10b |
| 12-bit | no producer (no such option) | exact · 4:2:0 12b | exact · 4:2:0 12b | exact · 4:2:0 12b | no producer (no such option) |
| 4:0:0 | dropped (wrote 4:2:0 8b) · 4:2:0 8b | exact · 4:0:0 8b | exact · 4:0:0 8b | dropped (wrote 4:2:0 8b) · 4:2:0 8b | no producer (no such option) |
| 4:2:0 | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b |
| 4:2:2 | no producer (no such option) | exact · 4:2:2 8b | exact · 4:2:2 8b | exact · 4:2:2 8b | no producer (no such option) |
| 4:4:4 | exact · 4:4:4 8b | exact · 4:4:4 8b | exact · 4:4:4 8b | exact · 4:4:4 8b | no producer (no such option) |
| alpha | exact (alpha exact) · 4:2:0 8b +α | exact (alpha exact) · 4:2:0 8b +α | exact (alpha exact) · 4:2:0 8b +α | exact (alpha exact) · 4:2:0 8b +α | no producer (no such option) |
| lossless | no producer (no such option) | exact · 4:4:4 8b | exact · 4:4:4 8b | exact · 4:4:4 8b | no producer (no such option) |
| thumbnail | no producer (no such option) | exact · 4:2:0 8b | exact · 4:2:0 8b | no producer (no such option) | no producer (no such option) |
| Exif | exact · 4:2:0 8b | no producer (no such option) | no producer (no such option) | no producer (no such option) | no producer (no such option) |
| XMP | no producer (no such option) | no producer (no such option) | no producer (no such option) | no producer (no such option) | no producer (no such option) |
| ICC | exact · 4:2:0 8b | no producer (no such option) | no producer (no such option) | exact · 4:2:0 8b | no producer (no such option) |
| irot | exact · 4:2:0 8b | no producer (no such option) | no producer (no such option) | no producer (no such option) | no producer (no such option) |
| imir | dropped (no imir property (pixels mirrored)) · 4:2:0 8b | no producer (no such option) | no producer (no such option) | no producer (no such option) | no producer (no such option) |
| clap | dropped (no clap property (pixels cropped)) · 4:2:0 8b | no producer (no such option) | no producer (no such option) | no producer (no such option) | no producer (no such option) |
| sequence | exact · track yuv420p | no producer (no such option) | no producer (no such option) | no producer (no such option) | exact · track yuv420p |
| gain map | no producer (no such option) | no producer (no such option) | no producer (no such option) | no producer (no such option) | no producer (no such option) |

#### Writer direction (this crate → readers; render vs our decode, 8-bit units)

| Written shape | sips | heif-convert | magick | ffmpeg | heif-info |
|---|---|---|---|---|---|
| hevc 1×1 | exact | exact | exact | refused (its HEIF demuxer yields no frame for 1×1; every producer's 1×1 HEIC is refused too) | opens |
| av1 1×1 | exact | exact | exact | exact | opens |
| hevc 7×5 | exact | Δ max 1 mean 0.02 | Δ max 1 mean 0.02 | Δ max 1 mean 0.44 (renders 6×4; overlap compared) | opens |
| av1 7×5 | exact | exact | exact | exact | opens |
| hevc 63×61 | Δ max 1 mean 0.39 | Δ max 1 mean 0.04 | Δ max 1 mean 0.04 | Δ max 2 mean 0.28 (renders 62×60; overlap compared) | opens |
| av1 63×61 | Δ max 1 mean 0.24 | exact | exact | exact | opens |
| hevc 96×80 | Δ max 90 mean 1.53 | Δ max 1 mean 0.03 | Δ max 1 mean 0.03 | Δ max 1 mean 0.07 | opens |
| av1 96×80 | Δ max 1 mean 0.25 | exact | exact | exact | opens |
| hevc 4032×3024 grid (12 MP) | Δ max 101 mean 0.28 | Δ max 1 mean 0.05 | Δ max 1 mean 0.05 | Δ max 1 mean 0.06 | opens |
| av1 4032×3024 grid (12 MP) | Δ max 1 mean 0.24 | exact | exact | Δ max 1 mean 0.00 | opens |
| hevc lossless (pcm) | Δ max 94 mean 1.43 | Δ max 1 mean 0.03 | Δ max 1 mean 0.03 | Δ max 1 mean 0.07 | opens |
| av1 lossless | Δ max 1 mean 0.25 | exact | exact | exact | opens |
| hevc 4:0:0 (grey) | exact | exact | exact | exact | opens |
| av1 4:0:0 (grey) | exact | exact | exact | exact | opens |
| av1 4:4:4 10-bit | Δ max 94 mean 1.44 | Δ max 1 mean 0.03 | Δ max 1 mean 0.03 | Δ max 1 mean 0.07 | opens |
| hevc alpha | Δ max 90 mean 1.53 | Δ max 1 mean 0.03 | Δ max 1 mean 0.03 | Δ max 1 mean 0.07 | opens |
| av1 alpha | Δ max 1 mean 0.25 | exact | exact | exact | opens |
| hevc thumbnail | Δ max 94 mean 1.43 | Δ max 1 mean 0.03 | Δ max 1 mean 0.03 | Δ max 1 mean 0.07 | opens |
| hevc Exif+XMP+ICC | Δ max 94 mean 1.43 | Δ max 1 mean 0.03 | Δ max 1 mean 0.03 | Δ max 1 mean 0.07 | opens |
| hevc irot | opens; renders 96×80: rotation not applied | Δ max 1 mean 0.03 | Δ max 1 mean 0.03 | Δ max 1 mean 0.07 | opens |
| hevc imir | Δ max 254 mean 86.60 | Δ max 1 mean 0.03 | Δ max 1 mean 0.03 | Δ max 1 mean 0.07 | opens |
| hevc clap | Δ max 92 mean 1.37 | Δ max 1 mean 0.02 | Δ max 1 mean 0.02 | Δ max 1 mean 0.06 | opens |

Every reader-direction cell that is not `exact` is explained by the
producer (`no producer`, `dropped`) or by the reference decoder (the
1×1 refusals; the 63×61 4:4:4 HEIC files, where ffmpeg's even-cropped
output differs from ours by up to 50 codes at scattered pixels while
the producers' own PNG renders of the same files match this crate
within 1 code — `tests/interop.rs`, vendored oracles). Every writer
direction cell opens; the non-exact `sips` cells are Apple ImageIO's
own colour pipeline on 4:2:0 HEVC (scattered saturated pixels, mean ≈
1.5 codes; the same pictures coded as AV1 4:4:4 render within 1) and
its non-application of `irot` / `imir` (the `imir` row is the
un-mirrored picture compared with our mirrored decode). Features no
tool here produces — XMP items, gain maps (`avifgainmaputil` is a
converter, not a still-image encoder), 12-bit / 4:2:2 / lossless from
Apple ImageIO, AVC / L-HEVC items — are covered by this crate's own
writer round-trips (`tests/gainmap.rs`, `tests/avc_lhevc.rs`,
`tests/writer.rs`) and, for gain maps, the vendored oracle files.


## Performance (r463; baseline r462)

`examples/heifbench` (`cargo run --release --example heifbench --
[--threads N|auto] <files>`), reference machine: Apple M4 Max (16
cores), macOS 26.6, rustc 1.98.1, release build, medians of 5 runs. The
three inputs are the matrix run's 4032×3024 (12 MP) files: Apple
ImageIO's 512-px `grid` HEIC (48 tiles + `grid`), `heif-enc` x265's
single-item HEIC and `heif-enc` aom's single-item AVIF, all 8-bit 4:2:0
of the same synthetic picture. Decode = `HeifFile::parse` +
`decode_primary` (direct factories) under an `ExecutionContext` of the
given thread budget; peak RSS = maximum resident set size of a fresh
process decoding once. The output FNV-1a is the byte-identity gate: it
did not move between r462 and any r463 configuration.

| File | r462 decode / RSS | r463 serial | 2 threads | 4 threads | 8 threads | 16 threads |
|---|---|---|---|---|---|---|
| Apple grid HEIC (49 items) | 0.330 s / 89 MiB | 0.326 s / 52 MiB | 0.180 s / 61 MiB | 0.100 s / 69 MiB | 0.058 s / 88 MiB | 0.063 s / 130 MiB |
| single-item HEIC | 0.294 s / 290 MiB | 0.298 s / 290 MiB | — | — | — | — |
| single-item AVIF | 0.778 s / 229 MiB | 0.709 s / 226 MiB | — | — | — | — |

What changed (phase 1, container side): grid / `tili` / `cexg` tiles
decode as independent jobs on up to `effective_workers` threads (codec
instances made on the workers, each serial) and are written straight
into an output-size canvas as they complete; reconstructions move out
of the codec's planes when they are tight, an untransformed item is
not copied, and coded items are cached only while another use in the
graph follows. The serial grid decode holds the canvas plus one tile
(89 → 52 MiB); the single-item cases are the codecs' own work and
buffers (a 12 MP HEVC still decodes in 0.30 s with a 290 MiB peak
inside oxideav-h265, AVIF 0.71 s / 226 MiB inside oxideav-av1) — the
container adds no copies any more. Handing a single still's codec the
thread budget measured slower (HEVC 0.30 → 0.34 s), so the budget is
spent on independent items only. Encoding is unchanged from r462: the
HEVC intra encoder ~2.7 s, the AV1 still encoder ~90 s at `fast` for
12 MP (codec-crate items).

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
