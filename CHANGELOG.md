# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

- `gainmap` (both builds): ISO 21496-1 gain maps — `GainMapMetadata`
  (C.2 payload, `tmap` version prefix, parse / serialise), H.273
  transfer + primaries helpers, `apply_gain_map` (Formulas 1–3, §6.2.2
  resampling, Annex B application space) → `LinearRgbImage` with
  `encode`; `DecodedImage::gain_map` attaches the decoded gain-map
  item (from a `tmap` whose first input is the image, or the `tmap`
  itself) and `DecodedImage::apply_gain_map(h_target)` applies it on
  request. The default output stays the baseline image.
- `encode` (`registry`): the alpha auxiliary is coded with parameter
  sets distinct from the master's (CTB 32 for CABAC intra; an extra
  clapped 16-row band for PCM) — Apple ImageIO refuses two items with
  byte-identical VPS / SPS / PPS; it now opens every written alpha file.

- `rgb` (both builds): `to_rgb` / `RgbImage` — YCbCr → RGB(A) of a
  composed frame with the item `colr` matrix / range (H.273), the
  identity (GBR) matrix, monochrome and any alpha plane; the renderer
  step decode / compose previously left to the caller.
- `compose` (both builds): apply `iscl` (image scaling, HEIF 3rd ed
  §6.5.13) at composition — output `ceil(input × num / den)` per axis
  with an area / bilinear resampler (`resize_area`) run per plane; the
  demuxer predicts the scaled output size. Was parsed but refused.
- `encode` (`registry`): writer output that opens in third-party
  readers — transformative properties as essential properties on the
  displayed coded item(s) instead of a hidden `iden` wrapper; alpha
  items carry no `colr`, a single-channel `pixi` and an essential
  `auxC` (HEVC `urn:mpeg:hevc:2015:auxid:1`, AV1 CICP URN); one `hvcC`
  per coded item (Apple ImageIO refuses a shared decoder config).
- `tests`: a vendored fixture corpus under `tests/fixtures/` (kept out
  of the package by `exclude`) so the decode / e2e / trace tests run on
  CI, plus `tests/interop.rs` (39 real-world files from Apple ImageIO /
  libheif / ImageMagick decode byte-exact against a black-box video
  decoder where layouts coincide, and within rounding against a
  black-box HEIF reader) and `tests/writer_interop.rs` (our output
  opened by `sips` / `heif-convert` / `magick` / `ffmpeg` / `heif-info`).
- `examples`: `heifdump` (decode a HEIF / AVIF file to PNG / raw / box
  tree) and `heifenc` (write a still in any shape) — for feeding
  third-party readers.

- `mux` (`registry`): `HeifSequenceMuxer` — the `"heif"` framework
  `Muxer`: HEVC packets (Annex B from the oxideav encoder, or `hvcC` +
  length-prefixed) or AV1 temporal units in, an `msf1` image-sequence
  file out (`pict` track, `hvcC` / `av1C` + `ccst`, sample table with
  durations and sync flags, first sync sample as the cover-image
  `meta` still); registered next to the demuxer. Round trip (encode →
  mux → demux → decode) is pixel-exact and MIAF-conformant.

- Fuzz sub-crate (`fuzz/`, standalone build): `heif_parse` (box walk,
  meta tree, item resolution across every construction method, typed
  properties, derivation graphs, MIAF checks), `heif_compose` (grid /
  overlay descriptors + pixel composition on synthetic tiles, then the
  clap / irot / imir chain) and `heif_sequence` (moov / trak / stbl
  walk + sample resolution); seeded with this crate's own writer
  output; daily `Fuzz` workflow. 150 s per target locally: no findings.
- `Cargo.toml`: `exclude = ["/tests", "/fuzz"]` for the crates.io package.

- `writer`: `HeifWriter` — MIAF-conformant still files from coded
  payloads (coded / `grid` / `iovl` / `iden` items, thumbnails, alpha
  and depth auxiliaries with `auxC` + `prem`, Exif / XMP items, entity
  groups, de-duplicated `ipco`, `ipma` essential flags, `iloc` v1/v2
  with cm 0 / 1, `infe` v2/v3, MIAF §7.2.1.13 `mdat` ordering,
  brand auto-selection); `SequenceWriter` — `msf1` / `hevc` image
  sequences (`pict` track, `hvc1` / `av01` entry with `ccst`, sample
  table, optional cover-image `meta`).
- `encode` (`registry`): `encode_still` — pixels → HEVC items through
  `oxideav-h265` (Annex B → `hvcC` + length-prefixed AU; `pcm`
  lossless or `intra` at a QP) or AV1 items through `oxideav-av1`
  (lossless key frame → `av1C` from the sequence header); padding +
  `clap` for unaligned sizes, `grid` tiling, thumbnails, alpha
  auxiliary, Exif / XMP, ICC, transformative properties as an `iden`
  item; `HeifEncoder` — the `"heif"` framework encoder (frame in,
  complete file out; options `codec` / `mode` / `qp` / `grid` /
  `thumbnail`), registered alongside the decoder.
- Tests: HEVC-lossless and AV1 round trips are pixel-exact, odd sizes
  round-trip through `clap`, grid + thumbnail + alpha + metadata
  round trip, transforms via `iden`, framework encoder → decoder, and
  a black-box decoder reproduces the written file byte-exact.

- `sequence`: `moov` / `trak` / `stbl` walk — `mvhd` / `tkhd` (flags,
  §7.2.1 matrix → rotation / mirror) / `mdhd` / `hdlr` / `stsd` visual
  sample entries (`hvcC`, `av1C`, `ccst`, `auxi`, `colr`, `clap`,
  `pasp`) / `stts` / `ctts` (v0/v1) / `stsc` / `stsz` + `stz2` /
  `stco` + `co64` / `stss` / `tref` / `elst`, bounded expansion.
- `demux` (`registry`): `HeifDemuxer` — stream 0 = the still image as
  one `"heif"` keyframe packet (predicted output geometry / pixel
  format in the stream parameters), one stream per visual track with
  the codec id resolved from the sample-entry type, `hvcC` / `av1C`
  extradata, pts / dts / duration / sync flags, `seek_to` on sync
  samples, `set_active_streams`, brand + track metadata; `HeifCodec`
  — the `"heif"` decoder (whole file in, composed primary out).
- `registry` (`registry`): `register` + `oxideav_core::register!`
  entry point; probe on the HEIF-family brands at a priority below the
  MP4 / MOV demuxers' so HEIF brands win ties while generic `isom` /
  `qt  ` files stay with them; `.heic` / `.heif` / `.heics` / `.heifs`
  / `.hif` / `.avif` / `.avifs` hints; the `"heif"` codec.
- Tests: sequence sample table, the three track frames byte-exact
  against a black-box decoder and within tolerance of the per-frame
  oracles; probe → open → packet → decode of every bundle through a
  `RuntimeContext`; probe-priority ordering against a generic `ftyp`
  prober.

- `compose`: pixel composition — `grid` (row-major tiling, right/bottom
  trim, tile alpha), `iovl` (sRGB `canvas_fill_value` converted with
  the H.273 matrix of the output `colr`, per-input offsets with
  clipping, §6.9.1 straight / pre-multiplied alpha blending, canvas
  opacity → output alpha for translucent fills), `iden`, `clap`
  (exact rational aperture), `irot`, `imir`, alpha auxiliary
  attachment (resize + depth match). Sub-sample chroma positions
  (odd crops / offsets / tile sizes / rotations, alpha edges) promote
  to 4:4:4 per the MIAF §7.3.6.7 rule.
- `decode` (`registry`): `decode_item` / `decode_primary` →
  `DecodedImage` (output image with alpha plane, depth auxiliary,
  effective `nclx` incl. the MIAF default, ICC, Exif with the offset
  word resolved, XMP, thumbnail ids, properties); shared inputs decode
  once; decode-count cap.
- Tests: all 14 corpus bundles against their `expected.png` oracle
  (five sample-exact, the rest within 8-bit colour-conversion noise),
  grid composition byte-exact against a black-box decoder, burst items,
  thumbnails / Exif / XMP / ICC surfacing.

- `image`: crate-local planar `HeifFrame` / `HeifPixelFormat` (4:0:0 /
  4:2:0 / 4:2:2 / 4:4:4 at 8–16 bits, optional alpha plane), bridged to
  `oxideav_core::VideoFrame` + `PixelFormat` under `registry` (with a
  neutral-chroma 4:4:4 promotion for layouts the framework lacks).
- `decode` (`registry`): `ItemDecoder` — `hvc1` / `hev1` items through
  `oxideav-h265` (`hvcC` extradata + length-prefixed access unit) and
  `av01` items through `oxideav-av1` (`av1C` extradata + temporal unit),
  via the direct factories or a caller-supplied `CodecRegistry`;
  decoded pictures are validated against the announced layout and
  cropped to `ispe`.
- Tests: every corpus coded item decodes to its announced layout; the
  nine plain-coded primaries match a black-box decoder byte-exact
  (8-bit 4:2:0, monochrome, Main 10, 4:4:4).

- Property surface (`props`): typed `ispe`, `pixi`, `colr` (nclx / ICC),
  `pasp`, `clap` (exact rational aperture resolution), `irot`, `imir`,
  `iscl`, `auxC` (both alpha / depth URN families), `hvcC`, `av1C`,
  `lhvC` / `avcC` (raw), `clli`, `mdcv`, `cclv`, `amve`, `rloc`, `lsel`,
  `a1op`, `a1lx`, `rref`, `crtt`, `mdft`, `udes`, `altt`; §6.5.1
  descriptive-before-transformative semantics, essential-unknown
  detection, transformative chain output size, box serialization.
- `hvcc`: standalone `HEVCDecoderConfigurationRecord` parse / byte-exact
  reserialize, NAL arrays, length-prefixed NAL split / join, Annex B
  split. `av1c`: `AV1CodecConfigurationRecord` parse / serialize.
- `derived`: `grid` / `iovl` descriptors (16/32-bit fields), `iden`, and
  the bounded derivation graph (`dimg` / `auxl` / `thmb` / `cdsc`, cycle
  detection, depth / fan-out / node / canvas limits).
- `miaf`: `MiafProfile` + `check` — §7 general requirements (brands,
  handler, primary item role, construction methods, protection,
  transformative essential / order / set, colr pairing, thumbnail
  ladder, derivation chain order, grid tile rules, overlay input
  agreement), §8 shared constraints, Annex A HEVC / AV1 codec limits.
- Corpus tests: `HVCC` / `HEVC_FRAME_FOR_ITEM` trace equivalence,
  per-bundle property expectations, graph shapes, MIAF conformance.

- ISOBMFF box reader (`boxes`): bounds-checked headers (`size` 0 / 1 /
  `largesize` / `uuid`), FullBox prefix, `Reader` cursor, bounded
  recursive box walk, writer helpers.
- `ftyp` / `styp` brands (`ftyp`): the HEIF structural + HEVC + MIAF +
  AV1 brand vocabulary, `BrandClass` classification, content probe.
- `meta` tree model (`meta`): `hdlr`, `pitm` (v0/v1), `iinf` (v0/v1) +
  `infe` (v2/v3 with `mime` / `uri ` tails), `iloc` (v0–v2, all field
  widths, construction methods 0/1/2), `iref` (v0/v1), `iprp` /
  `ipco` / `ipma` (v0/v1, 7/15-bit indices, essential flag, index-0
  placeholders, duplicate-key rejection), `idat`, `grpl`, `dinf/dref`,
  `ipro`, with per-item property / reference resolution helpers.
- `HeifFile` (`file`): whole-file parse, item payload resolution for
  every construction method with multi-extent concatenation,
  zero-length "to end" extents, bounded `iloc`-reference chains and
  self-reference rejection, size caps.
- Corpus trace-equivalence test over all 14 staged bundles (BOX / PITM
  / ITEM_INFO / IREF / IPRP_PROP / IPRP_ASSOC events).

- Bootstrap scaffold (2026-09-18).
