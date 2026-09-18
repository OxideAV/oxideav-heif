# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

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
