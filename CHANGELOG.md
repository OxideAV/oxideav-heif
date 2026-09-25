# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

- Alpha auxiliary tracks in image sequences (HEIF §7.5.3):
  `Movie::auxiliary_tracks_of` / `alpha_track_of`, `Track::aux_kind`
  / `sample_index_at` (the time-parallel sample); the framework
  demuxer adds a composed `"heif"` stream per master track that has
  an alpha track — each packet a synthesized single-image HEIF
  (master sample + time-parallel alpha sample, `auxl` + `auxC`) so
  the `"heif"` codec yields frames with alpha under the still-image
  alpha rules (resize / depth match). `SequenceWriter::alpha`
  (`SequenceAlphaTrack`) writes the `auxv` track with its `auxl`
  reference and `auxi` FullBox. `auxi` written as a plain Box (Apple
  ImageIO; libheif 1.23.4 refuses those files) is read too. New
  producer fixture `sips_seq_rgba_96x80.heics` pinned to the
  black-box decoder's raw planes of both tracks.

- `avc1` items (HEIF Annex E): typed `AvcConfig` (`avcC`, ISO/IEC
  14496-15 §5.3.2.1 incl. the High-family trailer) and decoding
  through oxideav-h264 (`"h264"`, record as `extradata`,
  length-prefixed access unit) — directly, through a registry, the
  framework demuxer and the `"heif"` codec; `avc_item_from_annex_b`
  builds the record + item data (size from the SPS crop) from an
  Annex B access unit. Items built from a black-box AVC encoder
  (Baseline / Main / High / High 4:2:2 / High 4:4:4 / High 10)
  decode byte-exact against that encoder's decoder; libheif, Apple
  ImageIO and ImageMagick open the files, ffmpeg's HEIF demuxer
  refuses them. `avc1` / `avc3` sample entries carry `avcC`.
- `lhv1` items (HEIF B.2.2.1.3): typed `LhevcConfig` (`lhvC`,
  14496-15 §9.5), `OperatingPoints` (`oinf`, §9.6.2.2: PTLs,
  operating points with output layers, layer dependencies) and `tols`
  properties; the base layer (`nuh_layer_id` 0, parameter sets and
  access unit filtered, Annex B) decodes through oxideav-h265; an
  output layer set with enhancement layers is the typed refusal
  `HeifError::layered_hevc(item_id, target_ols_idx)` (an
  `Unsupported`; `layered_hevc_info()` reads it back) unless
  `ItemDecoder::base_layer_fallback()`. `lhv1` / `hvc2` sample
  entries carry `lhvC`.
- New optional dependency `oxideav-h264 = "0.1"` under `registry`.

- `props` / `sequence`: `clli` / `mdcv` / `cclv` / `amve` follow ISO/IEC
  14496-12 8th ed. §12.1.6–9 exactly — plain Boxes on the wire (`cclv`
  and `amve` were written as FullBoxes; a FullBox prefix is still
  accepted on read, `cclv` by its flag-derived length), interleaved
  `(x, y)` primaries, wire bytes pinned by tests; the four boxes are
  typed on `SampleEntry` (`clli` / `mdcv` / `cclv` / `amve`) for image
  sequences, and `SequenceWriter::entry_properties` round-trips them.

- `tmap` per ISO/IEC 23008-12:2025/Amd 1 §6.6.2.4 + §10.2.6 (the
  staged DAM text), replacing the empirical rules: `ToneMapImage`
  body = `version` (shall be 0; others refused) + C.2 metadata (the
  bare-C.2 fallback is gone); `dimg` `reference_count` 2 enforced in
  the graph; the single↔multi-channel gain-map rules and the
  limited-range clip in `apply_gain_map`; `check` reports the brand
  (present ⇔ a `tmap` item), the input pair, the base / gain-map /
  `tmap` `colr` placements (gain map: nclx with primaries = transfer
  = 2), the body version and mixed-hidden `altr` groups as
  `"HEIF-A1 …"` clauses; `BRAND_TMAP`.
- `decode`: `ToneMapOutput` — decoding a `tmap` item yields the base
  (default; with the base's `nclx`, previously the tmap's) or, with
  `ItemDecoder::tone_mapped()`, the normative reconstruction
  (`gainmap::reconstruct_tone_map`: the map applied at the alternate
  headroom, re-encoded in the `tmap` item's `colr` at its `pixi`
  depth, base alpha carried over); a `tmap` that is the input of
  another derived item is always the applied image. PQ alternates are
  anchored at `DEFAULT_HDR_REFERENCE_WHITE_NITS` (203, the 21496-1
  3.6 example; `with_reference_white` overrides), relative transfers
  at `2^H_alternate` × reference white (`alternate_signal_scale`).
- `writer` / `encode`: `HeifWriter::add_tone_map` (hidden gain map,
  essential alternate `colr`, base-sized `ispe`, `idat` body, `tmap`
  brand — also under a brand override — and the `altr` [tmap, base]
  group) and `EncodeOptions::gain_map: Option<GainMapSpec>` (map
  picture + metadata + alternate `colr` / `clli` / `pixi` hint; the
  map is coded hidden with an `nclx` of primaries = transfer = 2). A
  black-box gain-map tool tone-maps our authored AVIF identically to
  the file it was rebuilt from; the HEVC form opens in the readers.
- `rgb::from_rgb`: RGB(A) → planar 4:4:4 / monochrome YCbCr with the
  `colr` matrix / range (the inverse of `to_rgb`).

- `file`: zero-copy parsing, additively — `HeifFile` is now
  `HeifFile<D = Vec<u8>>`; the bare name keeps every 0.0.3 signature
  (`parse(&[u8])` copies, `from_vec`, `into_bytes`, …) and
  `HeifFile::parse_borrowed(&'a [u8]) -> HeifFileRef<'a>`
  (`HeifFile<&[u8]>`, `into_owned`) is the view that borrows the
  caller's buffer; every reader (`item_data`, `build_graph`, `check`,
  `parse_movie`, `sample_bytes`, `decode_item` / `decode_primary`) is
  generic over `D: AsRef<[u8]>` so both forms take the same paths.
  The `"heif"` codec and the sequence writer use the borrowed form.
- `error`: the L-HEVC enhancement-layer refusal is an
  `HeifError::Unsupported` built by `HeifError::layered_hevc(item_id,
  target_ols_idx)` and read back with `layered_hevc_info()` — the
  enum gains no variant (consumers match it exhaustively).
- `props`: `mdcv` and `cclv` primaries are read and written
  interleaved per primary (`x, y` pairs, ISO/IEC 14496-12 §12.1.7 /
  §12.1.8); they were planar (`x x x y y y`). Bytes pinned by a test.
- `compose`: a quarter turn (`irot` 1 / 3) of a 4:2:2 picture promotes
  to 4:4:4 (MIAF §7.3.6.7) instead of keeping a 4:2:2 label over
  vertically subsampled planes; a monochrome overlay stays monochrome
  even with alpha-carrying inputs (nothing to promote). The demuxer's
  output prediction mirrors both.
- `writer`: `SequenceWriter::brands` / `with_brands` (brand override)
  and `cover_sample` (the cover still's primary item aliases a track
  sample through its `iloc` extent — one copy of the bytes; the
  framework muxer now aliases sample 0); `HeifWriter::add_exif_raw`
  (body with the offset word as is), `add_xmp_bytes` (any encoding),
  `add_metadata_item` (any `cdsc` item type / content type),
  `set_item_name` (`infe` `item_name`), `add_entity_group_with_flags`;
  metadata items of any type no longer need an `ispe`.
- `meta`: `EntityGroup::version` / `flags` surfaced from the
  `EntityToGroupBox` FullBox header.
- `sequence`: `sbgp` / `csgp` (§8.9.2 / §8.9.5, patterns expanded to
  runs, fragment-local msb kept in bit 31) and `sgpd` (§8.9.3, v0–v2,
  raw entries) on `Track::sample_groups` /
  `sample_group_descriptions` with `group_description_index` /
  `group_description_of` (mapping, else the `sgpd` default); top-level
  `prft` (§8.16.5) and `ssix` (§8.16.4) on
  `Movie::producer_reference_times` / `subsegment_indexes`.

## [0.0.3](https://github.com/OxideAV/oxideav-heif/compare/v0.0.2...v0.0.3) - 2026-09-25

### Other

- doc-comment wording that clippy read as an unindented list item
- README / CHANGELOG for the producer adoption (lossy AV1, HEVC VUI + ids, minimum pins)
- HEVC VUI range/colour, Main Still Picture, alpha by parameter-set ids
- lossy AV1 stills at native layouts through av1 0.1.19
- keep const-compatible enum defaults; document the effective ones
- enum option defaults name their default value
- label full-range stills with the framework's YuvJ* layouts
- declare the "heif" encoder options schema
- accept packed RGB / RGBA / BGR(A) / 16-bit / grey+alpha input
- emit stco for 32-bit chunk offsets (ISO/IEC 14496-12 §8.7.5)
- pass a "heif" still stream through as the file

- `encode` (`registry`): AV1 items are quality-dialled
  `reduced_still_picture_header` stills at the picture's own (depth,
  chroma) pairing (8/10/12-bit, 4:0:0–4:4:4) through oxideav-av1
  0.1.19 — `EncodeOptions::av1_quality` / `av1_speed`, framework
  `quality` (default 60 for `codec=av1 mode=intra`; `mode=pcm` and
  quality 100 stay lossless), `speed`; `av1C` from the encoder's codec
  configuration; alpha as a monochrome still.
- `encode` (`registry`): HEVC items carry the item's range and H.273
  colour description in the bitstream VUI and are Main Still Picture
  access units (oxideav-h265 0.0.11); Apple ImageIO now renders our
  full-range alpha exactly. The alpha auxiliary is differentiated by
  parameter-set ids (VPS/SPS/PPS 1) — the CTB / row-band workaround is
  gone. Framework `rd` (intra effort 0..=2) and `tiles` (`CxR`) knobs.
- Minimum producers: `oxideav-h265 = "0.0.11"`, `oxideav-av1 = "0.1.19"`.

- `mux` (`registry`): a `"heif"` still stream (whole-file packets from
  the `"heif"` encoder) passes through the muxer as the file — exactly
  one packet; a second is a typed refusal. `oxideav convert in.png
  out.heic` now writes a HEIC.
- `encode` (`registry`): packed RGB / RGBA / BGR(A) / 16-bit /
  grey+alpha input (`packed_to_planar`, H.273 matrix + range of the
  output `nclx`); `range` option (`full` default / `limited`); declared
  `HeifEncoderOptions` schema (`codec` / `mode` / `qp` / `grid` /
  `thumbnail` / `range`) so `oxideav info heif` lists them and unknown
  keys are refused; capabilities list the accepted pixel formats.
- `demux` (`registry`): full-range stills (nclx `full_range`, the MIAF
  default) are announced and emitted as the framework's `YuvJ*`
  layouts (`HeifFrame::to_core_ranged`), limited-range ones as `Yuv*`.
- `writer`: `SequenceWriter` emits `stco` for 32-bit chunk offsets
  (`co64` only beyond 4 GiB); libheif / ImageMagick now open the
  written `msf1` sequences.

## [0.0.2](https://github.com/OxideAV/oxideav-heif/compare/v0.0.1...v0.0.2) - 2026-09-23

### Other

- ISO 21496-1 gain-map metadata + opt-in application (tmap)
- distinct alpha parameter sets so Apple ImageIO opens alpha files
- make the reader interop suite portable and encoder-agnostic
- nclx paired with ICC must use primaries=transfer=2 (HEIF §6.5.5)
- README interop section + capability updates; CHANGELOG
- update writer transform test for item-level placement
- real-world reader/writer interop suites
- vendor a compact fixture corpus so CI proves pixels
- writer output that opens in third-party readers
- apply iscl (image scaling) at composition (HEIF 3rd ed §6.5.13)
- YCbCr -> RGB conversion (H.273 matrices, full/limited range)
- doc(hidden) sweep: internal box-writer / walk helpers, composition and encode plumbing, muxer/writer records
- 'heif' framework Muxer for image sequences (HEVC Annex B / length-prefixed, AV1 TUs) with cover still
- capability matrix, corpus scorecard, standalone build note, migration provenance, AVIF follow-up
- parser / composition / sequence targets, seeded corpus, Fuzz workflow; package exclude
- enforce the MIAF 64-pixel tile floor when gridding (fixes the grid round-trip test)
- writer + encode: HeifWriter / SequenceWriter, HEVC + AV1 still encoding, 'heif' framework encoder
- sequence + demux + registry: moov/trak/stbl tracks, framework Demuxer, heif codec, probe priority
- use clamp in the oracle channel count (clippy manual_clamp)
- grid / overlay / identity derivations, clap/irot/imir, alpha attachment; whole-image decode driver
- coded items through oxideav-h265 / oxideav-av1, crate-local planar frame
- typed property surface, hvcC/av1C records, derivation graph, MIAF typed checks
- ISOBMFF box reader, ftyp brands, meta tree model, item payload resolution

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
