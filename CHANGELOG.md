# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- **`lhv1` items whose enhancement picture is not an IRAP picture decode** (round 488; oxideav-h265 0.0.18): H.265 F.8.1.6 withholds the picture of a layer that is not initialised, and such an item was a typed refusal. An HEVC image item is independently coded (HEIF B.2.2.1.1) and holds one access unit (B.2.2.1.3), so the picture is predicted from that access unit alone: the decoder is asked for it (`output_uninitialized_layers`). The three `TRAIL_R` `lhv1` items of multilayer002 equal the elementary stream's layer-1 pictures byte for byte and the file decodes whole. `ItemDecoder::with_irap_layers_only(true)`, `DecodeOptions::with_irap_layers_only`, the `"heif"` decoder's `irap_layers_only` option (`HeifDecoderOptions::with_irap_layers_only`) keep the refusal (and `base_layer_fallback` the base layer under it).
- **Layer-set decode for the output layers an `oinf` names** (round 488): an `lhv1` item without `lsel` whose codec-derived output layer set yields other layers than its `oinf` lists (a VPS that outputs only its highest layer, an auxiliary layer above the primary one) is decoded through the HEVC decoder's layer-set target and the listed layers are taken from it — registry path and `exbl` path. `tests/aux_layers.rs`: an alpha layer carried as an `lhv1` auxiliary item (`auxC` `urn:mpeg:hevc:2015:auxid:1`, `lsel`) over the access unit of its `hvc1` master — master byte-exact against the black-box decoder, alpha within 1 code of the source ramp.

- **Fuzz record** (round 488, bounded foreground runs, 2 GiB RSS cap, 30 s per-input timeout, no finding): `heif_compose` 2 min / 417 k runs — now holding an oriented grid canvas (a chain of up to three rotations / mirrors from the input, every other tile through the turner) to the composed grid taken through the chain one operation at a time; `heif_framework` 100 s / 135 k; `heif_decode` 100 s / 4.0 k.
- `tests/oriented_canvas.rs` (written `grid` / `tili` files with transforms decode to the untransformed file's picture taken through the chain, 1 and 4 workers) and `tests/demux.rs::factory_decoder_frames_equal_the_keeping_decoder_s`.
- **Item sweep** (round 488, `tests/item_sweep.rs`): every image item of every file the checkout sees decoded on its own (`decode_item`), refusals counted by gate and none allowed outside the known ones — 402 image items in 152 files: 384 decode, 13 protected (no cipher), 4 `hvt1` without `rloc`, 1 thumbnail carrying a `clap` larger than its picture (MIAF007).

### Changed

- **A rotated grid is composed once** (round 488): the `irot` / `imir` entries that lead a `grid` item's transformative chain are folded into the canvas (`compose::GridCanvas::with_reorientation`, `Reorient`, `TileTurner`, `CanvasTile`, `GridCanvas::place_tile` / `finish_threads`, all `#[doc(hidden)]`) — tiles go straight to their oriented places, turned by the decode workers under a budget; `tili` items (in-file and external tiles) take the same path; 4:4:4 / monochrome always, 4:2:0 at even sizes, the chain applied step by step otherwise. An `irot` of angle 0 no longer copies the picture. Same bytes; 12 MP device still 49 → 32 MiB (1 worker), 69 → 50 MiB (8), the 8.5 MP one 60 → 35 MiB.
- **The registry path stops copying pictures** (round 488): the factory-made `"heif"` decoder moves the decoded planes into its frame (`HeifCodec::with_last_image(false)`; a decoder built by hand keeps `last_image` as before) — 52 → 34 MiB for the 12 MP still; the `"heif"` encoder converts a packed frame from the frame's own rows without copying it first and cuts a planar frame already in its coding layout into tiles in place — 109 → 74 MiB (packed RGB) and 85 → 50 MiB (planar 4:2:0) for a 12 MP still at 1 worker, same bytes.
- **Sequence pictures in flight are bounded by `EncodeOptions::sequence_pixels_in_flight`** (round 488; `with_sequence_pixels_in_flight`), default `SEQUENCE_PIXELS_IN_FLIGHT` 2²⁴ → 2²² (two 1080p pictures): 16 × 1080p HEVC on 8 workers 503 → 229 MiB (serial: 181) at 1.90 → 6.55 s; `Some(1 << 24)` gives the former trade back. The bytes do not depend on the bound.
- `heifencbench --fmt yuv420p` (a planar frame through the registry encoder) and `--in-flight PIXELS`.
- **oxideav-h265 floor 0.0.18.** Its VPS reader takes a VPS whose RBSP ends before `vps_extension_flag`: the base item of `iff_hevc_tile_multiple_items_tbas` decodes. Decode matrix: 152 files = 137 whole + 13 with keys + 2 in part, none refused (r486: 136 / 13 / 2 / 1).

## [0.0.15](https://github.com/OxideAV/oxideav-heif/compare/v0.0.14...v0.0.15) - 2026-11-11

### Other

- README — the r487 sections (alpha information draft status, what the errata note settles for cfen / cexg / tmap, encode side on the thread budget with before → after numbers, the codec profile shares and the packet-payload ask); heif_records fuzzes alpx bodies; fuzz record
- the encode side spends its thread budget — the alpha plane and depth map of a tiled picture are tiled like their master (a hidden grid with the auxC over hidden tiles, or a tili item under that carrier) instead of one whole-picture item that took none of the budget; image sequences are prepared and coded picture by picture as ordered parallel jobs (HEVC / AV1 / VVC; one picture in flight per worker, at most 2^24 pixels in flight; AVC stays one bitstream in order); tiles are cut from the caller's planes when the picture is already in its coding layout (encode_still / encode_still_into / the contract's encode: no owned copy of the picture); a failed tile / sample job stops the rest from starting; bytes identical for every budget; heifencbench --alpha / --tiling / --frames; tests: tiled auxiliaries (structure, lossless, budgets, readers), borrowed tiles, parallel sequences, banded conversion
- packed RGB(A) converts on the thread budget — the packed → coding-layout conversion is cut into bands of whole chroma row groups, one per worker (packed_to_planar_threads; pictures under 2^18 pixels stay serial), each band writing its own rows of every plane: the planes of the serial conversion for every worker count; the registry encoder and the contract's packed entry points hand it their budget
- CHANGELOG — sequence sample entries carry the cropped frame size
- sequence sample entries carry the cropped frame size — encode_all hands sequence pictures to the codec at their own size (HEVC / VVC on the chroma grid, AV1 as they are) instead of block-padding them itself, so the codec crops with its conformance window and the sample entry, tkhd and decoded picture agree without a clap for every size on the chroma grid (ISO/IEC 14496-12 §8.3.2 / §8.5.2, 14496-15 §4.5); clap remains for the odd edge of a subsampled HEVC / VVC picture and AVC's macroblock padding; the alpha track follows its master; tests/tkhd.rs pins a unit §8.3.2 scale for every codec and a black-box reader presenting the intended size with square pixels
- protected items re-mux — import_item / from_files carry a protected item as it is (encrypted body, ienc, the sinf of its ipro entry byte for byte, the auxiliary information item behind auxr with its iaux), no cipher involved; the writer keeps one ipro entry per distinct sinf payload; 24 written + 13 staged encrypted files re-mux to files that decrypt to the same picture and re-mux to themselves
- cexg with an ExtentDecoderConfigurationRecord decodes under the item's own decoder configuration — the record (Amd 1:2025 §6.5.41.3) is defined by no staged coding-format binding and stays opaque bytes; an item whose only configuration is the record stays a typed Unsupported; cfen K (7) refusal and docs state what the texts do and do not settle about the CMYK column
- alpha information box / property alpx (draft status) + mini native alpha — AlphaInformation parse / write per the latest public draft of ISO/IEC 14496-12 §12.11.5.2 (premultiplication_mode in flag bits 0-1, is_float = 4, 16-bit integer or 32-bit float opaque / transparent values; no published text defines the box and its four-character code is unsettled, documented on the type); as an item property of an alpha auxiliary and in an alpha track's sample entry the two values normalise the plane to the §6.9.1 convention and modes 1 / 2 report pre-multiplied colour; a mini box with alpha_flag = 1 and alpha_item_data_size = 0 expands (Amd 2:2026 O.4.6 entry 30, essential on item 1; the main pixi leads with the alpha channel, O.4.7.1) instead of being refused, opaque_value reproduced as printed (the bit depth) and documented as interpretation-bound; tests/alpha_information.rs
- SequenceSample is public — it was doc(hidden) while SequenceAlphaTrack::new / SequenceMetadataTrack::new take a Vec of it; SequenceSample and SequenceMetadataTrack re-exported at the crate root, SequenceAlphaTrack::push_sample / with_samples; additive
- HEVC tile items (hvt1 / tbas / rloc), registry-path bench, lookup oracle in the parse fuzz target
- partial decode, protected-item writer, out-of-order ipma, item-model lookups by table, banded irot
- Common Encryption of image items (ienc / iaux / auxi, cenc / cens / cbc1 / cbcs)
- L-HEVC enhancement-layer decode (oxideav-h265 0.0.17) + predictively coded items
- API hygiene: non_exhaustive HeifPixelFormat/BoxHeader + builders

### Added

- **Fuzz record** (round 487, bounded foreground runs on the final targets, 2 GiB RSS cap, 30 s per-input timeout, no finding): `heif_records` 2.5 min / 805 k runs (now round-tripping `alpx` bodies and expanding `mini` boxes that declare native alpha), `heif_parse` 100 s / 371 k, `heif_framework` 100 s / 191 k, `heif_decode` 100 s / 12.8 k, `heif_writer` 100 s / 12.1 k (the tiled auxiliary planes on every encoder-driven input).
- **Alpha information box / property `alpx` — draft status** (round 487): `props::AlphaInformation` (`AlphaPremultiplication`, `AlphaValues`), `Property::Alpx`, `ItemProperties::alpha_information`, `SampleEntry::alpha_information` parse and write the `AlphaInformationBox` of the latest public draft of ISO/IEC 14496-12 §12.11.5.2 (WG 03 N1702, "potential improvements" on DAM 2, *Carriage of depth and alpha*): a version-0 FullBox, `premultiplication_mode` in flag bits 0–1, `is_float` = flag 4 selecting `float(32)` over `unsigned int(16)` `opaque_value` / `transparent_value`; containers `ipco` and a visual sample entry. **No published text defines the box and its four-character code is not settled** (an earlier ballot text and the HEIF 4th edition working draft print `alpi`); the type and every doc comment say so. On decode an alpha auxiliary item (or an alpha track's sample entry) carrying it has its plane brought to the §6.9.1 convention (0 transparent, maximum opaque) through the two values (`compose::normalise_alpha`; the draft's defaults change nothing) and `premultiplication_mode` 1 / 2 report pre-multiplied colour like a `prem` reference. `tests/alpha_information.rs`.
- **`mini` files declaring native alpha expand** (round 487; HEIF Amd 2:2026 O.4.6 entry 30): `alpha_flag` = 1 with `alpha_item_data_size` = 0 was a typed refusal ("`AlphaInformationProperty`, which Amd 2 does not define"); the expansion now writes entry 30 as the draft `alpx` property (flags `alpha_is_premultiplied + float_flag × 4`, exactly the draft's flag layout), essential on item 1 between entries 5 and 11, and the main `pixi` leads with the alpha channel (O.4.7.1). `opaque_value` is reproduced as printed — the channel's *bit depth*, the `bits_per_channel` expression — and documented as not being a usable sample value (`MinimizedImage::native_alpha_information`); the item decodes to its colour picture (no codec here codes alpha inside the item).
- **`SequenceSample` is public** (round 487): it was `#[doc(hidden)]` while `SequenceAlphaTrack::new` and `SequenceMetadataTrack::new` take a `Vec` of it. `SequenceSample` and `SequenceMetadataTrack` are re-exported at the crate root next to `SequenceAlphaTrack`; `SequenceAlphaTrack::push_sample` / `with_samples` queue samples without naming the record. Additive.
- **Protected items re-mux** (round 487): `HeifWriter::import_item` / `from_files` refused a protected item; it is imported as it is — the encrypted body, the `ienc`, the `sinf` of its `ipro` entry byte for byte, the auxiliary information item behind `auxr` with its `iaux` — with no cipher involved. The 24 protected files of `tests/protected_items.rs` and the 13 staged encrypted conformance files re-mux to files that decrypt to the same picture under the same keys, stay a typed refusal without one, and re-mux to themselves byte for byte.

- **HEVC tile items** (round 486; HEIF B.2.5, §6.5.7): an `hvt1` item decodes as the region of its `tbas` HEVC image item's reconstructed image at its `rloc` offset, `ispe` in size — the picture's own layout, no promotion (an offset off the chroma grid is `Unsupported`), the picture decoded once however many of its tile items a file shows (a `grid` of them, a burst). An item without exactly one `tbas` reference to an `hvc1` / `hev1` item, without `rloc`, or whose region leaves the picture is `InvalidData`; `hvt1` items were handed to the HEVC decoder as whole pictures before. `tests/tile_items.rs`: four pictures (4:2:0, 4:4:4, 10-bit, 4:0:0) × four quadrant tiles + an off-centre one, each sample-exact against the picture, the grid of tile items equal to it on the contract, threaded and framework paths; eight refusal shapes.
- **`decode_all_partial`** (round 486): `decode_all_with` for a viewer that shows what it can — an entry that does not decode (a displayable item none of whose `altr` alternatives decodes, an image-sequence track one of whose samples the codec refuses) is listed in `PartialDecode::skipped` (`SkippedEntry`: the item ids tried or the track id, and the error) instead of failing the file; the frames returned are exactly `decode_all`'s for them, a file of which nothing decodes is still an error. C046 (a track whose first sample references a picture no sample holds) yields its still, multilayer002 six of its ten entries.
- **Decode matrix legs** (round 486, `tests/framework_decode_matrix.rs`): the 16 files `decode_all` refuses are measured further — the 13 encrypted ones decode on both paths under their `drm_cfg` test keys (the framework decoder as `HeifCodec::with_decode_options`, a black-box AES-128 shared with `tests/encrypted_items.rs` through `tests/common/cipher.rs`), C046 and multilayer002 through `decode_all_partial` against the framework's per-packet frames: 152 files = 136 whole + 13 with keys + 2 in part + 1 refused, 0 disagreements.
- `Meta::ipma_unordered` and the `miaf::check` violation `"ISOBMFF 8.11.14.1"`.
- `examples/heifscale` (container-side scaling bench) and `heifbench --framework` (the registry path timed and measured next to the direct one, same output digest).
- Fuzz: `heif_parse` holds every id lookup of the parsed tree to the plain walk over its vectors (items, locations, `ipma` rows, references both ways, entity groups); an 83-item seed puts the corpus past the lookup-table threshold in `heif_parse` / `heif_decode` / `heif_framework` / `heif_api`.
- **Writing protected items** (round 486; ISO/IEC 23001-7 Amd 1 §7.1 / §8.3 / §8.4): `cenc::encrypt` (the inverse of `decrypt` for `cenc` / `cens` / `cbc1` / `cbcs` — one shared walk over the ranges, counters, chains and patterns), `CencAuxData::new` / `to_bytes` (single- and multiple-key `CencSampleAuxiliaryDataFormat`), `cenc::nal_subsamples` (subsamples over length-prefixed NAL units: length fields, chosen units and a lead of each protected unit clear, optional 16-byte alignment) and `HeifWriter::protect_item` — the body encrypted through the caller's `BlockCipher`, the essential `ienc`, the `ipro` / `sinf` / `schm` entry and `item_protection_index`, the auxiliary data as an `auxi` item with an essential `iaux` behind `auxr`. A constant IV under a counter scheme and a pattern under `cenc` / `cbc1` are refused. `tests/protected_items.rs` (runs without the staged files): 24 protected files decode to the unprotected picture on the contract and framework paths, refuse typed without a cipher, never decode under another key; the vendored inter-coded sequence written as `pred`-referencing `hvc1` items decodes to its track pictures.
- **Fuzz record** (round 486, bounded foreground runs on the final targets, 2 GiB RSS cap, 30 s per-input timeout, no finding): `heif_records` 4 min / 1.08 M runs, `heif_parse` 2 + 2.5 min / 0.91 M, `heif_decode` 2.5 + 2 min / 36 k, `heif_compose` 1 min / 300 k, `heif_api` 1 min / 160 k, `heif_framework` 1 min / 97 k, `heif_writer` 1 min / 5.9 k.
- **Fuzz coverage of the r485 surface** (round 486): `heif_records` walks Common Encryption on arbitrary records (round-trip and identity oracles for `ienc`, `iaux`, the auxiliary data, `encrypt` / `decrypt` under every scheme, `nal_subsamples`), `heif_decode` decodes under a toy cipher with both readings, the base-layer fallback and the `decode_all_partial` / `decode_all_with` oracle; new seeds: protected files (8 protections), an `lhv1` stereo pair, a `pred` item chain, five record seeds.
- `HeifFrame::new`, `BoxHeader::new`, and `with_<field>` setters on `HeifFrame`, `HeifPixelFormat` and `BoxHeader`.
- **L-HEVC enhancement-layer decode** (round 485; oxideav-h265 0.0.17 multi-layer decoder, floor raised to `>=0.0.17, <0.1`): `lhv1` items without an `hvcC` decode every layer — the `lhvC` parameter sets (VPS / SPS / PPS first) in band ahead of the access unit, the `tols` output layer set or the `lsel` layer as the operating point — instead of the base layer alone; an `lhv1` item with an `exbl` reference (HEIF B.2.2.1.3) decodes the referenced item (an `avc1` base, say) first and hands its picture to the HEVC sequence decoder as the H.265 F.8.1.8 external base layer. A decoded layer the codec does not tag with the expected `nuh_layer_id` (a bitstream whose VPS lacks the layers the `oinf` names) is the typed `HeifError::layered_hevc` refusal; an `lhv1` item whose output-layer picture is not IRAP is a typed refusal (B.2.2.1.3 NOTE 1, H.265 F.8.1.6: no output for an uninitialised layer), `base_layer_fallback` yields its base. The five MPEG multi-layer conformance files decode: base items and both MV-HEVC views byte-exact against the black-box decoder, SHVC SNR / hybrid enhancement layers equal to the producer's streams through the sequence decoder (`tests/multilayer_conformance.rs`).
- **Predictively coded items** (HEIF §6.4.9): an `hvc1` item with `pred` references decodes the bitstream its references' item data and its own form and yields its own access unit's picture — C043 / C044 byte-exact against the black-box decoder (`tests/predictive_items.rs`); other coding formats refuse typed. `import_item` brings `exbl` / `pred` dependencies along.

- **Encrypted items** (ISO/IEC 23001-7 Amd 1 §8.3 / §8.4): new `cenc` module — `ItemEncryption` (`ienc`), `ItemAuxInfo` (`iaux`), `CencAuxData` (single- and multiple-key `CencSampleAuxiliaryDataFormat`), `ProtectionScheme` (`ipro` / `sinf` / `frma` / `schm`, `Meta::protection_schemes` / `Meta::protection_scheme`), `decrypt` for `cenc` / `cens` / `cbc1` / `cbcs` (§9.3–§9.7, §10) around a caller-supplied `BlockCipher` (AES-128 by KID; no cipher or key material in the crate); `Property::Ienc` / `Property::Iaux`; `ItemDecoder::with_decryptor` / `with_cenc_reading`, `DecodeOptions::decryptor` / `cenc_reading`; `HeifError::encrypted_item` / `encrypted_item_info` (the refusal without a cipher, replacing "unrecognised essential ienc"). `CencReading::compat()` also accepts the staged conformance files' departures (swapped pattern nibbles, 16-bit `multi_subindex_IV`, `cbcs` per-sample-IV chaining). `tests/encrypted_items.rs`: 4 of 13 strict, 13 of 13 compat, one picture, byte-exact vs the black-box decoder.

### Performance

- **The auxiliary planes of a tiled picture are tiled** (round 487): the alpha plane (and a depth map) of a picture coded as a grid was one coded item — a whole-picture codec instance (hundreds of MiB at 12 MP) that took none of the thread budget, so a 12 MP RGBA still spent most of its 8-worker wall on it. The plane is now cut into the master's tile size and coded as a hidden `grid` of hidden tiles under the same budget — an auxiliary image item is any image item with an `auxC` and an `auxl` reference (HEIF §6.4.5; MIAF §6.5), coded or derived — with a single-channel `pixi`, no colour information and the master's transforms. Byte-identical for every budget; lossless modes return the plane exactly; the black-box readers that showed the alpha of the single item show the grid's (alpha rendered sample-exact by the OS reader). 12 MP RGBA HEVC still (the device photograph, production defaults): 8 workers 3.47 s / 373 MiB → 1.12 s / 159 MiB, serial 347 → 119 MiB.
- **Image sequences are coded picture by picture on the thread budget** (round 487): `encode_all` coded the samples of a sequence one after the other whatever the budget (HEVC / AV1 / VVC code every sample independently). The pictures are now prepared (converted to their coding layout) and coded as parallel jobs, one picture in flight per worker, the coded samples handed to the writer in order; a budget left over by a short sequence goes to each picture's codec; the pixels in flight are bounded (`SEQUENCE_PIXELS_IN_FLIGHT`, 2²⁴: eight 1080p pictures, one 12 MP one) because each worker holds a picture of codec state. The file is the serial one for every budget; an AVC sequence (one bitstream per track) stays in order. 16 × 1080p HEVC: 8.39 s at every budget → 2.34 s on 4 workers, 1.39 s on 8 (resident set 195 → 514 MiB: eight pictures of codec state; serial 197 → 149 MiB, the pictures are prepared one at a time).
- **Packed RGB(A) converts on the thread budget** (round 487): the packed → coding-layout conversion ran on one thread — a tenth of a parallel 12 MP HEVC encode and more than half of a lossless one. It is cut into bands of whole chroma row groups, one per worker (`packed_to_planar_threads`; pictures under 2¹⁸ pixels stay serial), each band writing its own rows of every plane: the planes of the serial conversion for every worker count. 12 MP lossless RGB through the registry encoder, 8 workers: 0.132 → 0.075 s.
- **Tiles are cut from the caller's planes** (round 487): `encode_still` / `encode_still_into` / the contract's `encode` copied a planar picture into an owned coding picture before cutting its tiles. A picture already in its coding layout that is coded as tiles (and has no thumbnail to scale) is now used where it is — padded strides included — so the encode holds no second copy of the picture. 12 MP HEVC still through `encode_still(&frame)`: 83 → 68 MiB serial, 145 → 117 MiB on 8 workers.
- A failed tile / sample job stops the remaining jobs from starting (they used to run to the end before the error was returned).

- **Item-model lookups are table reads on large trees** (round 486): `Meta::item` / `location` / `properties_of` / `associations_of` / `references_from` / `references_to` / `groups_containing` / `display_order` walked their vectors on every call, so every per-item loop — the derivation graph of a grid, `info`, `miaf::check`, the demuxer's stream description, a re-mux — was quadratic in the item count. A tree of 64 records or more builds id → position tables on its first lookup (positions only; an entry is used only when the record at its position still carries the id, checked in constant time; pushes and removals fall back to the walk, `Meta::reindex` rebuilds after ids are rewritten in place). `examples/heifscale` (new) on a 16 385-tile grid: `info` 160 → 16 ms, graph 153 → 10.5 ms, `miaf::check` 281 → 22.5 ms, demuxer open + first packet ≈ 37 ms, re-mux 1 424 → 117 ms; on a 32 768-item burst the per-item property / data walk 332 → 7.8 ms. README section *Container scaling*.
- **`HeifWriter` writes in one pass over its references** (round 486): the `dimg` / `auxl` closures (mdat order, brand probe), the per-item thumbnail test and the reference validation were item × reference scans — 16 385-tile grid 1 175 → 120 ms, 32 768-item burst 2 830 → 57.5 ms; bytes unchanged.
- **`irot` on large pictures** (round 486): the rotation fills its output in bands of 32 rows (each source row read once per band, in memory order) instead of one sample at a time through the generic accessors, and planes above 2¹⁸ samples are rotated on the item decoder's thread budget (`compose::apply_irot_threads`); byte-identical to the per-sample definition on every layout and budget (unit oracle). The 12 MP device still (48-tile grid + `irot`): the rotation 18 → 10.5 ms serial, the 8-worker decode 0.111 – 0.127 → 0.091 – 0.094 s, output digest unchanged.

### Fixed

- **Sequence sample entries carry the cropped frame size** (round 487; ISO/IEC 14496-12 §8.3.2 / §8.5.2, ISO/IEC 14496-15 §4.5): `encode_all` padded every sequence picture to the codec's block size itself (16 for HEVC, 8 for AV1) and described the padded picture in the sample entry with a `clap` back to the visible size. The staged text puts block padding inside the codec ("expected to use codec-specific cropping structures, that are not exposed at the ISOBMFF level"; the sample entry documents "the maximum cropped frame dimensions") and has a reader scale by `tkhd` / sample entry when it finds no clean aperture — a black-box reader that does not consult the `clap` reported the padded size with a non-square sample aspect ratio. Sequence pictures now go to the codec at their own size (HEVC / VVC: brought to the chroma grid only; AV1: as they are), the codec crops with its conformance window, and the entry, the `tkhd` and the decoded picture agree with no `clap` at all for every size on the chroma grid (a 60×44 HEVC track: entry 64×48 + `clap` → entry 60×44; AV1 61×45: entry 64×48 + `clap` → 61×45). What is left to `clap`: the odd last column / row of a subsampled HEVC / VVC picture (entry 62×46, clean aperture 61×45 = `tkhd`) and AVC's macroblock padding (the AVC encoder writes no frame cropping). The alpha track follows its master. `tests/tkhd.rs`: the §8.3.2 scale factor is 1 on both axes for every codec, a black-box reader reports the intended size with square pixels.
- **Counts past their box field are refused** (round 486, found by the scaling bench): a reference box of more than 65 535 targets — a 256 × 256 grid — and an item of more than 65 535 `iloc` extents wrapped their 16-bit counts and wrote a corrupt file; both are `LimitExceeded` now.
- **`apply_irot` on a frame whose plane is shorter than its geometry** is `InvalidData` (it indexed past the plane).
- **An `ipma` whose rows are out of `item_ID` order is read** (round 486; ISO/IEC 14496-12 §8.11.14.1 orders them): the whole file used to be refused at parse (`iff_hevc_tile_multiple_items_tbas`: `info`, the item tree and re-mux now work; its decode is gated on its `hvcC` VPS, which ends before `vps_extension_flag`). The same item twice in one box stays an error.
- **`hvc1` items carrying enhancement layers** (HEIF B.2.2.1.2: readers ignore NAL units with `nuh_layer_id` > 0 in an `hvc1` item): such items (multilayer001 / 002 / 005) were refused ("pps never activated"); their layer > 0 units are dropped and the decoder is pointed at layer 0.
- **`lhvC` writer reserved bits**: the `bit(2) reserved = '11'b` ahead of `numTemporalLayers` (ISO/IEC 14496-15 §9.5) was written as `00`; re-muxed `lhv1` items now carry their `lhvC` byte for byte.

### Changed

- **`cexg` with an `ExtentDecoderConfigurationRecord`** (round 487; HEIF Amd 1:2025 §6.5.41): the record is defined by no staged coding-format binding, so it stays opaque bytes (`Cexg::extent_config`); an item that also carries its ordinary decoder configuration property now decodes with that one instead of being refused, an item whose only configuration is the record stays a typed `Unsupported`.
- API hygiene: `HeifPixelFormat` and `BoxHeader` are now `#[non_exhaustive]`. `Chroma` (the four ISO/IEC 23008-12 chroma structures) stays exhaustive; `HeifFrame` stays exhaustive for now because sibling crates still build it with a struct literal.

## [0.0.14](https://github.com/OxideAV/oxideav-heif/compare/v0.0.13...v0.0.14) - 2026-11-06

### Other

- CHANGELOG — heif_writer fuzz invariant and the external-tile iloc shape (pending the docs-gap ruling)
- heif_writer fuzz invariant exempts exactly the external-tile iloc shape — a tili item naming an external deti through data_reference_index keeps one (0, 0) extent (Amd 2 §6.11.4: no in-file payload; 14496-12 §8.11.3.3: extent_count >= 1), recognised from the parsed file; every other item keeps the no-zero-length rule; pending the docs-gap ruling; regression test on the written and re-muxed file; fuzz core floor 0.1.43
- Frame::StreamParams both ways (oxideav-core 0.1.43) — the "heif" and VVC item decoders answer output_params / output_params_generation and emit one event immediately before the first picture of a new shape (none for the first shape or a constant stream; payload == output_params; FIFO order through flush; reset keeps the reference shape); item / grid / tili / layer / sequence decodes read the pictures after a codec's event at its announced size and layout, never as a picture; the encoder refuses it as Unsupported; layer-identity side channels audited (image planes only); stub-decoder tests through a caller CodecRegistry; core floor 0.1.43
- tkhd width / height are the presentation size (ISO/IEC 14496-12 §8.3.2.3) — SequenceWriter wrote the padded coded size of odd-sized tracks; the entry's clap clean aperture (16.16 kept) and a non-square pasp now set it, the sample entry keeps the coded size; tests/tkhd.rs + unit test
- sato pixel transform derived items (23008-12 4th edition WD §6.6.2.6) — PixelTransform parse / validate (§6.6.2.6.4 stack rules; unknown version or reserved token = ignorable Unsupported) / serialise / saturating evaluation at 8–64-bit precision, composition clamped to the item's pixi depth and the nclx nominal range; decode, layout prediction, HeifWriter::add_pixel_transform, import_item re-mux, HEIF-WD 6.6.2.6.1 MIAF checks; encode_pixel_transform_still codes 9..=16-bit pictures losslessly as two 8-bit HEVC / AV1 items + the Annex J.9.1 transform; Annex J.9.1 / J.9.2 files decode sample-exact; fuzz (heif_records evaluates arbitrary bodies) + seeds for sato / external tili
- external tili tiles (Amd 2:2026 §6.11.2 / §6.11.5) — composed through a caller-supplied ExternalTileResolver (ItemDecoder / DecodeOptions::with_tile_resolver; requests carry index, row / column, tile ID, directory ID, the Table 12 URL), a typed external_tile_unresolved refusal naming the first URL without one; tile files checked against the §6.11.2 shalls, one coding format and the tilC tile size; HeifWriter::add_external_tiled_item + import_item re-mux; HEIF-A2 §6.11 checks in miaf::check; fuzz coverage (heif_decode serves tiles from the input, heif_records round-trips external deti entries); tests/external_tiles.rs
- edition 2024, MSRV 1.95
- r479 fuzz record — heif_writer 9 min / 58 364 runs with the pool, index-form and extent invariants, heif_sequence 5 min / 1.22 M runs with the udta / cprt walk; no finding
- degenerate derived items refused — add_grid bounds the layout to 1..=256 rows / columns (a zero-row grid underflowed the rows_minus_one byte, a 257-row one wrapped it) and a non-zero canvas, add_overlay needs one or more inputs and a canvas (HEIF §6.6.2.2 / §6.6.2.3); README + CHANGELOG
- track-level cprt both ways — Track::copyrights parsed from trak/udta (§8.10.2: a cprt in a track applies to that track), SequenceWriter::with_track_copyright writes it on the picture track, the demuxer reports track:<id>:copyright[:<lang>]; the cprt sequence file is asserted to open in heif-info / ffprobe / sips; heifdump prints the property pool (entries, rows, associations, ipma index form)
- conformance matrix measures the property pool — 285-tile HEVC / AV1 grids (exact in heif-convert / magick / ffmpeg, which refused the 289-entry ipco before; opens in heif-info and sips) and a 1 024-tile grid (a measurement: the readers' 1 000-item iloc / stream ceiling, gated on sips + this crate); README matrix rows + CHANGELOG
- the ipco is a pool — every distinct property box interned once (decoder configurations included: a 12 MP 256-px grid goes from 196 entries to 5, a 1 024-tile grid from 1 028 to 5; the two readers refusing the 192-tile file at their 100-box ipco limit open it, measured table in the README), 15-bit ipma indices verified on both sides of 127 + a typed refusal above 255 associations; no degenerate items (an empty body is refused by kind at write time — a 0-length iloc extent means "to the end of the source", 14496-12 §8.11.3.2.4.3; the cover-alias placeholder is the one carve-out); core 0.1.41 (encoder embedded_blob_kinds = ICC / Exif / XMP; sequence muxer set_metadata / carries_metadata_key carry `copyright` / `copyright:<lang>` as moov/udta/cprt, the demuxer reports them from Movie::copyrights); heif_writer fuzz asserts a duplicate-free pool, the right index form and no zero-length extent on every written file (9 min, no finding)

### Changed

- edition 2024, MSRV 1.95

### Fixed

- **`heif_writer` fuzz target red on the external-tile seed** (round 482; daily Fuzz run on 3b0736a): the r479 invariant "no written file carries a zero-length `iloc` extent" fired on the re-mux of the r481 `tili` external-tile seed. The staged text gives such an item no in-file payload (HEIF Amd 2:2026 §6.11.4 defines one only for `external_tiles_urls = 0`) while ISO/IEC 14496-12 §8.11.3.3 requires `extent_count` ≥ 1, so the writer keeps its single extent at offset 0 of length 0 naming the `deti` (§8.11.3.2.4.3 1c: the whole referenced resource) and the invariant now exempts exactly that shape — a `tili` item, construction method 0, `data_reference_index` naming a `deti` with `external_tiles_urls` set, one extent (0, 0) — recognised from the parsed file, never by item id; every other item keeps the rule. Pending the docs-gap ruling on how an external-tile item's `iloc` entry is meant to look. `tests/external_tiles.rs::external_tile_item_iloc_is_one_empty_extent_naming_the_deti` pins the shape on the written and re-muxed file; the crash input is the corpus seed `minimal.tili-external.heic`. The fuzz harness's `oxideav-core` floor follows the crate's (0.1.43).
- **`tkhd` width / height carry the presentation size** (round 482; ISO/IEC 14496-12 §8.3.2.3 — the track's visual presentation size, which "need not be the same as the pixel dimensions of the images, which is documented in the sample description"): `SequenceWriter` wrote the sample entry's coded size, so an odd-sized sequence padded to the codec's block alignment announced the padded picture as its track size. The `tkhd` now takes the entry's `clap` clean aperture (16.16 fractions kept) with a non-square `pasp` scaling the horizontal dimension (§12.1.4.1); the sample entry keeps the coded size. `writer::presentation_size`; `tests/tkhd.rs` (HEVC / AV1 odd sizes, raw 16.16 fields, aligned sizes unchanged) and a unit test over integer / fractional apertures, `pasp` and degenerate boxes.

### Added

- **`Frame::StreamParams` — producer** (round 482; oxideav-core 0.1.43): the `"heif"` decoder implements `output_params` / `output_params_generation` (`output_pixel_format` forwarded) — the width / height, the pixel format the frame's planes are labelled with (planar RGB for identity-matrix items, `YuvJ*` for 8-bit full range) and the colour signal of the picture most recently returned (before the first: of the first decoded one) — and returns one `Frame::StreamParams` event immediately before the first picture whose shape differs from the previous one's: files / items of different geometry, layout or colour description decoded in turn, output layers of different sizes. No event for the first shape or a constant stream; when the event is returned `output_params()` already answers its payload and the generation has moved (one bump per event); pictures queued before a change come out first (draining after `flush()` included); `reset()` drops the queue and keeps the last returned shape as the reference; a refused packet queues nothing. The VVC item decoder (`vvcdec`) does the same for its conformance-cropped size / layout and gains a real `reset()` (the trait default left it at end of stream). One FIFO (`stream_params::ShapedQueue`, crate-private) implements the rule for both.
- **`Frame::StreamParams` — consumer** (round 482): every loop over a codec's `receive_frame` — coded items and grid / `tili` tiles (`CodedJob`, serial and parallel workers), multi-layer items, sequence tracks (`drain_track`), the VVC wrapper's callers — skips the event as a picture and folds it into the shape the following pictures are read at (announced width / height and layout replace the item's `ispe` / layout or the sample entry's, a sequence's new size checked against the decode limits), so a later output layer of a different size decodes at its own size instead of failing the `ispe` check. The `"heif"` encoder refuses the event with `Error::Unsupported` (it codes one shape per stream). `tests/stream_params.rs`: a stub decoder registered under `h265` / `av1` / `h264` in a caller-supplied `CodecRegistry` surrounds its picture with events and tags it with a layer identity + colour signal; HEVC / AV1 / AVC items, a 2×2 grid (serial and 4 workers) and a grey item decode to exactly its picture with only the image planes; the producer rules above through `HeifCodec` and the registry decoder (`OutputParamsWatch::observe` and `observe_frame` agree, `receive_arena_frame` skips the event); `tests/layered.rs::output_layer_of_another_shape_follows_the_codec_event` (base at 960×960, event, layer 1 at 480×480); a `drain_track` unit test. The test loops that pulled frames with `while let Ok(Frame::Video(..))` (which would stop at an event) skip events (`tests/common/stub.rs::next_video`).
- **Layer-identity side channel audited** (round 482): every read of a codec's decoded planes goes through the image planes (`frame_from_planes` counts `image_plane_count()` and takes the leading planes, `HeifFrame::from_core*` / `HeifImage::from_video_frame*` / the `"heif"` encoder read `image_planes()`; `rgb.rs` and the still encoder work on this crate's `HeifFrame`, which has no side channels) — frames tagged with a `LayerIdentity` (and a colour signal) convert and encode as the picture they carry, pinned at 8 and 10 bits. `oxideav-core` floor raised to 0.1.43.
- **`sato` pixel transform derived items** (round 481; ISO/IEC 23008-12 4th edition working draft §6.6.2.6): `pixel_transform::PixelTransform` / `PixelToken` parse, validate (§6.6.2.6.4: at least one token, input operands within `reference_count`, no stack underflow, exactly one result; unrecognised version / reserved tokens are `Unsupported` — readers ignore the item) and serialise the body, `evaluate` runs it at the 8 / 16 / 32 / 64-bit intermediate precision with every result saturating (§6.6.2.6.3); `pixel_transform::compose` builds the reconstructed image (same-size / same-chroma inputs, clamped to the item's `pixi` depth and the `nclx` nominal range when `full_range_flag = 0`). `ImageKind::PixelTransform`, `ITEM_TYPE_SATO`, decode / layout prediction / `import_item`, `HeifWriter::add_pixel_transform` (idat body + `dimg`), `"HEIF-WD 6.6.2.6.1"` checks in `miaf::check`, the `heif_records` fuzz target round-trips and evaluates arbitrary bodies. `encode::encode_pixel_transform_still` codes a 9..=16-bit picture losslessly as two hidden 8-bit HEVC / AV1 items and the J.9.1 `sato` primary. `tests/pixel_transform.rs`: the Annex J.9.1 / J.9.2 bit-depth extensions over lossless HEVC items decode to their 16-bit sources sample for sample; range clamps; refusals.
- **External `tili` tiles** (round 481; ISO/IEC 23008-12:2025/Amd 2:2026 §6.11.2 / §6.11.5): a tiled image item whose tiles are separate HEIF files addressed by the `deti` URL template now composes through a caller-supplied `tiled::ExternalTileResolver` (`ItemDecoder::with_tile_resolver`, `DecodeOptions::with_tile_resolver` / `tile_resolver` holding a `SharedTileResolver`; any `Fn(&ExternalTileRequest) -> Result<Vec<u8>> + Sync` closure is one). Requests carry the item, row-major index, column / row, tile ID (`tileIDstart + index`), directory ID (NumTiles / (end − start) tiles per directory, the remainder in the last) and the URL built per Table 12 (`ExternalTiles::url` / `directory_id` / `tile_id` / `request`). Without a resolver the decode is the typed `HeifError::external_tile_unresolved` naming the first missing URL (`external_tile_info()`); the crate never opens a file or a socket. Tile files are checked against §6.11.2 (`tiled::check_external_tile`: HEIF-family, the tile the only image item, no entity groups, same `handler_type`), one coding format and the `tilC` tile size across tiles; plain tiles decode as parallel jobs (byte-identical to serial), a tile with its own transformative properties through its file's graph. `HeifWriter::add_external_tiled_item` writes the item (`deti` with `external_tiles_urls = 1`, `tilC` without `tile_item_type` / `tipa`, an `iloc` entry naming the `deti` with one zero-length extent); `import_item` re-muxes it verbatim. A `deti` whose storage form disagrees with the `tilC` tail now decodes by the `deti` (an in-file `deti` with no `tile_item_type` is `InvalidData`). `miaf::check` gains the HEIF-A2 §6.11.2 / §6.11.3.2 / §6.11.3.3 / §6.11.5.3 checks on every `tili` item. `tests/external_tiles.rs` (round trip serial / parallel / options, directories, per-tile properties, refusals, re-mux, MIAF).
- **The `ipco` is a pool** (round 479; ISO/IEC 14496-12 §8.11.14): `HeifWriter` interns every distinct property box once, by its serialized bytes — decoder configurations (`hvcC` / `av1C` / `avcC` / `vvcC` / `lhvC`) included, which were written once per coded item until 0.0.13 — and every item carrying it indexes the same entry. A 12 MP 256-px grid goes from 196 `ipco` entries to 5 (192 tiles share one record), a 1 024-tile grid from 1 028 to 5; the two black-box readers that refused the 192-tile file at their 100-child-box `ipco` limit open it, the 1 024-tile file stays behind their 1 000-item `iloc` / stream limits (measured table in the README). A master and its alpha keep their own `hvcC` (different parameter-set ids), a grey AV1 master and its alpha share one `av1C` — every shape still opens in `sips` / `heif-info` / `heif-convert` / `magick` / `ffmpeg`. `tests/writer.rs::ipco_is_a_pool_shared_by_every_item` (1 024 tiles, re-mux keeps the pool), the `heif_writer` fuzz target asserts a duplicate-free pool on every written file; the conformance matrix gains the 285-tile HEVC / AV1 rows (exact in every renderer that refused them before) and the 1 024-tile row (a measurement: the readers' 1 000-item limit).
- **`ipma` index width on both sides of 127** (round 479): the writer emits the 15-bit index form (`flags & 1`) exactly when the pool exceeds 127 entries and the 7-bit form otherwise, the reader resolves both; `tests/writer.rs::ipma_switches_to_15_bit_indices_past_127_properties` builds files with 127 and 128 distinct properties and checks the form, the count and every association. An item with more than 255 associations (`association_count` is one byte) is a typed refusal instead of a silent wrap.
- **No degenerate items** (round 479; ISO/IEC 14496-12 §8.11.3.2.4.3): an `iloc` extent of length 0 means "to the end of the source", so `HeifWriter` refuses at write time, by body kind, an empty coded body, an empty constrained-extent tile, an empty metadata body (empty XMP / raw Exif), an empty `tmap` body and an empty `tili` payload (`tests/writer.rs::empty_item_bodies_are_refused_at_write_time`); the sequence writer's cover-alias placeholder (whose extent is re-targeted at the sample) is the one carve-out. `add_grid` refuses a layout outside 1..=256 rows / columns (a zero-row grid underflowed the `rows_minus_one` byte, a 257-row one wrapped it) and a zero canvas, `add_overlay` an overlay with no input image or a zero canvas (HEIF §6.6.2.2 / §6.6.2.3: one or more inputs; `tests/writer.rs::degenerate_derived_items_are_refused`). The `heif_writer` fuzz target asserts no written file carries a zero-length extent.
- **Fuzz record** (round 479): `heif_writer` (now asserting the property-pool / index-form / extent invariants on every written file) ran 9 minutes on its corpus after the pool change (58 364 executions, the encoder-driven inputs at ~107 exec/s, peak RSS 909 MiB), `heif_sequence` 5 minutes after the `udta` / `cprt` parser joined the movie walk (1.22 M executions, peak RSS 459 MiB); no finding.
- **core 0.1.41** (round 479): the `"heif"` encoder declares `CodecCapabilities::embedded_blob_kinds = [ICC, EXIF, XMP]`; the sequence muxer implements `Muxer::set_metadata` / `carries_metadata_key` for the copyright declaration (`cprt` in the movie-level `udta`, ISO/IEC 14496-12 §8.10.2): keys `copyright` (language `und`) and `copyright:<lang>` are written, one `cprt` per language, every other key dropped (no ISOBMFF / HEIF carriage for free-form string pairs exists; none is invented), `Unsupported` on a written-through still stream, `InvalidData` after `write_header`. The demuxer reports the same keys from the new `Movie::copyrights`, and a track's own `trak/udta/cprt` (`Track::copyrights`, `SequenceWriter::track_copyrights` / `with_track_copyright`) as `track:<id>:copyright[:<lang>]` (`sequence::Copyright`, `parse_cprt`, `parse_udta_copyrights`; `SequenceWriter::copyrights` / `with_copyright`, a language that is not a three-letter lower-case code is a write-time refusal). `tests/sequence.rs::copyright_declarations_round_trip_through_cprt`, `tests/mux.rs::muxer_set_metadata_carries_copyright_as_cprt` / `encoder_declares_its_embedded_blob_kinds`.

## [0.0.13](https://github.com/OxideAV/oxideav-heif/compare/v0.0.12...v0.0.13) - 2026-10-21

### Other

- an item's iloc extents are bounded by its source — overlapping extents (a 3 KB fuzz input naming one span hundreds of times) concatenated to hundreds of MB; item_data / item_extents / the cm=2 chain refuse an item covering more bytes than the file / idat / source item holds (heif_parse finding once it re-muxed every item through import_item; seeded); fuzz record for heif_parse / heif_sequence / heif_writer
- production soak measured — examples/heifsoak decodes and re-encodes every file under an ExecutionContext budget (wall + peak RSS per file, slowest-ten tables, totals), the README carries the 81-file device-sample / conformance run at 1 / 4 / 8 workers with before / after numbers and the codec-internal attribution; C034 decodes with its Exif (an Exif item body that starts at its TIFF header is taken as the payload — the magic read as an offset refused the whole decode)
- sequence-level metadata both ways — SequenceWriter writes the whole-sequence Exif / XMP as items of the master track's MetaBox (HEIF §8.3, Annex A.2.2 / A.3) and per-picture metadata as timed Exif / metx tracks with cdsc; the reader parses the track meta (HeifFile::item_data_in) and metadata sample entries (a non-visual stsd entry no longer fails the moov), the contract decode_all and the demuxer surface it on every frame / the stream blobs / exif + xmp Data streams / the composed stream's per-frame blobs, the muxer takes it back; sequence files laid out ftyp meta moov mdat (ffmpeg refused cdsc items + a track meta the other way round); muxer ICC pairs with an unspecified nclx (HEIF 6.5.5); Exif chunk accounting
- tili from the high-level encoder — EncodeOptions::tile_carrier (TileCarrier::Tiled) and the framework tiling=tili option pack the grid rule's tiles behind a deti offset table as one tili item with the shared tilC (decoder configuration + tile ispe through tipa), alpha / depth / thumbnail / metadata / gain map attached as to a grid; import_item carries tili items byte for byte; conformance-matrix writer rows for hevc / av1 tili (no black-box reader implements the 2026 item type)
- tests/remux.rs gates on no refusals and a floor the vendored set clears (CI has the 57 vendored fixtures, not the docs corpus)
- heif_writer fuzz target (corpus re-mux oracle + encoder-driven writer → reader round trip) and its two findings fixed — the padding and caller crops fold into one exact clap with companions carrying the orientation only (chain order enforced), and overlays compose layers of mixed chroma after their transforms instead of refusing a conformant file
- writer interop measured — the conformance matrix builds every shape from its bytes and adds iden / depth / overlay / manual grid / collection rows; Apple ImageIO probed through ImageIO itself (irot / imir map onto the reported Exif orientation, applied before comparing); overlay fill-domain and ffmpeg iden / iovl findings in the README; writer_interop gains the iden, depth, overlay and collection gates
- derived items from the high-level encoder — encode_overlay writes an iovl from an OverlaySpec (hidden layers with alpha, canvas fill, thumbnail of the composite), TransformCarrier::Identity puts the chain on an iden over the un-rotated picture, EncodeOptions::depth_map writes a depth auxiliary, the gain map follows the primary's irot / imir; framework irot / imir options
- re-muxing without re-encoding — HeifWriter::import_item carries an item with its derivations, auxiliaries, thumbnails, metadata items and altr alternates byte for byte, from_files collects several files' primaries; the heif muxer merges a still stream's packets (a burst) into one collection with per-item Exif / XMP / ICC; tests/remux.rs re-muxes every corpus file to the same image
- brands are claimed truthfully — the writer decides miaf and the Annex A profile brands (MiHB / MiHA / MiHE, MA1B / MA1A, MiAB with a new A.6.2 check) on a header-only probe of its own layout, the sequence writer on its assembled file; a file the checker rejects keeps the structural brands only

### Added

- **Sequence-level metadata both ways** (round 478; HEIF §8.3, Annex A.2.2 / A.3, ISO/IEC 14496-12 §8.11.1 / §12.3): `SequenceWriter::exif` / `xmp` (`with_exif` / `with_xmp`) write the metadata true for the whole sequence as an `Exif` item (`ExifDataBlock`) and / or an XMP `mime` item of the master track's `MetaBox` (`pict` handler, bodies in that box's `idat`, item ids past the cover still's), and `SequenceWriter::metadata_tracks` (`SequenceMetadataTrack`, `push_metadata_track`) write timed metadata tracks — `meta` handler, `nmhd`, a `cdsc` track reference to the picture track, sample entry `Exif` with every sample an `ExifDataBlock` or `metx` naming `writer::XMP_NAMESPACE` with every sample an XMP packet, their own timescale and durations (a sample describes the pictures its interval covers). The reader parses the track-level `meta` (`Track::meta`, `Track::sequence_metadata`; `HeifFile::item_data_in` resolves any `Meta`'s items against the file / its `idat`) and the metadata tracks (`Track::is_metadata` / `metadata_kind`, `sequence::MetadataTrackKind`, `Movie::metadata_tracks_of`; `metx` / `mett` entry fields `content_encoding` / `namespace` / `schema_location` / `mime_format` on `SampleEntry` — before, a non-visual sample entry failed the `stsd` parse and with it the whole `moov`). The contract `decode_all` puts the sequence metadata on every track frame, refined by the time-parallel samples; the `"heif"` demuxer puts it on the track's stream `blobs`, surfaces each metadata track as a `MediaType::Data` stream (`demux::CODEC_ID_EXIF` = `exif`, packets the Exif payload from the TIFF header; `CODEC_ID_XMP` = `xmp`) and composes a `"heif"` stream for a master with metadata tracks (as for an alpha track) whose frames carry the samples as per-frame blobs; the `"heif"` muxer writes the stream's Exif / XMP blobs to the track `meta` and `exif` / `xmp` `Data` streams to metadata tracks. `meta::exif_payload` / `exif_data_block` / `metadata_items` are standalone (the `decode` re-export stays). `tests/sequence.rs::sequence_level_metadata_round_trips`, `tests/mux.rs::metadata_tracks_round_trip_through_the_muxer`; the `heif_sequence` fuzz target walks the track `meta` and the metadata tracks, `heif_parse` re-muxes every item through `import_item`.
- **Production soak, measured** (round 478): `examples/heifsoak` decodes and re-encodes every file it is given (HEVC defaults, AV1 q60, AVC where the source is `avc1`) under an `ExecutionContext` budget, recording wall time and peak RSS per file and budget, with the slowest-ten tables and totals; the README carries the run over the 81 device-sample / conformance files at 1 / 4 / 8 workers (decode 10.2 / 9.8 / 9.8 s for the set, the 12 MP grid 0.44 → 0.15 → 0.097 s, HEVC re-encode 38 → 21 → 18 s, AV1 39 → 13 → 9 s, peak RSS 59 → 76 MiB, 20 documented refusals), and which rows are codec-internal.
- **Fuzz record** (round 478): `heif_parse` (now re-muxes every item through `import_item`, `tili` ones with their offset table and `tipa` properties), `heif_sequence` (the track-level `MetaBox` — item resolution against the file / its `idat`, the Exif / XMP items — and the metadata tracks) and `heif_writer` ran 10.5 minutes each on their corpora after the overlapping-extents fix (1.58 M + 1.43 M, 1.81 M + 1.42 M and 41.7 k + 38.8 k executions; peak RSS 754 / 545 / 919 MiB), no finding.
- **`tili` from the high-level encoder** (round 478): `EncodeOptions::tile_carrier` (`TileCarrier::Grid`, the default and the previous behaviour; `TileCarrier::Tiled`) and the framework option `tiling` (`grid` / `tili`) choose how a tiled picture's tiles are declared whenever `grid_tile`'s rule tiles it (an explicit tile, or the automatic 512-px grid above 4 MP): under `Tiled` the tiles — the same coded pictures as the grid's, coded in parallel under the thread budget with bytes identical to the serial order — are packed back to back behind a `deti` offset table (HEIF Amd 2:2026 §6.11.5, 32-bit offsets and sizes, sequential) as one `tili` item whose `tilC` names the tile item type and associates the shared decoder configuration and the tile `ispe` through `tipa`; the item carries `ispe` (the true size — the padding of the last row / column is the §6.11.2 crop), `pixi`, `colr` / ICC and the transforms, and alpha / depth auxiliaries, the thumbnail, Exif / XMP and the gain map attach to it as to a grid. Every tile is coded at the full tile size, so one configuration describes them all (a codec that varied it would be a typed refusal). `HeifWriter::import_item` now carries a `tili` item byte for byte (tile data + offset table, the `deti` entry, the tile properties re-interned) instead of refusing it, so `from_files` and the muxer's bursts take tiled pictures. `tests/tiled.rs`: HEVC (with alpha, thumbnail, Exif / XMP / ICC) and AV1 tiled items decode exactly under 1 / 3 / 4 workers, equal the grid carrier's output, pass the MIAF checks, re-mux through `import_item`, and the framework `tiling` option selects the carrier (an unknown value is a typed refusal). Interop (the conformance matrix's writer direction): no black-box reader on this machine implements the 2026 item type — `sips` / `heif-convert` / `magick` / `heif-info` refuse the primary (the unknown item type is dropped from their item list and the `pitm` dangles), ffmpeg renders the thumbnail — so the carrier is opt-in; `heifenc --tiling tili`.

### Fixed

- **An item's extents are bounded by its source** (round 478, found by the `heif_parse` fuzz target once it re-muxed every item through `import_item`): an `iloc` whose extents overlap (a 3 KB input naming one span hundreds of times) concatenated to hundreds of MB inside the 1 GiB item cap — `item_data` / `item_extents` / the construction-method-2 chain now refuse an item whose extents cover more bytes than its source holds (the file, the `idat`, the source item), a typed `LimitExceeded`; no real file names a byte twice. Seed `fuzz/corpus/heif_parse/minimal.overlapping-extents-oom`.
- **C034 decodes with its Exif** (round 478): the conformance file's `Exif` item has no `exif_tiff_header_offset` word — the item is exactly the staged Exif blob, TIFF header first — and the magic read as an offset refused the whole decode; `meta::exif_payload` takes a body that starts at its TIFF header (`MM\0*` / `II*\0`) as the payload, since that bit pattern could only point past any item. `tests/decode.rs` pins the decoded blob against `C034.exf`.
- **Sequence files are laid out `ftyp`, `meta`, `moov`, `mdat`** (round 478): with the file-level `meta` after the `moov`, ffmpeg refuses a file whose cover still carries `cdsc` metadata items and whose track carries a `MetaBox` ("missing stream referenced by thmb item"); read first, the file-level `meta` is complete before the track's is seen, and every reader on this machine opens both orders (README table).
- **The sequence muxer's ICC pairing** (round 478): an ICC profile from the stream's blobs was written next to the MIAF-default `nclx` (primaries 1 / transfer 13), a HEIF §6.5.5 violation that silently cost the file its `miaf` brand; the `nclx` now signals unspecified primaries / transfer (2 / 2) next to a profile, as the still encoder does, on the sample entry and the cover item.
- **Overlays over layers of mixed chroma sampling** (round 477, found by the `heif_writer` fuzz target): an odd-sized 4:2:0 input comes out of its own `clap` promoted to 4:4:4 (MIAF §7.3.6.7) while an even-sized one stays 4:2:0, and `compose::composite_overlay` refused the mix as "layouts differ" — a MIAF-conformant file this crate itself writes. The inputs must share a bit depth (§7.3.11.3); differing chroma samplings now compose in 4:4:4 like any other promotion trigger. Seed `fuzz/corpus/heif_writer/minimal.overlay-odd-and-even-420-layers`.
- **A `clap` in `EncodeOptions::transforms` on a picture the codec pads** (round 477, found by the new `heif_writer` fuzz target): the padding crop and the caller's crop used to land as two `clap`s on the coded item (and its alpha), failing the MIAF §7.3.6.7 order check — and the crop, given in the primary's coordinates, was copied onto the thumbnail, the depth map and the gain map, items of other geometry. Now the two crops fold into one exact rational `clap` (the clean aperture the caller's, its offsets moved by half the padding), the thumbnail is cut from the cropped picture, and thumbnails / depth maps / gain maps carry the orientation (`irot` / `imir`) only. The high-level encoder refuses a chain that is not `clap → irot → imir` with each at most once (a typed `HeifError`); the `HeifWriter` item API stays free-form. Seed `fuzz/corpus/heif_writer/minimal.clap-with-padding-and-companions`.

### Added

- **Writer interop, measured** (round 477): the conformance matrix's writer direction (`tests/conformance_matrix.rs`, the README table) now builds every shape from its written bytes and adds the derived-item files — `irot`+`imir` on the coded item and on an `iden` (HEVC, AV1), depth auxiliaries, two-layer overlays with alpha and a thumbnail, a manual 64-px grid with a thumbnail, a three-picture collection with per-item Exif. Apple ImageIO is probed through ImageIO itself (`tests/common/imageio.swift`, compiled on demand with `swiftc`): it maps `irot` / `imir` onto the Exif orientation it reports (8 / 2 / 7 for the three chains, on a coded item and on an `iden` alike) and hands out un-rotated pixels, so the `sips` cells apply the reported orientation before comparing — the old "rotation not applied" cells are within 2 codes like every other 4:2:0 `sips` cell. Findings: ffmpeg's HEIF demuxer applies no `iden` transforms and composes no `iovl`; every other reader renders the `iden`, the depth-carrying file, the grid and the overlay (the overlay within 3 codes: the sRGB canvas fill is composed in the item's YCbCr here and in RGB there), and `heif-convert` / `magick` emit a collection's pictures as separate files. `tests/writer_interop.rs` gains the `iden` and depth shapes and an overlay / collection opener gate (ImageMagick's uncompressed render as the pixel oracle).
- **Derived items from the high-level encoder** (round 477): `encode_overlay` / `encode_overlay_into` write an `iovl` (HEIF §6.6.2.2) from an `OverlaySpec` — canvas size and `canvas_fill_value`, `OverlayLayer`s (picture + offsets, bottom-most first) each coded as a hidden item in the first layer's coding layout (MIAF §7.3.11.3) with its alpha auxiliary, the overlay the primary with the `colr` / `pixi` set, the transforms, Exif / XMP and a thumbnail of the composed canvas when asked; lossless, the primary decodes to exactly what `compose::composite_overlay` produces from the sources (`tests/writer.rs`). `EncodeOptions::transform_carrier` (`TransformCarrier::CodedItem`, the default and the previous behaviour; `TransformCarrier::Identity`) places `transforms` on an `iden` derived item (§6.6.2.3) over the un-rotated coded picture / `grid` / `iovl` (hidden), the `iden` the primary with the descriptive set — auxiliaries stay on the input without transforms, thumbnails and the gain map carry them; pixels and alpha identical to the default carrier's through both decode paths. `EncodeOptions::depth_map` writes a depth auxiliary (§7.5.3: the HEVC depth URN on HEVC items, the CICP one otherwise; coded like the alpha plane with its own parameter-set ids, any size), read back as `DecodedImage::depth` exactly under `pcm`. The gain-map item now carries the primary's `irot` / `imir` (it is resampled onto the oriented base, ISO 21496-1 §6.2.2): a rotated file's tone-mapped reconstruction is the rotation of the un-rotated file's (`tests/gainmap.rs`) — before, a transform chain next to a gain map left the map un-oriented.
- **Framework `irot` / `imir` options** (round 477): the `"heif"` encoder declares `irot` (0..=3 quarter turns anti-clockwise) and `imir` (`none` / `vertical` / `horizontal`), written as the primary's essential `irot` then `imir`; the bytes equal the contract encode with the same `transforms`, out-of-range values are typed refusals.
- **Re-muxing without re-encoding** (round 477): `HeifWriter::import_item(&file, id)` carries an image item from a parsed file into the writer with everything it needs — its derivation inputs (`dimg`, recursively), auxiliaries, thumbnails, metadata items (`cdsc`), the item members of every `altr` group it is in (a `tmap` alternate with its gain map), every reference among the imported items and every entity group whose members all came along (an `altr` with at least two) — bodies byte for byte (multi-extent `cexg` items keep their extents, `grid` / `iovl` / `tmap` / `cfen` descriptors re-typed), properties through `Property::parse` (unknown boxes verbatim), hidden flags, names, `content_encoding`; ids re-assigned; protected (`ipro`) and `tili` items are typed refusals. `HeifWriter::from_files(&[bytes…])` collects the primary images of several files into one collection (the first the primary). `tests/remux.rs`: every HEIF-family file the checkout can see (all 120 that decode: vendored fixtures, staged corpus, device samples, conformance set) re-muxes to the same pixels, colour and metadata, and stays MIAF-conformant whenever the source was.
- **Bursts through the framework muxer** (round 477): the `"heif"` muxer takes any number of still packets — the `"heif"` encoder's whole-file packets for the frames of one stream, or the demuxer's per-item packets — and merges them into one image collection with `from_files` (one packet stays the file, byte-identical): each picture keeps its own derivations, auxiliaries, thumbnails and Exif / XMP / ICC items, so the encoder's per-frame blobs (refined over the stream's) land on their own items and the demuxer reads the file back as the same packets with the same per-frame blobs. `tests/mux.rs::still_burst_merges_into_one_collection_with_per_item_metadata`: three `pcm` frames with distinct Exif / XMP over a stream ICC → one `heic` with three images and six metadata items, MIAF-conformant with `MiHB`, exact pixels and metadata through `decode_all` and the demuxer + decoder.
- **Brands are claimed truthfully** (round 477, the writer to production): `HeifWriter` decides its auto-selected brands on a header-only probe of the very layout it is about to write (the MIAF clauses read the `meta` tree and `idat`, never the coded bytes) — `miaf` stays only when the file passes the §7 / §8 general requirements, and the Annex A profile brands of the primary item's coding family (MIAF §7.3.2: a listed profile needs a conforming image among the primary and its alternates) join when their codec constraints hold: `MiHB` / `MiHA` / `MiHE` (cumulative — an 8-bit 4:2:0 HEVC file claims all three, Main 10 the last two, a 4:4:4 / monochrome item `MiHE` alone), `MA1B` / `MA1A` (AV1 `seq_profile` 0 both, 1 the latter), `MiAB` (new A.6.2 check: Progressive / Constrained High at level ≤ 5.2, 4:2:0 8-bit — the registry AVC encoder's record signals neither constraint flag, so `avci` files carry `miaf` without `MiAB` today); `tmap` stays last. A file the checker rejects (a thumbnail as primary, C005's shape) carries the structural brands only; a caller's `with_brands` override is written as given. `SequenceWriter` runs the same pass on its assembled file (the cover still's profile brands, `miaf` dropped when the §7 / §8 checks fail). `tests/writer.rs::brands_are_claimed_truthfully`; every black-box reader opens the files with the profile brands (`tests/writer_interop.rs`).

## [0.0.12](https://github.com/OxideAV/oxideav-heif/compare/v0.0.11...v0.0.12) - 2026-10-17

### Other

- withdraw the "avif" codec-id registration — one codec id has one owner (oxideav-avif's still encoder); the heif muxer keeps taking its avif still stream, the quality → qp mapping and the AV1 quality rule stay
- the heif / avif codec capabilities flag lossy + lossless; a registry-only gate pins the avif id's capabilities (the heif id's 26 accepted layouts, options schema) and the quality bytes of both ids on 16x16 Yuv420P / Yuv444P / Rgb24 frames
- the AV1 rule of the gateway `quality` pinned — an explicit quality < 100 without a mode codes mode=intra at that quality, 100 the lossless coding, absent today's 60; the 16x16 RGB gate through the avif and heif (codec=av1) encoders
- the "heif" muxer accepts an "avif" still stream exactly like a "heif" one — the gateway's frame → avif encoder → heif muxer path writes the avif-branded packet through unchanged
- gateway contract — an "avif" codec id (encoder default AV1, avif / avis brands; decoder accepts every HEIF-family file) next to "heif" (HEVC, heic); the framework `quality` governs HEVC / VVC / AVC through a documented qp mapping
- CHANGELOG — the round's fuzz record (every target >= 10 min on its corpus, no finding; the daily Fuzz workflow green on the released producers)
- HEIF metadata rides the framework path as core 0.1.40 blobs — ICC / Exif / XMP on the demuxer's stream parameters and on every decoded frame, embedded back by the encoder and the sequence muxer; both matrices gain a metadata dimension
- AV1 items decode under the thread budget (av1 0.1.21's ExecutionContext) — 12 MP single-item AVIF 0.456 s → 0.102 s on 8 workers, byte-identical; the decode matrix runs its framework side under an 8-worker context
- AVC and VVC items take every layout their encoders code — AVC 4:2:0 / 4:2:2 / 4:4:4 at 8 / 10 / 12 bits + lossless over h264 0.1.9's avcc surface, VVC any size / 4:0:0–4:4:4 / 10-bit / lossless over h266 0.0.10; matrix 553 → 724 cells with a black-box AVC leg
- adopt h264 0.1.9 / h265 0.0.15 / h266 0.0.10 / av1 0.1.21 / core 0.1.40 — the idr_pic_id rotation workaround is gone (the registry encoder rotates per session), all-IDR avcs sequences proved in both decoders, the two daily-fuzz crash inputs seeded

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
