# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- **AVC items and sequences take every High-family layout** (round 476, oxideav-h264 0.1.9): `StillCodec::Avc` / the framework `codec=avc` code 4:2:0 / 4:2:2 / 4:4:4 sources at 8 / 10 / 12 bits as they are (the §A.2 minimum High-family profile; Baseline / Main CAVLC / CABAC on 8-bit 4:2:0 as before, byte-identical), `mode=pcm` is lossless (High 4:4:4 Predictive transform bypass; lossless RGB as an identity-matrix 4:4:4 item, like HEVC), packed RGB(A) converts at `chroma`, planar RGB under a lossy mode through the matrix, 16-bit sources to 10 (`depth=12` for 12). A 4:0:0 source (planar grey, packed grey + alpha) is coded 4:2:0 with neutral chroma — the registry encoder's grey line — so grey AVC items come back 4:2:0-labelled. The encoder runs `format=avcc`: its `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15 §5.3.2.1, with the chroma-format / bit-depth trailer for the High-family profiles) is the item's `avcC` and its length-prefixed sample the `AVCItemData`; `avc_item_from_annex_b` / `avc_sample_from_annex_b` (the muxer's Annex B input) build the record with the codec crate's `avcc` module. `EncodeOptions::avc_coded_format`; declared framework option `cabac` (`HeifEncoderOptions::cabac` / `with_cabac`).
- **VVC items take every layout the h266 0.0.10 encoder codes** (round 476): 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 at 8 / 10 bits (`EncodeOptions::vvc_coded_format`: the source's sampling at the coded depth capped at `VVC_MAX_DEPTH` = 10; an explicit `depth=12` is a typed refusal), lossless under `mode=pcm` (transform-skip coding at `Qp′ = QpPrimeTsMin`; lossless RGB as an identity-matrix 4:4:4 item), packed RGB(A) at `chroma`, packed grey + alpha as 4:0:0, and any picture size: the encoder pads the coded picture to its own 8-sample grid and crops with the SPS conformance window, so this crate's 64-sample padding is gone (`VVC_ALIGNMENT` is 2 — the window is in chroma units, an odd edge goes through `clap` as on every codec; the `vvcC` `max_picture_*` carry the coded size, the `ispe` the item's). `tests/vvc.rs`: every layout × lossy / lossless round-trips byte-exact in this crate's decoder and in the black-box reference decoder, the size ladder (1×1, 7×5, 320×192, …) with the new coded-size rule, `pcm` exact.
- `tests/framework_matrix.rs`: 553 → 724 cells (AVC `pcm` on every layout and size, `qp4` / `qp30` / `qp40` / `thumbnail16` / `limited` / `pcm+limited` / `cabac` / `pcm+cabac` / `depth8|10|12` / `chroma444` / `pcm+chroma444` / `threads2` × six layouts, `pcm` grids; VVC `pcm` on every layout family and size, `depth10` / `depth12` / `chroma444` / `pcm+chroma444` / `limited` / `pcm+limited`), 723 coded, 1 typed refusal (VVC `depth=12`), 0 failures; a black-box leg decodes every `avc1` item of the 230 AVC files through this crate's AVC decoder and the black-box reference decoder, byte-identical; Apple ImageIO opens all of them (the deep and lossless ones included).

- **AV1 items decode under the thread budget** (round 476, oxideav-av1 0.1.21): `ItemDecoder` hands its budget (`with_threads` / `with_execution_context`, `DecodeOptions::threads`, the `"heif"` decoder's `set_execution_context` and `threads` option) to the AV1 codec instance of a single coded item, a layered item and a track (`make_budgeted`; the parallel grid-tile path keeps one serial instance per worker) — the decoder walks its tiles in parallel and runs the post-filters by row band, byte-identical at any budget. 12 MP single-item AVIF (this machine): 0.456 s serial → 0.246 / 0.147 / 0.102 s on 2 / 4 / 8 workers, same output digest; the 48-tile grid AVIF 0.446 → 0.075 s as before. HEVC stays serial (a single-item 12 MP HEIC measured 0.438 → 0.415 s under the budget — not granted). `tests/encode_budget.rs` pins byte identity at 1 / 2 / 3 / 4 / 8 workers on a 4×2-tile item through the direct decoder, `DecodeOptions::threads` and the framework decoder; the decode matrix runs its framework side under an 8-worker context against the serial contract path over every corpus file. `examples/heifbench`: `--threads`, `--grid none|<tile>`, `--write <out.avif|out.heic>`.

- **`.avif` encoding through the gateway** (round 476, gateway contract): the `"heif"` muxer accepts a still stream whose codec id is `avif` — oxideav-avif's own still encoder, the one owner of that codec id — exactly like a `heif` one (one composed, already-branded file per packet, written through unchanged; `demux::CODEC_ID_AVIF` names it). This crate registers the `heif` codec id only (its capabilities now flag lossy + lossless); a gateway writes `.avif` through oxideav-avif's encoder and this muxer, `.heic` through the `heif` encoder (`tests/gateway.rs`).
- **`quality` governs every codec** (round 476, gateway contract): on HEVC / VVC / AVC, when `qp` is not given, `quality` 0..=100 maps to `qp = round((100 − quality) × 51 / 100)` (`encode::qp_of_quality`: 90 → 5, 50 → 26, 0 → 51) and `quality` 100 is the lossless `mode=pcm` unless `mode` is given; an explicit `qp` wins; with neither given the bytes are exactly as before (qp 18 / AV1 quality 60). On AV1 an explicit `quality` < 100 without a `mode` codes `mode=intra` at that quality, 100 the lossless coding (`mode=pcm`'s bytes), absent today's 60. Gate (`tests/gateway.rs`, through `oxideav_core`'s registry): bytes shrink and PSNR falls from quality 100 to 90 to 50 on every codec (16×16 `Yuv420P` / `Yuv444P` / `Rgb24` frames with `codec=av1` included — an `Rgb24` frame at quality 50 comes back as lossy YCbCr, at 100 as the lossless identity `Gbrp8` item), 100 is lossless on HEVC / VVC / AVC / AV1.
- **Metadata blobs on the framework path** (round 476, oxideav-core 0.1.40): the `"heif"` demuxer announces each still stream's ICC profile / Exif payload / XMP packet (its first item's — what `info()` reports for the primary) and each track stream's ICC profile (the sample entry's `colr`) as `CodecParameters::blobs` under the well-known `BlobKind::ICC` / `EXIF` / `XMP`; the `"heif"` decoder puts every frame's own item metadata on the `VideoFrame` as its blobs (bursts: per item), exactly the contract `HeifImage::metadata` — `HeifImage::into_video_frame` / `from_video_frame` carry them both ways (`blobs_of_metadata` / `metadata_of_blobs`, the frame's blobs refining the stream's per kind). The `"heif"` encoder embeds the frame's blobs refined over the stream's as the file's `colr` / `Exif` / `mime` items (unless the options carry their own), byte-identical to the contract encode with that `Metadata`; the sequence muxer writes the stream's ICC profile as the sample entry's and the cover item's `colr` and its Exif / XMP as items describing the cover image. The decode matrix gains a metadata dimension (every frame's metadata equals the contract image's; the stream's blobs are its first item's), the encoder matrix a round trip across the codecs and input routes, `tests/mux.rs` the muxer's.

### Changed

- **producer adoption (round 476)** — `oxideav-h264 0.1.9`, `oxideav-h265 >=0.0.15`, `oxideav-h266 >=0.0.10`, `oxideav-av1 0.1.21`, `oxideav-core 0.1.40` (the crate manifest and the fuzz sub-crate).
- AVC image sequences: the H.264 §7.4.3 `idr_pic_id` rotation this crate applied to consecutive IDR samples (`encode::avc_rotate_idr_pic_id`, the muxer's `rotate_idr`) is gone — the registry encoder rotates a per-session counter itself since 0.1.9. `tests/avc_lhevc.rs` codes a three-frame `avcs` sequence as an IDR + P session and as three IDR access units (`avc_options` `gop=1`, and the `"heif"` muxer over an all-IDR `"h264"` stream): three sync samples, three distinct pictures in this crate's decoder and in the black-box reference decoder, byte-identical.

### Fixed

- fuzz: the two inputs the daily workflow crashed on (an HEVC item reaching a deblocking-filter index panic inside h265 0.0.14 — fixed by 0.0.15's hostile-input hardening; a `cfen` item without inputs — fixed in 0.0.11) are tracked as `minimal.*` seeds of `heif_decode` / `heif_framework` / `heif_records`. On the round's final state every target ran ≥ 10 minutes on its committed corpus (2 GiB RSS cap, 30 s per-input timeout) without a finding — `heif_parse` 5.98 M runs, `heif_compose` 8.13 M, `heif_sequence` 4.19 M, `heif_records` 9.77 M, `heif_api` 2.11 M, `heif_decode` 0.21 M, `heif_framework` 1.13 M — and the daily Fuzz workflow is green again on the released producers.

## [0.0.11](https://github.com/OxideAV/oxideav-heif/compare/v0.0.10...v0.0.11) - 2026-10-14

### Other

- the still streams stay first whatever the major brand (revert the 9b3e765 reorder) — the composed primary still is video stream 0, the gateway's primary() / max_frames(1) convention; the tracks follow
- mini expansion refuses 'uuid' codec types (fuzz-found: the equivalent file carried a uuid property box without an extended type); heif_records' packed-channel oracle composes under a YCbCr description
- cfen region planner refuses an item without inputs (fuzz-found index panic); fuzz corpus seeds for the two h265 0.0.14 recon crashes and the framework target; fuzz workflow lists seven targets
- sequence-brand files list their track streams first (the CLI handed the MP4 muxer the cover still); README framework surface — matrix counts, per-codec limits, 12 MP CLI budgets each direction, black-box findings
- framework surface proved cell by cell — 553-cell encoder matrix + decode matrix over every file, planar sources keep their chroma on HEVC, several still streams, decoder options, altr fallback, heif_framework fuzz target
- README — the r474 round-trip table replaces the r464 one (the AVIF rows moved with av1 0.1.20; HEIC rows identical)
- conformance matrix refreshed (r474) — every r464 cell unchanged; this crate's AVC and VVC items join the writer direction as measured rows
- AVC items write path — StillCodec::Avc through oxideav-h264's registry encoder (avc1 + avcC, avci brand, alpha, grid, thumbnails, mif3), avcs image sequences as one IDR + P session, the heif muxer takes h264 streams; fuzz lockfile untracked
- packed cfen inputs — Amd 2 §6.6.2.5.3 region planner (CfenRegionPlan), packer, writer entries, MIAF duplicate / zero channel rules, fuzz oracles
- frame_from_planes refuses zero output dimensions (fuzz-found subtract overflow on a 5x0 sample entry) and sizes planes with saturating arithmetic
- seed the heif_records fuzz corpus with the vvcC conformance-window overflow unit (tracked as a minimal.* seed)
- track the heif_records fuzz crash unit (vvcC SPS head conformance-window overflow) and pin it at the parser level
- vvcC SPS head — saturate the conformance-window derivation (fuzz-found multiply overflow on hostile ue(v) offsets)
- README — VVC image items (Annex L) across the layout tables, options, capability matrix; a plainly stated HEVC / AVC / AV1 / VVC codec matrix with the VVC validator findings
- VVC verification matrix — VVC vs HEVC at the same QP, vvcC / compact-record / SPS-head fuzz round trips, validator survey
- VVC image items encode — StillCodec::Vvc through oxideav-h266's IDR encoder (items, alpha, grid, gain map, mini, sequences, framework encoder + muxer), vvcC from the encoder's parameter sets, vvic / vvis brands
- VVC image items decode (HEIF Annex L) — vvcC record model, vvc1 items through oxideav-h266's stream decoder, vvic/vvis/vvi3 brands, Annex L MIAF rules, vvi3 compact-record expansion
- oxideav-h265 >=0.0.13, <0.1 (an exact 0.0.x pin duplicated h265 in the umbrella graph)

### Added

- **Framework surface proved cell by cell (r475).**
  `tests/framework_matrix.rs` drives the registry `"heif"` encoder
  over every declared option (`codec` × `mode` × `qp` / `quality` ×
  `grid` × `thumbnail` × `range` × `rd` / `tiles` / `ctb` × `threads`
  × `depth` × `filters` × `chroma`) and every input `VideoFrame`
  layout the gateway can hand it (planar 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4
  at 8 / 10 / 12 / 16 bits with and without alpha, `Gbrp*` /
  `Gbrap*`, packed RGB / RGBA / BGR / BGRA, 16-bit packed, grey +
  alpha; 1×1, odd, unaligned and ≥ 4 MP pictures): every file opens in
  the framework demuxer and the contract `decode` with the documented
  label, both paths return byte-identical planes, the bytes equal the
  contract-path encode of the same picture under the same options,
  lossless cells are exact, and `heif-info` / Apple ImageIO open every
  file with its geometry. `tests/framework_decode_matrix.rs` runs the
  demuxer + registry decoders against `decode_all` / `info` over every
  HEIF-family file the checkout can see (vendored fixtures, the staged
  corpus, the device samples, the MPEG / Nokia conformance set) —
  pixels, labels, colour signal, item ids, timing.
- **Several still streams.** Displayable items whose predicted output
  (layout, geometry, colour description) differs from the primary's
  used to be dropped by the demuxer; they now form further `"heif"`
  streams, one per distinct output in first-appearance order (the
  multi-part EXR rule), each with its `stream:<n>:item_ids`. The
  streams follow `decode_all`'s selection exactly (an `altr` entry's
  first buildable member — a `tmap` before its base; the other view
  of a `ster` pair rides the pair's packet as the view-tagged second
  frame rather than a packet of its own); a `pitm` the display order
  leaves out (a thumbnail as primary, conformance C005) comes first
  on both paths. Raw track streams carry `stream:<n>:track_id`.
  `HeifDemuxer::still_stream_items`.
- The still streams stay first whatever the major brand (the composed
  primary still is video stream 0 — the gateway's `primary()` /
  `max_frames(1)` convention — and the track streams follow); the CLI
  leg's `.heics → .mp4` refusal (the MP4 muxer handed the cover still)
  is cli-convert's stream selection, fixed there.
- README: *Framework surface* (the two matrices' counts, the per-codec
  limits table, the 12 MP CLI budgets each direction and the black-box
  findings outside this crate: the hardware JPEG decoder's 4:4:4 frames
  with 4:2:0 planes, the pure-Rust MJPEG encoder's `expected 3 planes`,
  the CLI's stream selection for `.heics → .mp4`).
- **Decoder options.** The `"heif"` decoder declares a schema
  (`HeifDecoderOptions`: `max_width` / `max_height` / `max_pixels` /
  `max_bytes` / `strict` / `tone_mapped` / `item_id` /
  `base_layer_fallback` / `reference_white`) so a pipeline bounds a
  decode before any allocation; `HeifCodec::with_decode_options`.
- `encode::to_layout` (any planar layout → a coding layout: chroma
  box-averaged or replicated, depth re-quantised) and
  `encode::coded_layout` (the one coding-layout rule per codec).
- Fuzz target `heif_framework`: the registry probe, the demuxer
  (streams, packets, seeks, active streams), the `"heif"` decoder
  under tight decoder options and the muxer pass-through.

### Fixed

- `compose::plan_colour_format_enhancement_regions` indexed `inputs[0]`
  on a `cfen` item without inputs (fuzz-found by `heif_records`, the
  5-byte body `0f 00 00 00 00`): a typed `InvalidData` refusal now;
  the input is a corpus seed and a unit test.
- `MinimizedImage::codec_types` refuses explicit codec types of `uuid`
  (the extended-type box of ISO/IEC 14496-12 §4.2): the O.4 equivalent
  file used to carry a `uuid` property box without an extended type,
  which the parser refused (fuzz-found by `heif_records`; corpus seed +
  unit test). The target's packed-channel oracle composes under a
  YCbCr description (under the identity matrix the planner asks for
  G / B / R channels, typed — that was the oracle's premise, not a
  defect).

### Changed

- **Planar sources keep their chroma sampling on HEVC.** A 4:0:0 /
  4:2:2 / 4:4:4 planar source used to be coded 4:2:0 by the HEVC path
  (grey with neutral chroma), which made the lossless mode lossy on
  those layouts and ignored `chroma=444`; HEVC now codes the source's
  own sampling at the coded depth (as AV1 did), `chroma` selects the
  sampling of packed RGB sources on both codecs (4:2:0 by default,
  4:4:4 on request), packed grey + alpha codes a 4:0:0 item, and
  planar RGB converted for a lossy HEVC item lands in `chroma`'s
  sampling. Apple ImageIO opens every such file (measured).
- `depth` applies to AV1 items of ≤ 12-bit sources too (it was
  ignored there).
- `mode=pcm` on `codec=avc` is a typed refusal (the AVC encoder has no
  lossless mode), as on VVC.
- The `"heif"` encoder's packets carry `dts` and `duration` 1 (the
  demuxer's shape).
- `decode_all` lists the primary first even when the display order
  leaves it out (a thumbnail named by `pitm`).
- Conformance-matrix round trip: Apple ImageIO's cell is held within
  1.5 dB of this crate (its own 4:2:0 pipeline lands 0.8–1.0 dB under
  every other reader, run to run); the other readers stay within 1 dB.

- **AVC items write path (HEIF Annex E).** `StillCodec::Avc` codes
  `avc1` items through `oxideav-h264`'s registry encoder
  (`encode::encode_avc_picture_owned`: one IDR picture at `qp`,
  `rc=cqp`; CAVLC Baseline, CABAC Main with the new
  `EncodeOptions::avc_options` (`with_avc_options`, a positional
  argument of `new`) `cabac=1`; 8-bit 4:2:0 — the encoder's input line
  — pictures padded to 16 + `clap`), `avcC` from the SPS / PPS, the
  `avci` brand, alpha under the CICP alpha URN (E.2.4), grid tiles,
  thumbnails, `encode_still_minimized` with explicit `avc1` / `avcC`
  types, the `"heif"` framework encoder's `codec=avc` / `h264`;
  `encode_all` writes `avcs` image sequences as one encoder session
  per track (IDR + P pictures, `gop` = the frame count; sync flags from
  the packets) and the `"heif"` muxer takes `"h264"` streams (Annex B
  access units with or without parameter sets, or length-prefixed with
  an `avcC` extradata; `encode::avc_sample_from_annex_b`). H.264
  §7.4.3 — consecutive IDR access units differ in `idr_pic_id` —
  is applied by the muxer to the streams it is fed
  (`encode::avc_rotate_idr_pic_id`: a same-length ue(v) rewrite that
  keeps the CABAC alignment; refused typed when the value is 0 and
  repeats). `SequenceWriter` picks `avcs` for `avc1` / `avc3` entries.
  `tests/avc_lhevc.rs`: every written item and sequence sample
  byte-exact in the black-box AVC decoder; `heif-info` / `sips` open
  the files.

### Changed

- Conformance matrix refreshed (r474, same machine): every r464 cell
  unchanged; the writer direction gains this crate's AVC and VVC items
  (`heif-info` opens them, Apple ImageIO renders the AVC items, the
  other readers have no decoder for those item types here — reported,
  not judged).

### Fixed

- `fuzz/Cargo.lock` is no longer tracked (and is ignored): the nightly
  Fuzz workflow resolves the sibling codec crates afresh, so a codec
  release that fixes a fuzz-found panic reaches the harness without a
  heif commit.

- **Packed `cfen` inputs (HEIF Amd 1:2025 §6.6.2.5.2 `is_packed_flag`,
  Amd 2:2026 §6.6.2.5.3 regions).** A `cfen` input whose luma plane
  carries `rows × cols` channel partitions separated by guard bands now
  composes: `compose::plan_colour_format_enhancement_regions` plans
  every region (`CfenRegionPlan` / `CfenRegion`, the partition cells cut
  as `image_width / (num_cols_minus1 + 1)` — the clause's own
  "(num_cols_minus1+1) × (num_rows_minus1+1) regions" sentence; the
  printed `/ num_cols_minus1` divides by zero — with the `OrigX` /
  `EndX` guard-band offsets, a remainder ignored),
  `composite_colour_format_enhancement` copies the regions,
  `compose::pack_colour_format_channels` builds a packed picture from
  channel pictures, `HeifWriter::add_colour_format_enhancement_inputs`
  writes `CfenInput::Packed` entries, and `layout::predict_output`
  predicts the result. `packed_channel_id` 0 (unused) and 1
  (unspecified) are skipped; depth (6), K (7 — the Amd 2 CMYK column,
  whose selecting `colr` signalling the clause does not name) and
  reserved values are refused typed. `miaf::check` reports a channel
  above 1 signalled twice (`HEIF-A1 6.6.2.5.1`) and an unpacked
  `channel_id` 0 (`HEIF-A2 6.6.2.5.3`). `plan_colour_format_enhancement`
  keeps its shape (unpacked inputs; packed ones refer to the regions
  planner). Tests: `tests/tiled.rs`
  `packed_colour_format_enhancement_round_trips` (4:2:0 / 4:2:2 / 4:4:4
  / RGB / RGBA / mono+alpha, guard bands, odd sizes with a remainder
  column, serial and parallel decode) and `_refusals`; the
  `heif_records` fuzz target plans regions from parsed bodies (every
  region inside its input), composes them and checks the pack → cut
  identity.
- **VVC image items decode (HEIF Annex L).** `vvc1` items are coded
  images: the `vvcC` property (ISO/IEC 14496-15:2024 §11.2.4
  `VvcDecoderConfigurationRecord` as a FullBox) is modelled by
  `VvcConfig` / `VvcPtlRecord` (`vvcc` module, standalone; byte-exact
  rewrite, `ptl_present_flag = 0` records take their sample layout from
  the SPS in the arrays), `Property::VvcC` / `ItemProperties::vvcc`,
  `layout::vvc_layout`, `SampleEntry::vvcc` (`vvc1` / `vvi1` entries),
  the `vvic` / `vvis` / `vvi3` brands (`BrandClass::vvc`), and
  `miaf::check` rules `HEIF L.2.3.1` (essential `vvcC`), `HEIF L.2.4.3`
  (`vvs1` + `vvnC`) and `HEIF L.4.1.1` (the `vvic` essential-property
  set). With `registry`, `vvc1` items decode through the new `vvcdec`
  module — a framework `Decoder` over `oxideav-h266`'s Annex B stream
  decoder (its registry factory is a parser-only placeholder on the
  published 0.0 line): the record's NAL units are re-assembled with the
  item's length-prefixed ones (`vvcc::access_unit_annex_b`, AUD / OPI
  first, the L.2.2.1.2 `tols` OPI rule), the conformance window
  applied, 4:2:0 / 4:0:0 at the SPS bit depth. The contract path, the
  direct item decoder and the `"heif"` framework codec are byte-identical
  on VVC stills, and ffmpeg's VVC decoder is byte-exact against ours on
  the same access units (`tests/vvc.rs`). `vvcdec::vvc_item_from_annex_b`
  splits an encoder's access unit into the record (DCI / OPI / VPS /
  SPS / PPS / prefix APS / SEI, complete arrays) and the `VVCItemData`,
  taking the PTL from the SPS or the VPS. Low-overhead `vvi3` files
  (Amd 2 L.4.3): the `CompactVvcDecoderConfigurationRecord`
  (`CompactVvcConfig`) expands to the equivalent `vvcC` (L.4.3.3.4) and
  a single-layer item's header-less `IDR_N_LP` payload to `VVCItemData`
  (previously the compact bytes were written as the property verbatim).
  New optional dependency `oxideav-h266 >=0.0.9, <0.1` behind
  `registry`.
- **VVC image items encode.** `StillCodec::Vvc` codes `vvc1` items
  through `oxideav-h266`'s IDR encoder (CABAC intra at `qp`, 8-bit
  4:2:0 — the encoder's line; `hevc_mode = "pcm"` is refused, there is
  no lossless VVC mode) in every shape the HEVC / AV1 paths take:
  `encode` / `encode_rgb8` / `encode_rgba8` / `encode_all`
  (`vvis` image sequences with `vvc1` sample entries), `encode_still`
  (+ `_owned` / `_into`), `encode_still_minimized` (`mif3` + `vvic`
  minor with explicit `vvc1` / `vvcC` types), grid tiling, thumbnails,
  alpha (CICP `auxC` URN), gain maps, transforms, Exif / XMP / ICC, the
  `"heif"` framework encoder (`codec=vvc` / `h266`) and the `"heif"`
  sequence muxer (`"h266"` streams, Annex B or length-prefixed with a
  `vvcC` extradata). Pictures are padded to 64 (`VVC_ALIGNMENT`) and
  cropped back with `clap`: the published encoder codes 64-multiples
  (or a single CTU) conformantly and nothing else. `vvcC` is written
  from the encoder's parameter sets (VPS + SPS + PPS complete arrays,
  PTL from the VPS, `ptl_present_flag` 1, 4-byte lengths), brands
  `vvic` + `mif1` + `miaf` (`vvis` + `msf1` for sequences).
  `EncodeOptions::vvc_options` (`with_vvc_options`) passes the
  encoder's tool knobs (`tiles`, `wpp`, `dep_quant`, `sdh`, `palette`,
  `mtt_bt`, `mtt_tt`, `alf_clip_rdo`, `chroma_sao_merge`,
  `loop_filter_across_tiles`, `slice_per_tile`, `raster_slices`);
  `EncodeOptions::new` gained that positional field. `heifenc` takes
  `--codec vvc`. `tests/vvc.rs`: round trips at 1×1 … 320×192 (padding
  + `clap`), every tool axis, alpha / grid / overlay / gain map /
  sequence / mini / framework-encoder parity, MIAF + Annex L
  conformance, and every coded item byte-exact in ffmpeg's VVC decoder.
- **VVC verification matrix.** `tests/vvc.rs` prints VVC against HEVC
  at the same QP (bytes / luma PSNR) and holds both to the 30 dB bound;
  the `heif_records` fuzz target round-trips `VvcConfig`,
  `CompactVvcConfig`, `SpsHead` and the VPS PTL walk. No staged
  conformance or device-sample file carries a VVC item (334 files
  swept), so the workspace encoder's output is the corpus; of the
  black-box tools on the reference machine ffmpeg's VVC decoder is
  byte-exact on every coded item, `heif-info` opens the `vvic` files
  (structure, layout, alpha), and `heif-convert` / `magick` (libheif
  without a vvdec plugin), `sips` and ffmpeg's HEIF demuxer do not
  decode `vvc1` items.

### Changed

- `oxideav-h265` requirement is `>=0.0.13, <0.1` (a `0.0.x` caret requirement is exact and left two h265 versions in the umbrella graph).

## [0.0.10](https://github.com/OxideAV/oxideav-heif/compare/v0.0.9...v0.0.10) - 2026-10-05

### Other

- demuxer emits bursts (one still packet per displayable item); lossless RGB codes as identity-matrix 4:4:4
- fallible from_rgb8/from_rgba8, encode_all (items + image sequence) — image-crate API fleet sweep
- README examples use the current registry API

### Changed

- **Framework demuxer: bursts are frames.** Stream 0 (`"heif"`) emits
  one packet per displayable image item — the primary first, then the
  other items in `decode_all`'s order (MIAF display order, `altr`
  groups once, thumbnails / auxiliaries / hidden items excluded, only
  items whose predicted layout and geometry match the primary's) —
  each the whole file with its `pitm` patched to the item, so the
  `"heif"` decoder reconstructs it with its own derivations, transforms
  and alpha. Item packets are untimed: `pts` = item index, duration 1,
  time base 1/1; `StreamInfo::duration` is the item count and
  `metadata` carries `stream:<n>:item_ids`. Image-sequence tracks were
  already one stream each (one packet per sample, `pts` / `dts` /
  `duration` from the sample table in the track timescale); both are
  now pinned byte-identical to `decode_all` over every corpus bundle,
  every interop file and the two real-world `.heics`. `seek_to` on the
  still stream lands on the item index; `HeifDemuxer::still_items()`
  lists the carried items. A plain still is unchanged (one packet).
- **Lossless RGB is lossless — the file signals what it holds.** With
  `mode=pcm` (and AV1 at quality 100 / without a quality) a packed
  RGB / RGBA / BGR(A) or planar RGB (`Gbrp*`, ≤ 12 bit) source is coded
  as a 4:4:4 item with `matrix_coefficients = 0` (identity: the G, B, R
  planes as they are, full range) on HEVC too, instead of BT.601 4:2:0
  YCbCr, which the gateway suite measured at mean 8 / max 22 after the
  round trip (chroma subsampling, not a matrix mismatch). The written
  `nclx` is therefore primaries / transfer of the source (BT.709 /
  sRGB by default), matrix 0, full range — what the encoder does;
  `decode` yields `Gbrp8` / `Gbrap8`, the registry stream and frames
  are labelled the same, and `decode(encode(rgb)).to_rgba8() == rgb`
  exactly (0 LSB) on both codecs, pinned. Lossy coding (`intra`, AV1
  with a quality) keeps the 4:2:0 BT.601 default; 16-bit RGB sources
  still convert at the coded depth, and an explicit `range=limited`
  keeps the YCbCr conversion (identity items are full range).

- **Fallible constructors (`IMAGE_CRATE_API` fleet sweep, breaking).**
  `HeifImage::from_rgb8` / `from_rgba8` return `Result<HeifImage,
  HeifError>` like `HeifImage::new` already did, refusing a zero
  dimension or a short buffer with `InvalidData` at construction; the
  infallible signatures are not kept.

### Added

- `encode_all(&[Frame], &EncodeOptions)`, the mirror of `decode_all`:
  delay-less frames become image items (primary + burst), timed frames
  the samples of an image-sequence track (timescale 1000, alpha track
  when the pictures carry alpha, MIAF cover item aliasing sample 0),
  mixed as `decode_all` returns them. `decode_all(encode_all(frames))
  == frames` is pinned for planar `Yuv420P` frames in the lossless HEVC
  mode; the sequence output passes the MIAF checker.

## [0.0.9](https://github.com/OxideAV/oxideav-heif/compare/v0.0.8...v0.0.9) - 2026-10-03

### Other

- contract rulings — HeifError::Io(std::io::Error), Option limits in DecodeOptions, validating HeifImage::new, (&VideoFrame, &CodecParameters) bridge
- README in the contract's section order + CHANGELOG for the image-crate API
- fuzz + ci: contract targets (heif_api standalone, heif_decode under registry) and a full no-default-features job
- the image-crate contract vocabulary at the root (standalone probe / info / HeifImage; registry decode* / decode_all / encode*)

### Added (the workspace image-crate API contract, r465)

- Root vocabulary per `IMAGE_CRATE_API.md`. Standalone
  (`default-features = false`): `probe(&[u8]) -> bool` (HEIF-family
  `ftyp` brands), `info(&[u8]) -> ImageInfo` / `info_of(&HeifFile)`
  (primary geometry after transforms, the `PixelFormat` a decode
  yields, `frames` = displayable items + sequence samples, alpha /
  colour / ICC / Exif / XMP presence, `primary_item_id`,
  `has_gain_map` — read from the container alone), the types
  `HeifImage` (`width`, `height`, `format`, `planes`, `color`,
  `metadata`, `palette`; `new` / `from_rgb8` / `from_rgba8` /
  `from_frame`, `to_rgb8` / `to_rgba8` exact H.273 kernels at the
  signalled matrix and range, `as_bytes` / `into_raw` / `to_frame` /
  `into_frame` / `validate`), `PixelFormat` (a new enum whose variants
  mirror `oxideav_core::PixelFormat` by name: the planar YCbCr / grey
  / `Gbrp*` layouts this crate emits plus packed `Rgb24` / `Rgba`;
  `from_layout` / `from_layout_promoting` / `layout` and `TryFrom`
  both ways with `HeifPixelFormat`), `Plane` (the former `HeifPlane`),
  `ColorInfo` / `ColorRange`, `Metadata`, `Palette` (always `None` for
  HEIF), `RgbImage` / `RgbaImage` (8-bit, tightly packed),
  `ImageInfo`, `DecodeOptions` (`max_width` / `max_height` /
  `max_pixels` / `max_bytes` as `Option`s, `None` = unlimited, finite
  defaults; `strict` + `item_id` / `tone_mapped` /
  `base_layer_fallback` / `threads` / `reference_white_nits`),
  `Frame`, `Error` (= `HeifError`). `HeifImage::new` validates the
  plane geometry and returns `Result`; `try_to_rgb8` / `try_to_rgba8`
  report a bad geometry where `to_rgb8` / `to_rgba8` substitute.
- With `registry`: `decode` / `decode_with` / `decode_file` /
  `decode_file_item` (the whole `DecodedImage`), `decode_rgb8` /
  `decode_rgba8`, `decode_all` / `decode_all_with` (burst items —
  primary first, then by id, `altr` groups collapsed — then every
  sample of the image-sequence tracks through one codec instance per
  track, alpha tracks composed in, the sample entry's `clap` applied,
  `delay` from the track timing), `decode_from<R: Read>`, `encode` /
  `encode_owned` / `encode_rgb8` / `encode_rgba8` / `encode_to<W:
  Write>`; `From<DecodedImage> for HeifImage`; the framework bridges
  `HeifImage::into_video_frame` / `from_video_frame(&VideoFrame,
  &CodecParameters)` / `from_video_frame_parts` /
  `TryFrom<(&VideoFrame, &CodecParameters)>`, `From<HeifImage> for
  VideoFrame`, `From<PixelFormat> for oxideav_core::PixelFormat` +
  `TryFrom` back, `ColorInfo::{to_color_signal, from_color_signal}`,
  `PixelFormat::to_core_labelled`. Limits are checked against the
  header before any decode (`HeifError::LimitExceeded`); `strict`
  requires a HEIF-family brand and MIAF conformance of `miaf` files.
- `EncodeOptions::chroma` (+ `with_chroma`): the chroma layout packed
  / planar RGB sources are coded in on AV1 (the framework encoder's
  `chroma=444` option now reaches the library API). `EncodeOptions::new`
  takes it as its last argument.
- `HeifError::{LimitExceeded, Io(std::io::Error)}` (+ `limit`, `io`,
  `From<std::io::Error>`, `Error::source`); `ItemDecoder::with_threads`.
- `layout` module: `hevc_layout` / `av1_layout` / `layered_layout` /
  `layered_base_layout` / `layout_of` / `predict_output` /
  `entry_layout` are standalone now (re-exported from `decode` /
  `demux` where they lived).
- `tests/api.rs` (contract conformance against the corpus oracles,
  limits / strictness / selection, encode round trips, byte-identity
  of the framework adapters with the contract path), fuzz targets
  `heif_api` (standalone `probe` / `info`) and `heif_decode`
  (`decode_with` / `decode_rgba8` / `decode_all_with` under limits;
  the fuzz crate now builds with `registry`), seeded from the corpus.
- CI: the `ci-standalone` job builds and tests every target with
  `--no-default-features` and runs clippy.

### Changed

- The `"heif"` framework decoder and encoder are thin adapters over
  the contract functions (`decode_file_item`, `encode_owned` /
  `encode_packed_bytes`): one implementation, byte-identical output
  (pinned by `tests/api.rs`).
- `rgb::RgbImage` (interleaved `u16` samples) is `rgb::RgbImage16`;
  the root `RgbImage` is the contract's 8-bit record. `rgb::to_rgb`
  returns `RgbImage16`; the shared kernel is `rgb::for_each_pixel`,
  with `rgb::planes_to_rgb8` the 8-bit path.
- `HeifError::exhausted` builds `LimitExceeded`; `ResourceExhausted`
  is no longer produced (deprecated, see below). The `Display` text is
  `heif: limit exceeded: …`.
- `HeifError` no longer derives `Clone` / `PartialEq` / `Eq` (its
  `Io` variant wraps `std::io::Error`); match on variants or `Display`.
- The `heifencbench` example requires `registry` (it always did).

### Fixed

- A monochrome + alpha layout is promoted to `Yuva444P` on the
  framework side instead of being labelled `Ya8`: the framework's
  `Ya8` is a *packed* grey + alpha layout, so the two-plane frame the
  demuxer announced under that label could not be read back by its
  consumers.

### Deprecated (one release)

- `HeifPlane` → `Plane` (type alias).
- `HeifError::ResourceExhausted` → `HeifError::LimitExceeded` (the
  variant stays so existing `match` arms compile; it is never
  produced).

## [0.0.8](https://github.com/OxideAV/oxideav-heif/compare/v0.0.7...v0.0.8) - 2026-10-02

### Other

- plumbing records go #[doc(hidden)] (non_exhaustive dropped there); documented records keep the constructors

### Changed (API hygiene, pass 2 — plumbing records leave the documented surface)

- `#[doc(hidden)]` (and no longer `#[non_exhaustive]` — hidden items
  are outside the semver contract) on the records that exist only
  because a module boundary needed them: `meta::{Extent, ItemLocation,
  Handler, DataReference, PropertyAssociation,
  ItemPropertyAssociations, ItemReference, RawProperty, PyramidInfo,
  RegionPartitionArea}`, `props::PropertyEntry`, `hvcc::NalArray`,
  `lhvc::{OperatingPointPtl, OperatingPointLayer, OperatingPoint,
  LayerDependency}`, `mini::{MiniChroma, MiniHdrBoxes, MiniGainMap,
  MinimizedImage}`, `tiled::{DataEntryTiledItem, ExternalTiles}`,
  `gainmap::LinearRgbImage`, `derived::ImageNode`, `ftyp::BrandClass`,
  `writer::SequenceSample`. They stay `pub` (public signatures —
  `Meta` / `Property::Unknown` / `LhevcConfig` / `HevcConfig` fields,
  `build_graph`, `apply_gain_map` — name them) and keep their `new`.
- Kept `#[non_exhaustive]` + constructors (deliberately constructed or
  read): the typed properties, `ItemInfo` / `ItemProperties` / `Meta` /
  `EntityGroup`, the codec configurations, `DecodedImage` and its
  `LayerFrame` / `GainMapAttachment`, the derived descriptors
  (`GridDescriptor`, `OverlayDescriptor`, `ColourFormatEnhancement`,
  `TiledItem`), `compose::OverlayInput`, `rgb::RgbImage`, gain-map
  metadata, MIAF report / violation, `EncodeOptions` / `GainMapSpec` /
  `HeifEncoderOptions`, `FileType`, the sequence views (`Track`,
  `Movie`, `SampleEntry`, `Sample`, `Edit`, `TrackOrientation`,
  `CodingConstraints`, sample groups, `prft`, `ssix`), the writer
  specs, `HeifFile`.
- Doc links: `lhvc` module → `HeifError::layered_hevc`; `encode_still`
  no longer links a private item.

## [0.0.7](https://github.com/OxideAV/oxideav-heif/compare/v0.0.6...v0.0.7) - 2026-10-02

### Other

- #[non_exhaustive] on every public record and growing enum, new(...) constructors, with_* setters on the option records

### Changed (API hygiene — a minor bump; every public record gains an additive-friendly shape)

- **`#[non_exhaustive]` on every public record struct** that callers
  could build by struct literal, with `Type::new(<every field, in
  declaration order>)` as the construction path (fields stay `pub`:
  read and assign them after `new` / `Default`; `..Default::default()`
  struct update is no longer available from outside the crate — use
  the setters below or assign). Affected, by module: `props` (`Ispe`
  `Pixi` `Pasp` `Clap` `Irot` `Imir` `Iscl` `AuxC` `Clli` `Mdcv` `Cclv`
  `Amve` `Rloc` `Lsel` `A1op` `A1lx` `Rref` `TimeInfo` `Udes` `Altt`
  `Reve` `Ndwt` `Cexg` `Dadj` `StereoAggressor` `Stag` `PixiChannel`
  `PixiExtended` `TilC` `PropertyEntry` `ItemProperties`), `meta`
  (`Handler` `ItemInfo` `Extent` `ItemLocation` `RawProperty`
  `PropertyAssociation` `ItemPropertyAssociations` `ItemReference`
  `EntityGroup` `PyramidInfo` `RegionPartitionArea` `DataReference`
  `Meta`), `derived` (`GridDescriptor` `OverlayDescriptor`
  `ColourFormatEnhancement` `TiledItem` `ImageNode`), `decode`
  (`DecodedImage` `GainMapAttachment` `LayerFrame`), `encode`
  (`EncodeOptions` `GainMapSpec` `HeifEncoderOptions`), codec
  configurations (`HevcConfig` `NalArray` `Av1Config` `AvcConfig`
  `LhevcConfig` `OperatingPoints` `OperatingPoint` `OperatingPointPtl`
  `OperatingPointLayer` `LayerDependency`), `ftyp` (`FileType`
  `BrandClass`), `gainmap` (`GainMapMetadata` `GainMapChannel`
  `LinearRgbImage`), `miaf` (`MiafViolation` `MiafReport`), `mini`
  (`MiniChroma` `MiniHdrBoxes` `MiniGainMap` `MinimizedImage`),
  `sequence` (`CodingConstraints` `SampleEntry` `Sample` `Edit`
  `TrackOrientation` `SampleGroup` `SampleGroupDescription`
  `ProducerReferenceTime` `SubsegmentIndex` `Track` `Movie`), `tiled`
  (`DataEntryTiledItem` `ExternalTiles`), `compose::OverlayInput`,
  `rgb::RgbImage`, `writer` (`SequenceAlphaTrack`, `SequenceWriter` —
  which keeps its own `new`), `HeifFile`.
- **Setters** `with_<field>` on the option records `EncodeOptions`,
  `HeifEncoderOptions` and `GainMapSpec` (one per field), so
  `EncodeOptions::default().with_qp(20).with_grid_tile(Some(512))`
  replaces the struct-update literal.
- **`#[non_exhaustive]` on the public enums that grow with the
  standard**: `Property` (a variant per typed property — this crate
  gains properties every round), `Colr` (`Nclx` / `Icc` / `Other`; a
  future `colr` colour type), `HeifError` (error classes), `StillCodec`
  (VVC items are in the standard already), `ImageKind`, `CfenInput`,
  `ToneMapOutput`, `MiafProfile` (brands), `AuxKind`,
  `StereoFallbackPosition` (`flags & 3` has a reserved value),
  `LoopBehaviour`, `mini::SampleFormat`. Matches on them need a `_`
  arm from outside the crate.
- **Kept open on purpose** (closed by the standard or fundamental
  value types): `Chroma` (`chroma_format_idc` 0..=3 — H.265 / AV1
  define no fourth structure), `HeifFrame` / `HeifPlane` /
  `HeifPixelFormat` (the planar sample model: dimensions, layout,
  planes), `CropRect`, `gainmap::Rational`, `boxes::BoxHeader` (the
  ISOBMFF header: size / type / `largesize` / `uuid`).
- Internal-only `pub` items were already `#[doc(hidden)]` (`CodedPicture`,
  `WriterItem`, `ItemBody`, `SequenceSample`, `CodedKind`, `ItemKind`,
  `WalkEntry`); unchanged.

## [0.0.6](https://github.com/OxideAV/oxideav-heif/compare/v0.0.5...v0.0.6) - 2026-10-01

### Other

- CLI budget rows re-measured on the final binary
- honour the source's colour signal in the written colr (frame record, else stream; range unless given; RGB sources keep the derived matrix)
- the encode runs take the --threads budget; doc names the r464 defaults
- cut grid tiles straight from the coding picture (edge-replicated per tile, no padded canvas); README budget table re-measured
- r464 — production defaults with the OS-encoder comparison, the encode budget table (library / CLI, 1/4/8 threads, where the time and memory go), the round-trip table, region groups
- unrg / corg groups of regions (HEIF Amd 2 §11.3.5) — accessors, image association, writer helpers, check rules
- round trip at the production defaults through the CLI path; AV1 keeps the chroma layout of deep sources
- thread budget through grid tiles / wavefront / AV1 tiles, owned coding frames, direct packed conversion, streamed mdat, production defaults

- Encode path budgeted and optimised (round 464; every written file
  decodes byte-identically for every thread budget — pinned by
  `tests/encode_budget.rs`). `EncodeOptions::threads` carries the
  thread budget: the tiles of a `grid` are coded as independent jobs
  on that many workers and handed to the writer in row-major order as
  they complete (ids and bytes those of the serial encode), the HEVC
  quadtree coder gets `wpp` (always on there, so the bytes never
  depend on the budget) or its `tiles` fan-out through
  `set_execution_context`, and the AV1 still search runs on it over a
  tile layout derived from the picture size alone
  (`AV1_TILE_LAYOUT_THREADS`). The framework `"heif"` encoder takes
  the budget from `Encoder::set_execution_context` or the new
  `threads` option (`auto` / a count; explicit wins).
- Memory: coded pictures are moved into the codecs (no clone on this
  side — `encode_hevc_picture_owned` / `encode_av1_picture_owned`,
  `encode_still_owned` for a frame already in the coding layout),
  padding and alpha / thumbnail cuts only allocate when they change
  something, packed RGB / RGBA / grey(+alpha) frames convert straight
  into the coding layout row pair by row pair (`packed_to_planar_for`:
  no full-resolution 4:4:4 intermediate), `HeifWriter::write_to`
  streams the `mdat` from the item bodies (no assembled payload copy)
  and the muxer validates a still packet with the borrowing parser;
  grid tiles are cut straight from the coding picture with edge
  replication (no padded canvas). 12 MP 8-bit HEVC through the
  framework path: 627 → 77 MiB peak serial (grid) / 504 MiB single
  item; see the README's encode budget table.
- Production defaults: `qp` 18 (the size / PSNR of the OS encoder's
  default on a 12 MP photograph — README), HEVC deblocking + SAO on
  lossy items (`EncodeOptions::hevc_filters`, option `filters`),
  automatic 512-px `grid` tiling above 4 MP (`grid_tile == None`;
  `Some(0)` / option `grid=none` forces a single item), 4:2:0 for
  packed RGB sources (option `chroma=444` keeps 4:4:4 for AV1), and
  deeper-than-8-bit sources coded as Main 10 (`hevc_depth` / option
  `depth`; 12-bit sources at 12; `to_yuv420` converts to any depth;
  `coded_depth` is the rule). A layout that merely needs the quadtree
  coder runs its level-0 search unless `rd` asks for more.
- Round-trip proof at the production defaults
  (`tests/conformance_matrix.rs`, third table): a PNG source through
  the CLI path (the registry encoder fed the packed frame; the real
  `oxideav convert` binary when `OXIDEAV_CLI` names it) to a HEIC and
  an AVIF, decoded back by this crate and rendered by the five
  readers — PSNR against the source per reader, every reader within
  1 dB of our decode. AV1 items of a source deeper than 12 bits keep
  their chroma layout at the coded depth (`to_depth`; a 16-bit 4:4:4
  source is a 10-bit 4:4:4 item, which Apple ImageIO now renders
  exactly) instead of dropping to 8-bit 4:2:0.
- `unrg` / `corg` groups of regions (HEIF Amd 2:2026 §11.3.5):
  `EntityGroup::region_union` / `compound_region` (main region +
  the regions it logically includes), `Meta::region_items_of` (the
  `rgan` items with a `cdsc` to an image) and `Meta::region_groups_of`
  (the groups whose every entity is such a region),
  `HeifWriter::add_region_union` / `add_compound_region`, and `check`
  rules under `HEIF-A2 11.3.5.x` (region items only, one image per
  group, a `corg` names a main region and at least one part).
  `ITEM_TYPE_RGAN`.
- The framework encoder honours the colour signal: a frame's
  `ColorSignal` record (else the stream's
  `CodecParameters::color_signal`) refines the written `colr` — H.273
  code points that are not unspecified replace the MIAF defaults, a
  signalled range replaces the default range unless `range` was given
  (a limited-range YCbCr source is no longer labelled full), and for
  packed / planar RGB sources only the primaries / transfer / range
  are taken (the signal's matrix describes RGB, not the derived
  YCbCr).
- `examples/heifencbench`: the stage-timed encode bench (wall, CPU,
  peak RSS from `getrusage`; library and framework paths).
- oxideav-h265 0.0.13 is the minimum pin (wavefront, `cqpoffset`,
  lossy intra in every layout).

## [0.0.5](https://github.com/OxideAV/oxideav-heif/compare/v0.0.4...v0.0.5) - 2026-09-29

### Other

- r463 — finals reconcile, Amd 1 / Amd 2 surface, low-overhead files, layered items, tiled / cfen items, the r463 performance table
- tili / cexg / cfen: tiled items behind deti offset tables, constrained-extent tiles, colour format enhancement
- enhancement layers through oxideav-h265 0.0.12 — stereo pairs, per-layer output, LayerIdentity tags
- parallel grid tiles under ExecutionContext, incremental canvas, copy-free reconstructions
- identity-matrix items as planar RGB, ColorSignal on streams + frames; rd/tiles pick the CTB
- low-overhead image files (HEIF Amd 2 Annex O) — reader expansion, writer, dExf
- props + groups: HEIF Amd 1 reve/ndwt/cexg/dadj/stag, Amd 2 pixi channels + tilC, pymd/rgpa/stem, MIAF Annex A order + looping
- finals reconcile: tmap fully-applied weight, MIAF Amd 1 altr mandate, advisories

- Tiled image items (HEIF Amd 2:2026 §6.11): `tili` items compose from
  their `deti`-addressed tiles — `tiled::DataEntryTiledItem` parses the
  `dref` entry (offset / size / count field widths, sequential flag,
  the external-URL template) and its `TiledImageOffsetTable` (sizes
  inferred from offset differences when absent, empty tiles), the
  tiles decode as coded pictures of `tile_item_type` under the
  `tipa`-associated properties (in parallel under the thread budget),
  are cropped to the `ispe` and empty tiles render neutral grey
  (`GridCanvas::place_blank`). `HeifWriter::add_tiled_item` packs
  tiles + table behind a `deti` (the first `dinf/dref` this writer
  emits) and threads the tile properties through the `tilC`.
  Hyperrectangles and external tiles are typed refusals.
  `HeifFile::item_extents` returns an item's `iloc` extents one by one.
- `cexg` items (HEIF Amd 1:2025 §6.5.41): a coded item whose `iloc`
  extents are the tiles of the announced grid decodes extent by
  extent into the canvas (`HeifWriter::add_constrained_extents_item`
  writes the multi-extent `iloc`); an `ExtentDecoderConfigurationRecord`
  is refused (codec-specific, undefined for HEVC / AV1 here).
- `cfen` colour format enhancement derived items (HEIF Amd 1:2025
  §6.6.2.5, channel table as amended by Amd 2): the inputs' luma planes
  become the Y / Cb / Cr (or, under an identity-matrix `colr`, R / G /
  B) and alpha planes of one picture, the chroma layout following the
  chroma planes' sizes (`derived::ColourFormatEnhancement`,
  `compose::composite_colour_format_enhancement` /
  `plan_colour_format_enhancement`, `HeifWriter::add_colour_format_enhancement`;
  `check` reports the §6.6.2.5.1 property / hidden-input rules). Packed
  inputs are refused: the Amd 2 region formula divides by
  `num_cols_minus1` / `num_rows_minus1` (a docs question, not guessed).
- `Reader::uint` reads any 1..=8-byte width (the 24 / 40 / 48-bit
  fields of `deti` tables).

- L-HEVC enhancement layers (oxideav-h265 0.0.12, now the minimum pin):
  an `lhv1` item that carries `hvcC` + `lhvC` decodes through the
  multi-layer decoder (extradata = base `hvcC` ++ item `lhvC`;
  `layer=<lsel>` for an item with an `lsel`, else `ols=<tols>`) — the
  HEIF stereo shape (two `lhv1` items with `lsel` 0 / 1 in a `ster`
  group) yields each view, and an item without `lsel` whose `tols`
  output layer set has several output layers yields all of them as
  `DecodedImage::layers` (`decode::LayerFrame`, base first,
  transforms applied; `ItemDecoder::decode_coded_layers`).
  `base_layer_fallback` now decodes `layer=0`; the typed
  `layered_hevc` refusal remains only for items without a base `hvcC`
  (Annex B base-layer path). Framework: the still stream announces
  `CodecParameters::layers` (output layers, or the two views of a
  `ster` pair) and the `"heif"` decoder emits one frame per layer /
  view tagged with oxideav-core's `LayerIdentity` (view 0 = left).
  Both views of an MV-HEVC access unit from a black-box producer
  (Apple VideoToolbox through `AVAssetWriter`, `tests/fixtures/layered/`)
  wrapped by `HeifWriter` decode byte-exact against the black-box
  decoder's per-view output (`ffmpeg -view_ids`). QuickTime sample
  entries ending in a 4-byte zero terminator after their child boxes
  now parse.

- Decode optimisation, phase 1 (byte-identical output; the r462
  fixture pins and a serial-vs-parallel test over every grid in the
  corpora are the gate). `ItemDecoder::with_execution_context`: the
  tiles of a `grid` decode as independent jobs on up to
  `effective_workers` threads (codec instances made on the workers,
  each serial) and are written straight into the canvas as they
  complete; the framework `"heif"` decoder takes the budget from
  `Decoder::set_execution_context` (serial by default, per the
  oxideav-core contract). `compose::GridCanvas` composes a grid
  incrementally at the output size (no full-size canvas + crop, no
  vector of decoded tiles; `composite_grid` now runs on it);
  `compose::apply_transforms_owned` returns an untransformed image
  without a copy; coded reconstructions are moved out of the codec's
  planes when tight and cached only while another use in the graph
  follows. 12 MP on an M4 Max (medians of 5): Apple 48-tile grid
  0.330 s → 0.326 s serial / 0.100 s at 4 / 0.058 s at 8 threads, peak
  RSS 89 → 52 MiB serial; single-item HEVC 0.294 s / AVIF 0.778 s →
  0.298 / 0.709 s, RSS 290 / 229 MiB → 290 / 226 MiB (the codecs' own
  buffers dominate both). `examples/heifbench`: `--threads N|auto` and
  an output FNV-1a column.

- Identity-matrix items (`matrix_coefficients = 0`, e.g. black-box
  lossless RGB) are planar RGB in the framework: the still stream and
  the decoded frames carry `Gbrp8` / `Gbrp10Le` / `Gbrp12Le` /
  `Gbrp14Le` / `Gbrp16Le` (`Gbrap*` with alpha; 9 / 11 / 13 / 15-bit
  depths keep the YCbCr label) instead of being read as BT.601
  YCbCr (`HeifPixelFormat::to_core_gbr` / `from_core_gbr`,
  `HeifFrame::to_core_signalled` / `from_core_gbr`,
  `demux::core_pixel_format_for`). The item's effective `nclx` (the
  MIAF default when absent) rides as the oxideav-core 0.1.37
  `ColorSignal` on the stream parameters (still, composed-alpha and raw
  track streams) and on every emitted frame, so full-range and >8-bit
  stills are no longer taken as limited range downstream. The encoder
  accepts `Gbrp*` / `Gbrap*` input: AV1 codes the planes as an
  identity-matrix 4:4:4 item (lossless stays exact), HEVC converts
  through the configured matrix.
- HEVC `rd` / `tiles` no longer need `ctb`: they run on the h265
  quadtree coder, which the encoder enables through `ctb`, so the size
  is now supplied automatically — `encode::auto_ctb`, the largest of
  64 / 32 / 16 whose CTB grid holds the tile layout — unless the caller
  sets `EncodeOptions::hevc_ctb` / the new framework option `ctb`
  (16 / 32 / 64; 0 = automatic; other values refused).

- Low-overhead image files (ISO/IEC 23008-12:2025/Amd 2:2026 Annex
  O): `mini::MinimizedImage` parses the bit-packed `MinimizedImageBox`
  (O.3: flags, 7/15-bit dimensions, chroma centring, integer / float
  depths, CICP defaults of Table O.3, explicit codec types, the HDR
  block with gain map + `tmap` CICP / ICC + main / tone-mapped
  `clli` / `mdcv` / `cclv` / `amve` / `reve` / `ndwt` bodies, the
  10/20-bit / 3/12-bit / 15/28-bit chunk sizes, `trailing_bits`, the
  chunk order) and rebuilds the normative O.4 equivalent — `ftyp` with
  the O.2.1.2 implied `mif1` / `tmap` and equivalent major brand, the
  fixed item ids 1–7, `auxl` / `prem` / `dimg` / `cdsc` references,
  the `altr` [3, 1] group, the 32-slot `ipco` with `free` placeholders,
  the O.4.6 association order, the O.4.7 per-channel `pixi`, the O.4.8
  `ToneMapImage`, `iloc` over the O.4.10 `mdat` order. `HeifFile`
  expands a `mini` file on parse (`HeifFile::minimized`,
  `original_bytes`), so decode / MIAF check / demux take the regular
  paths. Writer direction: `MinimizedImage::to_box` / `to_file` /
  `to_file_with_minor` and `encode::encode_still_minimized` (HEVC or
  AV1, alpha, ICC, Exif, XMP, gain map, Exif orientation from
  `irot` / `imir`; minor version = the equivalent `heic` / `avif`
  brand, which libheif keys on) — decodes byte-identically to the
  regular writer's file of the same picture, and `heif-convert`
  renders both identically. `mif3` brand (`BRAND_MIF3`, probe +
  `BrandClass`), `.hmg` extension, `image/hif2` (`mini::MIME_TYPE`);
  `check` enforces Table O.1 (`ftyp` + `mini` only). Refused with a
  typed `Unsupported`: codec-native alpha (O.4.6 slot 30, the
  `AlphaInformationProperty` Amd 2 names but does not define) and
  implicit codec types under an unknown codec brand (`vvi3` → `vvc1`
  / `vvcC` is expanded; this crate has no VVC decoder).
- `dExf` items and `content_encoding = "deflate"` XMP (Amd 2 A.2.1 /
  O.4.3) inflate on decode (`decode::inflate_metadata`, bounded) under
  the new default-on `deflate` feature (optional `compcol`
  dependency).

- HEIF Amd 1:2025 / Amd 2:2026 properties and groups, typed with
  writers: `reve` (§6.5.44), `ndwt` (§6.5.45), `cexg` (§6.5.41,
  16/32-bit tile fields, optional extent configuration), `dadj`
  (§6.5.42), `stag` (§6.5.43, aggressor list with URIs), the Amd 2
  per-channel `pixi` (`px_flags & 1`: `channel_idc`,
  `component_format`, Table 14 subsampling, labels —
  `Property::PixiExtended`, `ItemProperties::pixi_channels`; `pixi()`
  still answers the depths) and `tilC` + `tipa` (§6.11.3). Entity
  groups keep their post-`entity_id` bytes (`EntityGroup::payload`)
  with typed readers for `pymd` (§6.8.12 tile sizes), `rgpa`
  (§6.8.13 area), `stem` (§6.8.11 left / right / monoscopic fallback
  and its position); `HeifWriter::add_pyramid`,
  `add_stereo_with_fallback`, `add_entity_group_with_payload`.
- MIAF Amd 1:2025 Annex A (implementation practices):
  `Meta::display_order` (A.3 / A.4: displayable masters in `iinf`
  order with `altr` groups collapsed) and `Track::loop_behaviour`
  (A.2 Table C.1 from the `elst` `RepeatEdits` flag and the `tkhd`
  duration, now kept as `Track::repeat_edits` / `track_duration`);
  `SequenceWriter::looping` / `with_looping` writes the matching
  `edts` / `tkhd` / `mvhd` durations.
- Fuzz: `heif_records` covers the new property types, `heif_parse`
  the group payload readers and the display order, `heif_sequence`
  the loop behaviour.

- Finals reconcile (ISO/IEC 23008-12:2025/Amd 1:2025, ISO/IEC
  23000-22:2025/Amd 1:2025, ISO/IEC 14496-12:2026 — the published
  texts replace the MPEG drafts the `tmap` / HDR-box work was built
  from). `tmap`: the reconstruction is the map "fully applied (i.e.
  with a weight of 1.0 or -1.0 depending on the gain map metadata)"
  (§6.6.2.4.1) — `GainMapMetadata::fully_applied_weight`, equal to
  Formula 3 at `H_alternate`; doc citations follow the final (plain
  "ISO 21496-1", terms 3.1.54–56); the alpha carry-over is documented
  as this crate's reading (the final is silent; the 4th-ed. WD
  paragraph is draft-only). `clli` / `mdcv` / `cclv` / `amve` and
  `csgp` (flag bit 7) already matched the 2026 edition (editorial
  differences only). `miaf::check`: MIAF Amd 1 §7.3.11.5 — a `tmap`
  shall be in an `altr` group with a non-hidden master image item
  (`"MIAF-A1 7.3.11.5"`), inputs should share rotation / mirroring;
  HEIF Amd 2 §6.5.6.3 `pixi` depth 0 refused; new
  `MiafReport::advisories` for the should-level rules (hidden gain
  map, `tmap` `pixi` hint, input orientation).

## [0.0.4](https://github.com/OxideAV/oxideav-heif/compare/v0.0.3...v0.0.4) - 2026-09-26

### Other

- a producer that writes a non-ISOBMFF file under the .heic name is a refusal
- absent black-box decoder is a "no reference" cell; single-reader hosts report instead of judging
- bound the pattern expansion (fuzz finding); matrix: 12 MP rows opt-in
- conformance matrix + performance baseline + record fuzzing
- keep HeifFile owned and additive — zero-copy as HeifFile<D = Vec<u8>> / parse_borrowed; L-HEVC refusal under Unsupported
- alpha auxiliary tracks composed into frames with alpha; proportional alpha depth matching
- avc1 + lhv1 items: avcC / lhvC / oinf / tols typed, AVC through the h264 codec, L-HEVC base layer through h265
- hdr boxes: ISO/IEC 14496-12 8th ed. §12.1.6–9 wire syntax (plain Boxes, interleaved primaries, typed on sample entries)
- normative carriage per 23008-12:2025/Amd 1 §6.6.2.4 + §10.2.6 (reader, checker, writer)
- zero-copy HeifFile, interleaved mdcv/cclv, 4:2:2 turns, mono overlays, writer brands/aliasing/raw metadata, sample groups

- `tests/conformance_matrix.rs`: the both-direction conformance matrix
  generator — producers (Apple ImageIO `sips`, `heif-enc` x265 / aom,
  ImageMagick, ffmpeg + libsvtav1) × features (sizes 1×1 … 12 MP,
  8 / 10 / 12-bit, 4:0:0–4:4:4, alpha, lossless, thumbnail, Exif /
  XMP / ICC, `irot` / `imir` / `clap`, sequence, gain map) decoded by
  this crate and compared byte-exact with the black-box video decoder
  (`no producer` / `producer refused` / `dropped` / measured Δ
  otherwise), and our writer's shapes × readers (`sips`,
  `heif-convert`, `magick`, `ffmpeg`, `heif-info`) with the render
  delta against our decode. Prints both tables as Markdown (README
  "Conformance matrix"); runs with SKIPs where binaries are absent.
  `tests/common/pngw.rs`: a stored-deflate PNG writer for the sources.
- `examples/heifbench`: the performance baseline tool — decode
  wall-clock (median of N) + peak RSS of a child decode, encode
  wall-clock at default HEVC / AV1 settings, Markdown out (README
  "Performance baseline (r462)").
- `fuzz`: `heif_records` target (tmap body, HDR boxes, `avcC` /
  `lhvC` / `oinf` / `tols`, `sbgp` / `csgp` / `sgpd`, `prft` /
  `ssix`, every typed property, with a serialize → parse round-trip
  oracle); `heif_parse` covers tmap bodies, entity-group flags and the
  borrowed `HeifFile` view; `heif_sequence` covers sample groups and
  auxiliary-track lookups. First finding fixed: a `csgp` pattern over
  2³² samples walked every sample (`MAX_SAMPLES` bound up front, one
  run per single-index pattern, `MAX_CSGP_RUNS` = 2¹⁶ on the expansion).

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
