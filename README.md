# oxideav-heif

Pure-Rust HEIF / HEIC / MIAF image container (ISO/IEC 23008-12, ISO/IEC
23000-22) for the [oxideav](https://github.com/OxideAV) framework.

This crate owns the **container**: the ISOBMFF box tree, the `meta`
item model, derived images, auxiliaries, thumbnails, metadata, image
sequences, MIAF conformance and a writer. It never decodes an HEVC,
AV1, AVC or VVC bitstream itself — coded items are handed to
[`oxideav-h265`](https://github.com/OxideAV/oxideav-h265),
[`oxideav-av1`](https://github.com/OxideAV/oxideav-av1),
[`oxideav-h264`](https://github.com/OxideAV/oxideav-h264) and
[`oxideav-h266`](https://github.com/OxideAV/oxideav-h266) (the
default-on `registry` feature). With
`default-features = false` the crate is a dependency-free container
parser / composer / writer over its own planar frame type (see
*Standalone use*).

This crate follows the workspace **image-crate API contract**
(`IMAGE_CRATE_API.md` in the umbrella): the same root vocabulary every
`oxideav-<format>` crate exposes — `probe`, `info`, `decode` /
`decode_rgb8` / `decode_rgba8` / `decode_all`, `encode` / `encode_rgb8`
/ `encode_rgba8` / `encode_all`, `HeifImage` (its `new` / `from_rgb8` /
`from_rgba8` are `Result`s that refuse a zero dimension or planes that
do not fit the layout with `InvalidData`), `RgbImage` / `RgbaImage`, `PixelFormat`,
`ImageInfo`, `DecodeOptions` / `EncodeOptions`, `HeifError` (= `Error`).

## Standalone use

```toml
oxideav-heif = { version = "0.0", default-features = false }
```

**Pixels need `registry`.** HEIF is a container: its pixels come from
the HEVC / AV1 / AVC / VVC codec crates, and those are framework-only by
rule, so `decode*` / `decode_all` / `encode*` exist only with the
default-on `registry` feature (below). With `default-features = false`
the crate still gives you the header, the whole container model and
composition over planes you decoded yourself — no framework, no codec
dependency:

```rust
let bytes = std::fs::read("photo.heic")?;
if oxideav_heif::probe(&bytes) {
    // Header only: no codec runs.
    let info = oxideav_heif::info(&bytes)?;
    println!("{}x{} {:?}, {} image(s), alpha {}, icc {} exif {} xmp {}",
        info.width, info.height, info.format, info.frames,
        info.has_alpha, info.has_icc, info.has_exif, info.has_xmp);

    // The container model.
    let file = oxideav_heif::HeifFile::parse_borrowed(&bytes)?;
    let node = oxideav_heif::derived::build_primary_graph(&file)?;
    for coded in node.coded_items() {
        let au = file.item_data(coded.item.id)?;        // the HEVC access unit / AV1 temporal unit
        let cfg = coded.properties.hvcc();              // its decoder configuration
        // … hand `au` + `cfg` to any HEVC decoder, build a HeifFrame from
        // its planes, then compose:
    }
    // oxideav_heif::compose::{GridCanvas, composite_overlay, attach_alpha,
    //   apply_transforms} over HeifFrame, and HeifImage::from_frame(frame,
    //   color, metadata).to_rgba8() for the bytes.
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

With the default features the one-screen path is the contract's:

```rust
// Cargo.toml: oxideav-heif = "0.0"            (registry on by default)
let bytes = std::fs::read("photo.heic")?;
if oxideav_heif::probe(&bytes) {
    let info = oxideav_heif::info(&bytes)?;             // header only
    let img  = oxideav_heif::decode(&bytes)?;           // HeifImage, native layout (Yuv420P, Yuv420P10Le, Gbrp8, …)
    let rgba: Vec<u8> = img.to_rgba8();                 // tightly packed RGBA, 4 * width bytes per row
    let (w, h) = (img.width(), img.height());
    // or in one call: oxideav_heif::decode_rgba8(&bytes)?.data

    let opts = oxideav_heif::EncodeOptions::default().with_qp(20);
    let out: Vec<u8> = oxideav_heif::encode_rgba8(w, h, &rgba, &opts)?;   // HEVC 4:2:0 + alpha item
    std::fs::write("out.heic", out)?;
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`decode_all` yields every image of a burst (the primary first, then by
item id; `altr` groups collapse to their first decodable member) and
every sample of the image-sequence tracks, each `Frame` with its
`delay` from the track timing. `decode_with(&DecodeOptions)` adds
limits, `strict`, `item_id`, `tone_mapped`, `threads`; `decode_from` /
`encode_to` take a `Read` / `Write`. `encode_all(&[Frame],
&EncodeOptions)` is the mirror of `decode_all`: frames without a
`delay` become image items (the first is the primary, the rest a
burst), frames with a `delay` the samples of an image-sequence track
(timescale 1000, an alpha track when the pictures carry alpha, a MIAF
cover item aliasing sample 0 when there are no item frames); both may
be mixed as `decode_all` returns them, all sharing frame 0's geometry
and codec. With the lossless HEVC mode (`with_hevc_mode("pcm")`) planar
`Yuv420P` frames round-trip exactly. The deeper HEIF surface stays
under its own names: `HeifFile`, `decode_primary` / `decode_item` →
`DecodedImage` (depth map, thumbnails, gain map, layers, typed
properties), `HeifWriter` / `SequenceWriter`, `encode_still*`.

## Framework use

`oxideav_heif::register(&mut ctx)` installs the `"heif"` demuxer
(probe on the HEIF-family `ftyp` brands, `.heic` / `.heif` / `.heics`
/ `.heifs` / `.hif` / `.hmg` / `.avif` / `.avifs` hints), the `"heif"`
sequence muxer and the `"heif"` codec (decoder + encoder; direct
factories `make_decoder` / `make_encoder`). A `.heic` opens as stream 0
= the still images (one `"heif"` packet per displayable image item →
one composed `VideoFrame` each: the primary first, then the rest of a
burst in `decode_all`'s order; item packets are untimed — `pts` = item
index, duration 1, time base 1/1 — and `metadata` lists them under
`stream:<n>:item_ids`; items whose predicted output — layout, geometry,
colour description — differs from the primary's form further `"heif"`
still streams, one per distinct output; alpha / depth / gain-map
auxiliaries and thumbnails are never frames of their own) plus one
stream per image-sequence track (`"h265"` / `"av1"` / `"h264"` /
`"h266"` packets with `hvcC` / `av1C` / `avcC` / `vvcC` extradata, one
per sample, `pts` / `dts` / `duration` from the sample table in the
track's timescale, `stream:<n>:track_id`; a track with an alpha
auxiliary track is also offered composed, as `"heif"` packets,
`stream:<n>:composed_from_track`). The still streams come first
whatever the major brand — the composed primary still is video stream
0, what the gateway's `primary()` / `max_frames(1)` rely on — and the
track streams follow them. The `"heif"` decoder takes
`HeifDecoderOptions` (see *Limits*). Every still-stream frame and every sequence frame is
byte-identical to the corresponding `decode_all` frame (pinned over
the corpus and the real-world interop set). The framework decoder and encoder are thin adapters over the
contract functions — one implementation — and `HeifImage` converts
both ways: `into_video_frame()` / `From<HeifImage> for VideoFrame`,
`HeifImage::from_video_frame(&frame, &codec_parameters)` (also
`TryFrom<(&VideoFrame, &CodecParameters)>`; `from_video_frame_parts`
takes the geometry and label directly); the
`PixelFormat` enums map one to one by name (`From` / `TryFrom`), the
8-bit YCbCr layouts carrying the framework's full-range `YuvJ*` label
when `color.range` is full.

## Framework surface (r475)

The path `oxideav-image` / `oxideav-io` / `oxideav-cli-convert` take —
the registry `"heif"` encoder, the `"heif"` demuxer and decoder, the
`"heif"` sequence muxer — is measured cell by cell against the
contract functions; both tests print their tables and write them to
the scratch directory (`framework_matrix.txt`, `decode_matrix.txt`).

**Encoder matrix** (`tests/framework_matrix.rs`): every declared option
(`codec` hevc / av1 / avc / vvc × `mode` × `qp` / `quality` × `grid` ×
`thumbnail` × `range` × `rd` / `tiles` / `ctb` × `threads` × `depth` ×
`filters` × `chroma`) × every input `VideoFrame` layout the gateway can
hand it (47 layouts: planar 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 at 8 / 10 /
12 / 16 bits with and without alpha, `Gbrp*` / `Gbrap*` 8–16 bits,
packed RGB / RGBA / BGR / BGRA, `Rgb48Le` / `Rgba64Le`, `Ya8` /
`Ya16Le`) × the sizes that matter (1×1, 7×5, 65×33, 48×32, 160×96
explicit grids, 2056×2048 automatic grid): **724 cells, 723 coded, 1
typed refusal** (`depth=12` on VVC), 0 failures (r476: 150 AVC cells
joined — every High-family layout, lossless, `cabac`, the depth /
chroma / range knobs, `pcm` grids — and 21 VVC cells: `pcm` on every
layout family and size, `depth` / `chroma` / `range`). Per cell:
the file opens in the framework demuxer with the documented label,
geometry, `1/1` untimed stream and `stream:<n>:item_ids`; the `"heif"`
decoder and the contract `decode` return byte-identical planes and the
same colour signal; the bytes equal the contract-path encode of the
same picture under the same options (every cell whose layout the
contract has: 455 of 551; the other 96 — BGR(A) swizzles, 16-bit
packed, grey + alpha — equal the contract encode of the swizzled /
planar equivalent where one exists); lossless cells are exact (per
sample, through the 4:4:4 promotion of odd sizes); lossy cells land
≥ 30 dB RGBA8 (≥ 20 dB at `qp 40` / `quality 0`, ≥ 24 dB on VVC);
`heif-info` opens all 723 files with their geometry and Apple ImageIO
(`sips`) all 692 HEVC / AV1 / AVC files (it has no VVC decoder) —
the 10 / 12-bit 4:2:2 / 4:4:4 and lossless AVC items included; every
`avc1` item of the 230 AVC files decodes byte-identically in this
crate's AVC decoder and in the black-box reference decoder;
the thread budget (`threads`, `set_execution_context`) and the option
aliases (`pcm` = `quality=100` on AV1, `depth=8` on an 8-bit source,
`rd 0` on the quadtree layouts) never change a byte; 19 invalid
parameters (unknown codec / mode / option, `depth=9`, `ctb=48`,
`grid=abc`, `Nv12` / `Pal8` / `Yuyv422` / `Cmyk` / `MonoBlack` /
`Argb` / `Yuv411P` frames, missing geometry, short planes, audio) are
typed refusals.

**Decode matrix** (`tests/framework_decode_matrix.rs`): the demuxer +
registry decoders against `decode_all` / `info` over every HEIF-family
file the checkout sees — 57 vendored fixtures, the 14 staged corpus
bundles, the 2 device samples, the 66 MPEG / Nokia conformance files
and the 13 encrypted ones: **152 files, 128 decode on both paths, 24
refused on both** (the `cenc` items, the predictive `pred` items C043 /
C044 and the L-HEVC enhancement layers this crate does not decode),
0 disagreements — every contract image item is a packet of a still
stream with byte-identical pixels, label, colour signal and metadata
(r476: the frame's ICC / Exif / XMP blobs refined over the stream's
equal the contract image's `metadata`; a still stream's own blobs are
its first item's, a track stream's the sample entry's ICC profile),
`pts` = index / duration 1 / `1/1`; every track frame comes from its
composed or raw track stream with the same pixels, metadata and
`duration × time_base` = the contract delay; `info()` describes the first still stream; the
framework decoders run under an 8-worker execution context against
the serial contract path (r476: the AV1 items decode tile- /
band-parallel under it, byte-identical). A
debug build skips the files whose header announces more than 4 M
samples of decode work (the 12 MP device samples, the 720p
conformance sequences) unless `OXIDEAV_HEIF_MATRIX_FULL=1`; a release
build takes everything (68 s).

What the two matrices changed (r475): planar sources keep their chroma
sampling on HEVC (a 4:0:0 / 4:2:2 / 4:4:4 source used to be coded
4:2:0 — the lossless mode was lossy on those layouts and `chroma=444`
was ignored); `depth` applies to AV1 items of ≤ 12-bit sources; `pcm`
on AVC is refused; the encoder's packets carry `dts` / duration 1;
displayable items whose output differs from the primary's form further
still streams instead of being dropped; the still streams follow
`decode_all`'s `altr` selection (a `tmap` before its base, the other
view of a `ster` pair on the pair's packet, a `pitm` thumbnail first);
a primary that cannot be decoded falls back to its `altr` alternatives
on both paths; the decoder declares `HeifDecoderOptions`.

### Still-image decode budgets (12 MP, this crate, r476)

`examples/heifbench` (release, `--no-encode --runs 5 --threads N`,
Apple M4 Max) over the 12 MP iPhone device sample re-encoded by this
crate (`--write`): a single-item AVIF (one `av01` item, the AV1
encoder's size-derived 4×2 tiles) and the 48-tile grid AVIF / HEIC.
oxideav-av1 0.1.21 decodes under an `ExecutionContext` — tile-,
band-, wave- and stripe-parallel, byte-identical at any budget — and
this crate hands it the item decoder's thread budget (`DecodeOptions::
threads`, `ItemDecoder::with_threads`, the `"heif"` decoder's
`set_execution_context`, the `threads` option); the output digest is
the same at every worker count.

| file | 1 worker | 2 | 4 | 8 |
|---|---|---|---|---|
| 12 MP AVIF, one `av01` item (720 KiB) | 0.456 s (= before r476 at any budget) | 0.246 s | 0.147 s | 0.102 s |
| 12 MP AVIF, 48-tile grid (740 KiB) | 0.446 s | 0.234 s | 0.135 s | 0.075 s (tiles in parallel, as before) |
| 12 MP HEIC, one `hvc1` item (2180 KiB) | 0.438 s | — | — | 0.415 s with the budget (measured, not granted: ≈5 % for eight threads) |
| 12 MP HEIC, 48-tile grid (the device file) | 0.445 s | — | — | 0.092 s (tiles in parallel, as before) |

The HEVC decoder keeps its budget for independent items (grid tiles):
a single-item still gains ≈5 % under it (r463 measured a loss), so it
is not granted. The AV1 grid file is faster than the single item at
8 workers because its tiles are independent decodes; the single item
is now within 35 % of it.

### CLI budgets (12 MP, `oxideav convert`, r475)

The umbrella CLI as a black box (`cargo build -p oxideav-cli --release`,
this machine: Apple M4 Max, macOS 26.6), `/usr/bin/time -l` wall and
peak RSS of the whole process, the 4032×3024 iPhone 13 Pro Max device
sample decoded to PNG / JPEG by Apple ImageIO as the source; no
options (the CLI's defaults: thread budget auto, the `.heic` / `.avif`
extension picking the codec). `--no-hwaccel` gives the same bytes and
the same budgets on every HEIF direction (this crate and the pure-Rust
codecs are the path either way); `heic→jpg` under `--no-hwaccel` is
refused by the pure-Rust MJPEG encoder (`expected 3 planes`: an
`oxideav-mjpeg` / gateway finding, not this crate's).

| direction | wall | peak RSS | output |
|---|---|---|---|
| PNG → HEIC (`png→heic`, 48-tile automatic grid) | 0.77 s | 253 MiB | 2 238 585 B |
| HEIC → PNG | 0.17 s | 192 MiB | 14 994 518 B |
| HEIC → AVIF | 0.87 s | 214 MiB | 730 000 B |
| AVIF → HEIC | 0.46 s (r475; the AV1 decode under the r476 budget is 0.35 s faster when the CLI grants it) | 199 MiB | 1 466 171 B |
| JPEG (4:2:0) → HEIC | 0.50 s | 175 MiB | 2 234 977 B |
| HEIC → JPEG | 0.19 s | 151 MiB | 2 708 883 B |
| device HEIC (Apple 48-tile grid, `irot`) → PNG | 0.19 s | 191 MiB | 14 618 082 B |

Small files (96×72 / 128×128 / a 96×80 `.heics`): `png→heic`,
`heic→png`, `heic→avif`, `avif→heic`, `jpg→heic` (4:2:0 JPEG),
`heic→jpg`, alpha HEIC → PNG / AVIF and `.heics` → GIF all open in
`heif-info` / ImageMagick / ffprobe with the right geometry; a
truncated HEIC or PNG input fails with a typed error and **no output
file is created** (checked both directions). Two directions fail
outside this crate (reported, not worked around): a 4:4:4 JPEG
through the hardware JPEG decoder arrives labelled 4:4:4 with 4:2:0
planes (`--no-hwaccel` converts it; a `videotoolbox` / cli finding),
and `.heics → .mp4` is refused by the MP4 muxer (`no sample entry for
codec heif`: the CLI hands it stream 0, the cover still, rather than
the first timed stream — a cli-convert fix in progress; the still
stays stream 0 here by the workspace convention).

### Limits (what each codec refuses or converts)

| Codec | Coded layouts | Converted silently | Refused (typed `Unsupported`) |
|---|---|---|---|
| HEVC (`hevc`, default) | the source's sampling — 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 — at 8 / 10 / 12 bits; packed RGB(A) / BGR(A) at `chroma` (4:2:0 default, 4:4:4) ; packed grey + alpha as 4:0:0; lossless RGB (`pcm`, full range, ≤ 12 bits) as identity-matrix 4:4:4 (`Gbrp*`) | 16-bit sources to 10 bits (`depth=12` for 12); 14-bit planar RGB to 10; planar RGB under a lossy mode through the matrix into `chroma`'s sampling; `pcm` at limited range converts RGB to YCbCr | `depth` other than 8 / 10 / 12; `ctb` other than 16 / 32 / 64 |
| AV1 (`av1`) | the source's sampling at 8 / 10 / 12 bits; packed RGB(A) at `chroma`; packed grey + alpha as 4:0:0; planar RGB ≤ 12 bits as identity 4:4:4 (lossy too); lossless RGB (`pcm` / `quality=100`) exact | 16-bit sources to 10 (`depth=12` for 12); 14 / 16-bit planar RGB to 4:4:4 YCbCr at the coded depth | as HEVC |
| AVC (`avc`) | as HEVC: the source's sampling at 8 / 10 / 12 bits (Baseline / Main for 8-bit 4:2:0, the §A.2 minimum High-family profile otherwise), packed RGB(A) at `chroma`, lossless RGB (`pcm`) as identity 4:4:4; `cabac` | 16-bit sources to 10; 4:0:0 sources (planar grey, packed grey + alpha) to 4:2:0 with neutral chroma — the registry encoder's grey line, so a grey source comes back as a 4:2:0 item (its luma exact under `pcm`) | `depth` other than 8 / 10 / 12 |
| VVC (`vvc`) | as HEVC at 8 / 10 bits: the source's sampling (4:0:0 / 4:2:0 / 4:2:2 / 4:4:4), packed RGB(A) at `chroma`, packed grey + alpha as 4:0:0, lossless RGB (`pcm`) as identity 4:4:4; any picture size (the encoder pads to 8 and writes the SPS conformance window; an odd edge goes through `clap`) | 12 / 16-bit sources to 10 | `depth=12` (the `oxideav-h266` encoder codes 8 – 10 bits) |

Every codec: alpha rides a separate auxiliary item at the coded depth
(HEVC: the 4:2:0 shape every producer writes; AV1: 4:0:0); odd sizes
compose as 4:4:4 (the `clap` crop of a subsampled item); 4:2:0 12-bit
+ alpha and grey + alpha come back promoted to the 4:4:4 alpha layout
(no framework label for them); `grid` tiles are ≥ 64 px and
block-aligned (the automatic grid tiles pictures above 4 MP at 512 px);
an `altr`-less file without a patchable `pitm` carries the primary
alone. `HeifDecoderOptions` (`max_width` / `max_height` / `max_pixels`
/ `max_bytes` / `strict` / `tone_mapped` / `item_id` /
`base_layer_fallback` / `reference_white`) bound a decode before any
allocation; `0` keeps the contract default.

## Supported layouts

Decode (the `HeifImage::format` a file yields; the item's `hvcC` /
`av1C` / `avcC` / `vvcC` layout after composition):

| Source | `PixelFormat` |
|---|---|
| HEVC / AV1 / AVC 4:0:0 at 8 / 10 / 12 / 16 bit; VVC 4:0:0 | `Gray8` `Gray10Le` `Gray12Le` `Gray16Le` |
| 4:2:0 at 8 / 10 / 12 / 16 bit, + alpha at 8 / 10 | `Yuv420P` `Yuv420P10Le` `Yuv420P12Le` `Yuv420P16Le` `Yuva420P` `Yuva420P10Le` |
| 4:2:2 at 8 / 10 / 12 / 16 bit, + alpha | `Yuv422P*` `Yuva422P*` |
| 4:4:4 at 8 / 10 / 12 / 16 bit, + alpha (also every derived image whose geometry forces promotion — odd crops, rotations, overlays) | `Yuv444P*` `Yuva444P*` |
| 4:4:4 with `matrix_coefficients = 0` (identity; the planes are G, B, R) | `Gbrp8` `Gbrap8` `Gbrp10Le` … `Gbrap16Le` |
| grey + alpha, 4:2:0 12/16-bit + alpha, 9/11/13/15-bit anything | promoted to the 4:4:4 variant with neutral chroma (no sample lost) |

`to_rgb8` / `to_rgba8` convert every layout above exactly (H.273 §8.3
at the signalled matrix and range, chroma replicated to its co-sited
luma, deeper samples rounded to 8 bits, alpha scaled; opaque when
absent). Gain maps are **not** applied by default — `decode` returns
the base (SDR) rendition; `DecodeOptions::with_tone_mapped(true)`
returns the HEIF Amd 1 §6.6.2.4.1 reconstruction instead.

Encode (`encode(&HeifImage, &EncodeOptions)`; nothing is converted
silently except what the codec's coding layout implies):

| `HeifImage::format` | HEVC (`codec: Hevc`, the default) | AV1 | VVC (`codec: Vvc`) |
|---|---|---|---|
| planar YCbCr / grey (`Yuv*`, `Gray*`) | the image's own chroma sampling (4:0:0 / 4:2:0 / 4:2:2 / 4:4:4) at the coded depth (`hevc_depth`: 8-bit sources at 8, 9–10 at 10, 11–12 at 12, deeper at 10), alpha as the `auxC` item (coded 4:2:0) | the image's own chroma layout and depth (≤ 12; deeper at the coded depth) | the image's own chroma sampling at the coded depth capped at 10 (the `oxideav-h266` encoder's 8 – 10-bit line), alpha as a VVC `auxC` item |
| `Gbrp*` / `Gbrap*` (planar RGB) | lossy (`intra`): RGB → YCbCr through `opts.colr`'s matrix into `EncodeOptions::chroma`'s sampling (4:2:0 by default), then as above; lossless (`pcm`, ≤ 12 bit): coded as-is, 4:4:4 with `matrix_coefficients = 0` | ≤ 12 bit: coded as-is, `matrix_coefficients = 0` (lossless stays lossless); 16-bit: converted to 4:4:4 | RGB → 8-bit 4:2:0 YCbCr through `opts.colr`'s matrix; no lossless mode (`pcm` is refused: the identity-4:4:4 lossless ruling needs a 4:4:4 encoder, which h266 0.0.9 does not have) |
| `Rgb24` / `Rgba` (packed) | lossy (`intra`): RGB → YCbCr through `opts.colr`'s matrix at `EncodeOptions::chroma` (4:2:0 by default — the production path of `oxideav convert` — or 4:4:4), alpha as the `auxC` item; lossless (`pcm`): the G, B, R planes as a 4:4:4 identity-matrix item (`decode` yields `Gbrp8` / `Gbrap8`, `to_rgba8` exact) | lossy: `EncodeOptions::chroma` (`None` = 4:2:0, `Some(Yuv444)` keeps full chroma); lossless (no quality / 100): identity-matrix 4:4:4 as HEVC | RGB → 8-bit 4:2:0 YCbCr through `opts.colr`'s matrix, alpha as the `auxC` item |

`encode_rgb8` / `encode_rgba8` are the packed rows above without the
`HeifImage` (the alpha channel becomes the alpha item): an RGB image
into this YCbCr container is converted to the natural layout exactly
as `encode_rgb8` would, never refused. `HeifImage::new` validates the
plane geometry (`HeifError::InvalidData`), so a malformed image is
refused before anything is coded; a layout the codec cannot take is
`HeifError::Unsupported`.

## Options

Gateway contract (r476): this crate registers the `heif` codec id
only (encoder default `codec=hevc`, a `heic` file; `codec=av1` writes
an `avif`-branded file) — the `avif` codec id belongs to oxideav-avif's
own still encoder, whose packets the `"heif"` muxer writes through
exactly like a `heif` still stream's, so a gateway writes `.avif`
through that encoder on this muxer and `.heic` through the `heif`
encoder; both decode here. The framework `quality`
(0..=100) steers every codec: AV1 codes `mode=intra` at it (an
explicit `quality` < 100 without a `mode` is lossy; 100 is the
lossless coding, the same bytes as `mode=pcm`; absent = 60), and on
HEVC / VVC / AVC
— when `qp` is not given — `qp = round((100 − quality) × 51 / 100)`
(90 → 5, 50 → 26, 0 → 51), with `quality` 100 the lossless `pcm` mode
unless `mode` is given; an explicit `qp` wins; neither given keeps
today's defaults (qp 18, AV1 quality 60).

`DecodeOptions` (`Default` + `with_*`): `max_width` / `max_height`
(`Some(2²⁰)`), `max_pixels` (`Some(2³⁰)`), `max_bytes` (`Some(4 GiB)`;
`None` = unlimited) — checked against the header before any decode or
allocation (`HeifError::LimitExceeded`);
`strict` (a HEIF-family brand is required and a `miaf`-branded file
must pass `miaf::check`; off, any ISOBMFF file with a `pict` item tree
decodes); `item_id` (decode that item instead of `pitm`),
`tone_mapped`, `base_layer_fallback` (layered `lhv1` items), `threads`
(grid tiles decode in parallel; output identical for every budget),
`reference_white_nits`.

`EncodeOptions` (`Default` + `with_*`; the defaults are the production
settings, see *Production defaults*): `codec` (`Hevc` / `Av1` /
`Vvc`), `hevc_mode` (`"intra"` at `qp` 18 / `"pcm"` lossless), `qp`
(HEVC and VVC), `grid_tile`, `thumbnail_max_dim`, `colr`,
`icc_profile` / `exif` / `xmp`, `transforms`, `av1_quality` /
`av1_speed`, `hevc_rd` / `hevc_tiles` / `hevc_ctb` / `hevc_depth` /
`hevc_filters` / `hevc_options`, `vvc_options` (the h266 encoder's
tool knobs: `tiles`, `wpp`, `dep_quant`, `sdh`, `palette`, `mtt_bt`,
`mtt_tt`, `alf_clip_rdo`, `chroma_sao_merge`, …), `gain_map`,
`threads`, `chroma`, `depth_map` (a depth auxiliary, §7.5.3),
`transform_carrier` (`CodedItem`: the chain as essential properties on
every displayed item, the default; `Identity`: on an `iden` derived
item over the un-rotated picture). `encode_overlay` /
`encode_overlay_into` take an `OverlaySpec` (canvas size and
`canvas_fill_value`, `OverlayLayer`s with offsets) and write an `iovl`
over hidden per-layer coded items (alpha auxiliaries where the layers
have alpha), with the same options applying to the overlay.
`HeifWriter::import_item` / `from_files` re-mux items between files
without re-encoding (derivations, auxiliaries, thumbnails, metadata
items and `altr` alternates come along); the `"heif"` muxer merges a
still stream's packets (a burst) into one collection through it. The
framework encoder declares `irot` (quarter turns) and `imir`
(`vertical` / `horizontal`) next to its other options.

## Metadata and colour

Framework carriage (r476, oxideav-core 0.1.40): the `"heif"` demuxer
announces a still stream's ICC profile / Exif / XMP (the first item's)
and a track stream's ICC profile as `CodecParameters::blobs`
(`BlobKind::ICC` / `EXIF` / `XMP`, the payload conventions being this
crate's own forms — a complete profile, the Exif block from the TIFF
header, the XMP packet); the `"heif"` decoder puts each frame's item
metadata on the `VideoFrame` as its blobs; the encoder embeds a
frame's blobs (refined over the stream's) as the file's items; the
sequence muxer writes the stream's ICC profile as the sample entry's
`colr` and its Exif / XMP as items on the cover image.
`HeifImage::into_video_frame` / `from_video_frame`
(`blobs_of_metadata` / `metadata_of_blobs`) carry `Metadata` both ways.


`HeifImage::color` is a `ColorInfo { range, primaries, transfer,
matrix }` (H.273 code points) filled from the item's `colr` `nclx`, or
the MIAF §7.3.6.4 default when the file signals none — BT.709
primaries (1), sRGB transfer (13), BT.601 matrix (6), full range; an
ICC-only `colr` reads as that default too. On encode the image's
colour is the `colr` written, with code points left unspecified (2)
filled from `EncodeOptions::colr`; for RGB sources (packed or `Gbrp*`)
the matrix always comes from `EncodeOptions::colr`, since the image's
describes RGB rather than the YCbCr the encoder derives; next to an
ICC profile the `nclx` carries primaries / transfer 2 (HEIF §6.5.5).
Images built with `HeifImage::new` / `From<HeifFrame>` start with
`ColorInfo::unspecified()`, so `EncodeOptions::colr` governs them as it
governs a bare frame.

`HeifImage::metadata` is a `Metadata { icc, exif, xmp, gamma }`: the
ICC profile (`prof` / `rICC`), the Exif payload from the TIFF header
on (the HEIF offset word resolved; `dExf` inflated with the `deflate`
feature), the XMP packet bytes; `gamma` is always `None` for HEIF. On
encode they are embedded unless `EncodeOptions` carries its own.
`ImageInfo::{has_icc, has_exif, has_xmp}` report their presence from
the header.

## Limits

`DecodeOptions` bounds above, plus the structural caps every path
enforces before allocating: `MAX_ITEMS` 2²⁰, `MAX_PROPERTIES` 2¹⁵,
`MAX_DERIVATION_DEPTH` 16, `MAX_DERIVATION_INPUTS` 2¹⁶,
`MAX_GRAPH_ITEMS` 2¹⁶, `MAX_CANVAS_PIXELS` 2³⁰, `MAX_ITEM_BYTES` 2³⁰,
`MAX_ILOC_DEPTH` 8, `MAX_SAMPLES` 2²⁴, `MAX_ITEM_DECODES` 4096,
demuxer input ≤ 4 GiB in memory. Cycles in `dimg` / `auxl` / `thmb`
graphs and construction-method-2 self-references are rejected. Every
function returns `HeifError` on hostile input, never panics
(`HeifError::Io` wraps the `std::io::Error` of `decode_from` /
`encode_to`); the fuzz
targets cover `probe` / `info` (standalone) and `decode` / `decode_all`
(registry).

---

The rest of this README is the HEIF-specific material: what the
container layer implements, how it scores against the staged corpus
and real-world producers, and the production defaults.

## Capability matrix

| Area | Status |
|------|--------|
| ISOBMFF boxes | size 0 / 1 / `largesize` / `uuid`, FullBox, bounded recursive walk |
| Brands | `mif1` `mif2` `mif3` `msf1` `heic` `heix` `hevc` `hevx` `heim` `heis` `hevm` `hevs` `miaf` `MiHB` `MiHA` `MiHE` `MiAB` `avif` `avis` `avio` `MA1B` `MA1A` `jpeg` `avci` `avcs` `vvic` `vvis` `vvi3` `tmap` `1pic` `pred`, `styp` |
| Low-overhead files | Amd 2:2026 Annex O `mini` box (O.3.2 in full: bit-packed header, chunk sizes, HDR block with gain map) expanded to the normative O.4 equivalent `meta` + `mdat` on parse (`HeifFile::minimized`; a `vvi3` file's `CompactVvcDecoderConfigurationRecord` becomes the equivalent `vvcC` and its header-less `IDR_N_LP` payload the `VVCItemData`, L.4.3.3.4); `encode_still_minimized` writes `mif3` files (HEVC / AV1 / VVC, alpha, ICC, Exif, XMP, gain map, Exif orientation) that libheif renders identically to the regular file; `image/hif2`, `.hmg`; `dExf` / deflated XMP inflate (`deflate` feature) |
| `meta` tree | `hdlr`, `pitm` v0/v1, `iinf` v0/v1 + `infe` v2/v3 (`mime` / `uri ` tails, hidden flag), `iloc` v0–v2 (all widths, construction methods 0 / 1 / 2, multi-extent, zero-length "to end"), `iref` v0/v1, `iprp` / `ipco` / `ipma` v0/v1 (7 / 15-bit indices, essential flag, index-0 placeholders), `idat`, `grpl`, `dinf` / `dref`, `ipro` |
| Properties | `ispe` `pixi` `colr` (nclx + `rICC` / `prof`) `pasp` `clap` `irot` `imir` `iscl` `auxC` `hvcC` `av1C` `lhvC` `oinf` `tols` `avcC` `vvcC` (14496-15 §11.2.4 FullBox, `VvcPtlRecord`, `ptl_present_flag = 0` records read the SPS head in their arrays) `clli` `mdcv` `cclv` `amve` (14496-12 8th ed. §12.1.6–9 plain-Box syntax, FullBox-prefixed writers tolerated) `rloc` `lsel` `a1op` `a1lx` `rref` `crtt` `mdft` `udes` `altt` `reve` `ndwt` `cexg` `dadj` `stag` (Amd 1:2025 §6.5.41–45) `tilC` + `tipa` and the per-channel `pixi` (`px_flags & 1`, Amd 2:2026); §6.5.1 descriptive-before-transformative order, unrecognised-essential refusal, exact rational `clap` |
| Coded items | `hvc1` / `hev1` → oxideav-h265 (`hvcC` extradata, length-prefixed AU); `av01` → oxideav-av1 (`av1C` extradata, temporal unit); `avc1` → oxideav-h264 (`avcC` extradata, length-prefixed AU; byte-exact against a black-box AVC decoder on Baseline / Main / High / 4:2:2 / 4:4:4 / 10-bit); `lhv1` → oxideav-h265's multi-layer decoder (`hvcC` ++ `lhvC` extradata, `layer=<lsel>` / `ols=<tols>`): the stereo shape (two `lhv1` items with `lsel` 0 / 1 in a `ster` group) yields each view, an item without `lsel` every output layer (`DecodedImage::layers`), both views byte-exact against a black-box decoder's per-view output; `vvc1` → oxideav-h266's Annex B stream decoder through this crate's `vvcdec` wrapper (the h266 registry factory is a parser-only placeholder on 0.0.9): `vvcC` arrays + `VVCItemData` re-assembled per L.2.2.2 (AUD / OPI first, the L.2.2.1.2 `tols` OPI rule), conformance window applied, byte-exact against ffmpeg's VVC decoder on every item; `cexg` items decode extent by extent; 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 at 8–16 bit; via direct factories or a caller `CodecRegistry`; grid / `tili` / `cexg` tiles decode in parallel under `ItemDecoder::with_execution_context` (byte-identical for every budget) |
| Derived images | `grid` (row-major, trim, tile alpha), `iovl` (sRGB fill via the H.273 matrix of the output `colr`, offsets, clipping, §6.9.1 straight / pre-multiplied alpha, translucent canvas → output alpha), `iden`, `tmap` (23008-12:2025/Amd 1:2025 §6.6.2.4: base by default, normative tone-mapped reconstruction on request or as an input of another derived item; see Gain maps), `cfen` (Amd 1 §6.6.2.5: the inputs' luma planes become Y / Cb / Cr or R / G / B and alpha; packed inputs (`is_packed_flag`) cut into `rows × cols` regions with guard bands per the Amd 2 §6.6.2.5.3 derivation — see Packed `cfen` below; depth (6) and K (7, the Amd 2 CMYK column) refused typed), `tili` tiled items (Amd 2 §6.11: `deti` offset tables, empty tiles, `ispe` crop; external tiles refused) |
| Transforms | `clap` → `irot` → `imir` → `iscl` in `ipma` order; `iscl` (§6.5.13) resizes by the exact ceil-ratio with an area/bilinear resampler; sub-sample chroma positions promote to 4:4:4 (MIAF §7.3.6.7) |
| Auxiliaries | alpha (`urn:mpeg:mpegB:cicp:systems:auxiliary:alpha` and `urn:mpeg:hevc:2015:auxid:1`, resized / depth-matched, `prem`), depth (both URN families, surfaced as a frame); alpha *tracks* (§7.5.3 `auxv` + `auxl` + `auxi`, plain-Box `auxi` tolerated) composed into a `"heif"` stream of frames with alpha by the demuxer, written by `SequenceWriter::alpha` |
| Metadata | thumbnails (`thmb`), Exif (offset word resolved), XMP, ICC, effective `nclx` (MIAF default when absent), `pixi` / `clli` / `mdcv` / … via the typed property list |
| Entity groups | `altr` (display order), `ster` / `stem` (stereo, monoscopic fallback), `pymd` (pyramids), `rgpa` (region partitions), `brst` / `eqiv` surfaced; Amd 2:2026 §11.3.5 `unrg` (union of regions) / `corg` (compound region: main + the regions it logically includes) with `Meta::region_items_of` / `region_groups_of` (the `rgan` items with a `cdsc` to an image and the groups over them), writer helpers, `check` rules (`"HEIF-A2 11.3.5.x"`) |
| MIAF | `MiafProfile` + `check`: §7 general requirements, §8 shared constraints, Annex A HEVC / AV1 codec limits, HEIF Annex L (`"HEIF L.2.3.1"` essential `vvcC`, `"HEIF L.2.4.3"` `vvs1` + `vvnC`, `"HEIF L.4.1.1"` the `vvic` essential-property set on the brand-carrying item), the HEIF Amd 1 `tmap` / `cfen` shalls (`"HEIF-A1 …"`), MIAF Amd 1:2025 §7.3.11.5 (`tmap` in an `altr` with a master image, `"MIAF-A1 …"`), Amd 2 `pixi` / `mif3` / `tili` rules (`"HEIF-A2 …"`) — typed `MiafViolation`s with clause numbers, should-level rules as `advisories`; MIAF Amd 1 Annex A practices: `Meta::display_order` (`altr` collapse), `Track::loop_behaviour` (`elst` `RepeatEdits`) |
| Image sequences | `moov` / `trak` / `stbl` (`stts` `ctts` `stsc` `stsz` `stz2` `stco` `co64` `stss` `sbgp` `csgp` `sgpd` `tref` `elst`), top-level `prft` / `ssix`, visual sample entries (`hvcC` `av1C` `avcC` `lhvC` `vvcC` `ccst` `auxi` `colr` `clap` `pasp` `clli` `mdcv` `cclv` `amve`; QuickTime zero terminators tolerated), `elst` `RepeatEdits` + `tkhd` duration, §7.2.1 matrix → rotation / mirror; framework `Demuxer` with pts / dts / sync / seek |
| Writer | `HeifWriter` (coded / grid / overlay / identity / `tmap` / `cfen` / `tili` / `cexg` items, thumbnails, alpha / depth, Exif / XMP (raw bodies, any encoding) / arbitrary `cdsc` items, item names, entity groups with flags and payloads (`pymd` pyramids, `stem` stereo with fallback), de-duplicated `ipco`, MIAF `mdat` order, `deti` data references, truthful brand selection — `miaf` only when the §7 / §8 checks pass on the header about to be written, the Annex A profile brands `MiHB` / `MiHA` / `MiHE` / `MA1B` / `MA1A` / `MiAB` only when the primary's coding family satisfies them (r477); `import_item` / `from_files` re-mux items and whole primaries between files byte for byte — every corpus file re-muxes to the same image (`tests/remux.rs`)); high-level `encode_still` (grid / thumbnail / alpha / depth / gain map / Exif / XMP / ICC / transforms on the items or on an `iden`), `encode_overlay` (`iovl` over hidden layers with alpha, thumbnail of the composite); `SequenceWriter` (`msf1` / `hevc` / `avis` / `vvis`, brand override, `pict` track + `ccst`, HDR sample-entry boxes, `stco` / `co64` per §8.7.5, `stss` when not every sample is a sync sample, looping via `elst`, alpha track, cover-image `meta` that can alias a track sample, the same truthful `miaf` / profile pass on the assembled file; opens in every black-box reader on this machine) |
| Gain maps | ISO 21496-1 + HEIF Amd 1:2025 §6.6.2.4 (the published text; "fully applied" = weight ±1.0): `ToneMapImage` body (`version` 0 + C.2 `GainMapMetadata`), `dimg` = [base, gain map] (count 2 enforced), the three `colr` placements and the `tmap` brand (§10.2.6) checked; `DecodedImage::gain_map` (decoded gain-map item + metadata + alternate `colr`), `apply_gain_map(h_target)` → linear RGB in the application space (Formulas 1–3, §6.2.2 resampling, Annex B primaries conversion, single↔multi-channel rules, limited-range clip); `ItemDecoder::tone_mapped()` → the reconstruction in the `tmap` item's `colr` at its `pixi` depth (alpha carried over); writer authors `tmap` items (`HeifWriter::add_tone_map`, `EncodeOptions::gain_map`) with the hidden map, the brand and the `altr` [tmap, base] fallback; matches a black-box tone-mapper within 1 code at every headroom (2 on the half-size map, 7 after a BT.2020→709 conversion of the 16-bit PQ reconstruction), and the tool tone-maps our authored file identically to its own |
| Colour | `HeifImage::to_rgb8` / `to_rgba8` and `rgb::to_rgb` (16-bit): YCbCr → RGB(A) with the item `colr` matrix / range (H.273), identity (GBR), monochrome, alpha carried; the renderer step over the composed frame. Framework: the effective `nclx` rides as the core `ColorSignal` on streams and frames; identity-matrix 4:4:4 items are planar RGB (`Gbrp*` / `Gbrap*`) end to end |
| Encoder (`registry`) | `encode_still` / `encode_still_owned` / `encode_still_into`: HEVC (lossless `pcm` or CABAC `intra` at a QP; 4:2:0 at 8 / 10 / 12 bits — deeper sources code Main 10 (`hevc_depth`), in-loop filters on by default (`hevc_filters`), VUI range + colour description, Main Still Picture, `rd` / `tiles` / `ctb` and the wavefront), AV1 stills (lossless or quality 0..=100, `speed`, native 8/10/12-bit 4:0:0–4:4:4, a size-derived tile layout, `av1C` from the codec configuration) or VVC items (oxideav-h266's IDR encoder at `qp` or lossless `pcm`, 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 at 8 / 10 bits, any size — the encoder pads to 8 and crops with the SPS conformance window, an odd edge via `clap` (`VVC_ALIGNMENT` = 2), `vvcC` from the VPS / SPS / PPS with the PTL from the VPS, `vvc_options` tool knobs; no VUI — colour rides `colr` only) items, padding + `clap`, grid tiling (automatic 512-px tiles above 4 MP, MIAF 64-px floor; tiles coded in parallel under `threads`, bytes identical to serial), thumbnails, alpha (per-codec `auxC` URN, single-channel `pixi`, own parameter-set ids; AV1 alpha as a monochrome still), Exif / XMP / ICC, transforms as essential properties on the coded item; `"heif"` framework `Encoder` (frame in — planar YCbCr / grey / planar RGB (`Gbrp*`, coded identity-matrix 4:4:4 by AV1) or packed RGB / RGBA / BGR(A) / 16-bit / grey+alpha converted straight into the coding layout — file out; `set_execution_context` or the `threads` option; declared options `codec` (`hevc` / `av1` / `vvc` / `avc`) / `mode` / `qp` / `grid` / `thumbnail` / `range` / `quality` / `speed` / `rd` / `tiles` / `ctb` / `threads` / `depth` / `filters` / `chroma`); the `"heif"` muxer passes its whole-file packets through, so `oxideav convert in.png out.heic` writes a HEIC (see Production defaults) |
| Fuzz | `fuzz/`: `heif_parse`, `heif_compose`, `heif_sequence`, `heif_records` (`mini`, `deti`, `cfen` body + region planner + pack / cut identity, `vvcC` / compact VVC record / VVC SPS head and every typed property with round-trip oracles), `heif_api` (`probe` / `info`), `heif_decode` (`decode_with` / `decode_rgba8` / `decode_all_with` under limits, through the codecs), daily workflow |

`iscl` is applied at composition (§6.5.13); `tmap` gain maps are
applied on request (`DecodedImage::apply_gain_map` /
`ItemDecoder::tone_mapped()`), the default output stays the base.

## Codec matrix

What each coding format can do in this container, through the
workspace's own codec crates (published versions: h265 0.0.13, h264
0.1, av1 0.1.19, h266 0.0.9). "Derived" = the item as an input of
`grid` / `iovl` / `iden` / `tmap` and as an alpha / depth auxiliary.

| Codec | Still decode | Still encode | Sequence decode | Sequence encode | Derived | Black-box |
|---|---|---|---|---|---|---|
| HEVC (`hvc1` / `hev1`, `lhv1`) | 4:0:0–4:4:4, 8–16 bit, multi-layer | `pcm` lossless (identity-4:4:4 RGB) / `intra` at `qp`, 8 / 10 / 12 bit 4:2:0 | yes (+ alpha tracks) | yes | yes | reader + writer exact vs ffmpeg / libheif / ImageIO (tables below) |
| AVC (`avc1`) | Baseline–High, 4:2:2 / 4:4:4, 10-bit | `StillCodec::Avc`: one IDR at `qp` through oxideav-h264's registry encoder — 4:2:0 / 4:2:2 / 4:4:4 at 8 / 10 / 12 bits (grey coded 4:2:0), CAVLC Baseline / CABAC Main on 8-bit 4:2:0, High / High 10 / High 4:2:2 / High 4:4:4 Predictive otherwise, lossless `pcm` (transform bypass), `avc_options` (`cabac`, `profile`, `transform_8x8`, `deblock`, `intra_4x4`, `gop`), the encoder's own `avcC` (`format=avcc`), 16-aligned + `clap` | yes | yes (`encode_all`: one IDR + P session per track, `avcs`; the `"heif"` muxer takes `"h264"` Annex B or length-prefixed streams) | yes (grid, alpha, thumbnail, mini) | every coded item and every sequence sample byte-exact in ffmpeg's AVC decoder; `heif-info` and `sips` open the `avci` files, ffmpeg's HEIF demuxer and `magick` do not (no third-party `avc1` item support here); ffmpeg plays the `avcs` track as an ordinary `avc1` track |
| AV1 (`av01`) | 4:0:0–4:4:4, 8 / 10 / 12 bit | lossless / quality 0..=100, native layouts | yes | yes | yes | reader + writer exact (tables below) |
| VVC (`vvc1`) | 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 at the SPS depth | `intra` at `qp` or lossless `pcm` (transform-skip coding), 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 at 8 / 10 bits, any size (the encoder's own conformance window; an odd edge via `clap`), `vvc_options` tool knobs | yes (`vvis`, `vvc1` entries; the demuxer announces `"h266"` + `vvcC`) | yes (`encode_all`, the `"heif"` muxer) | yes (grid, overlay, alpha, gain map, mini) | every coded item byte-exact in ffmpeg's VVC decoder; `heif-info` opens the files; `heif-convert` / `magick` (libheif, no vvdec plugin), `sips` and ffmpeg's HEIF demuxer do not decode `vvc1` items here |

VVC specifics (HEIF Annex L, ISO/IEC 14496-15:2024 §11.2.4): the
contract path, the direct item decoder and the `"heif"` framework
codec are byte-identical on VVC stills (`tests/vvc.rs`); `vvs1`
subpicture items and VVC base items (`subp` / `spor`) are parsed and
checked but not reconstructed (an essential `spor` refuses the item,
§6.5.1). No staged conformance or device-sample file carries a VVC
item, so the workspace encoder's output is the corpus. At one QP on
the same pictures (`tests/vvc.rs`, debug build): gentle 128×128 QP 18
— HEVC 2233 B / 47.8 dB, VVC 951 B / 43.9 dB; QP 30 — HEVC 758 B /
39.5 dB, VVC 606 B / 42.4 dB; smooth 512×512 QP 18 — HEVC 2690 B /
52.5 dB, VVC 3109 B / 55.6 dB. The published h266 encoder's intra
prediction is DC-only, so hard-edged synthetic content (the corpus'
mod-256 sawtooth) lands at ~23 dB at every QP; it also codes only
64-sample-multiple pictures (or a single CTU) conformantly, hence the
padding. Those are oxideav-h266 items, not this crate's.

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

**AVC items written by this crate** (`tests/avc_lhevc.rs`
`avc1_items_written_by_this_crate_decode_in_both_decoders`):
`StillCodec::Avc` codes `avc1` items through oxideav-h264's registry
encoder (one IDR picture, `rc=cqp` at `qp`; CAVLC Baseline, or CABAC
Main with `avc_options` `cabac=1`, on 8-bit 4:2:0; since r476 every
High-family layout the registry encoder takes — 4:2:2 / 4:4:4, 10 /
12 bits — under the §A.2 minimum profile, and lossless under `pcm`
(High 4:4:4 Predictive transform bypass); a 4:0:0 source is coded
4:2:0 with neutral chroma, the encoder's grey line; pictures padded to
16 with `clap`), the encoder's own `avcC` record (`format=avcc`), the
`avci` brand (E.4.1.2, with `mif1` + `miaf`), alpha as an `avc1`
auxiliary under the CICP alpha URN (E.2.4), grid tiles, thumbnails,
the `mif3` low-overhead form with explicit `avc1` / `avcC` types, and
the `"heif"` framework encoder's `codec=avc`. Measured: 96×80 CAVLC
QP 20 → 3.4 KB at 46.8 dB luma PSNR; 62×50 CABAC QP 16 → 2.5 KB at
49.3 dB; every item's stream decodes byte-exact in ffmpeg's AVC
decoder (the raw stream — ffmpeg's HEIF demuxer does not read `avc1`
items, nor does ImageMagick; `heif-info` and Apple ImageIO (`sips`)
open the files). Image sequences (`encode_all`, the `"heif"` muxer)
code each track as one encoder session — an IDR followed by P
pictures, `gop` = the frame count unless `avc_options` says otherwise;
an all-IDR `gop=1` sequence is fine since oxideav-h264 0.1.9 rotates
`idr_pic_id` per session itself (H.264 §7.4.3; this crate's own
rotation of the slice headers is gone). The three-sample `avcs` files
(IDR + P, and three IDR access units through `encode_all` and through
the muxer) decode as three distinct pictures, byte-exact in the
black-box reference decoder against this crate's `decode_all`.

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

**Packed `cfen`** (`is_packed_flag = 1`, Amd 1 §6.6.2.5.2 with the Amd
2:2026 §6.6.2.5.3 region derivation): one coded item's luma plane
carries `(num_cols_minus1 + 1) × (num_rows_minus1 + 1)` channel
partitions, each region the partition cell less a guard band of
`2 × hor_guard_band_mul2` / `2 × ver_guard_band_mul2` samples on every
side (`OrigX = k·W + 2·hg`, `EndX = (k+1)·W − (2·hg + 1)`); a
`packed_channel_id` of 0 marks an unused partition, 1 an unspecified
one, both ignored. The printed cell formula reads `image_width /
num_cols_minus1` — a division by zero for one column and regions past
the picture otherwise — while the sentence introducing it partitions
the picture "into (num_cols_minus1+1) × (num_rows_minus1+1) regions";
this crate cuts cells of `image_width / (num_cols_minus1 + 1)` (the
only self-consistent reading; a remainder column / row is outside every
region and ignored, as the clause's last sentence allows) and the
writer emits the same geometry, so files round-trip and the reading is
stated here rather than hidden. `compose::plan_colour_format_enhancement_regions`
returns every region with its output plane (`CfenRegionPlan`),
`compose::pack_colour_format_channels` builds a packed picture from
channel pictures (guard bands and unused partitions mid-grey), and
`HeifWriter::add_colour_format_enhancement_inputs` takes `CfenInput`
entries (unpacked or packed) per input. `tests/tiled.rs` round-trips
Y + packed [Cb Cr] (4:2:0 and 4:2:2, with guard bands, odd sizes with a
remainder column), one packed [Y Cb Cr] 4:4:4 item, RGB 2×2 with an
unused partition, RGBA 2×2 with guard bands, and a packed alpha next to
an unpacked luma; `check` reports a repeated channel (`HEIF-A1
6.6.2.5.1`) and an unpacked `channel_id` 0 (`HEIF-A2 6.6.2.5.3`). The
Amd 2 Table 2 CMYK column (channel 7 = K) parses and writes, but
composes to nothing: the clause names no `colr` signalling that selects
that column (an `nclx` `colr` is mandatory on the item and H.273 has no
CMYK matrix), so `K` is refused with that message until the
signalling is specified.

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

## Conformance matrix (r474, this machine: macOS 26.6 / Apple M4 Max)

Generated by `tests/conformance_matrix.rs` (every cell is a
measurement from that run; rerun it with `OXIDEAV_HEIF_MATRIX_12MP=1`
in a release build to refresh — it prints the three tables and writes
`matrix.md` to its scratch directory; CI runs it without the 12 MP
rows, which take a 12 MP encode per codec; `OXIDEAV_CLI=/path/to/oxideav`
routes the round-trip table through the real binary). Producers: Apple
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
`gbrp`. The r474 run added this crate's AVC and VVC items to the
writer direction: no third-party decoder on the machine renders
`avc1` or `vvc1` items (libheif without plugins, ImageMagick, ffmpeg's
HEIF demuxer refuse them — their streams are byte-exact in ffmpeg's
raw decoders, see the codec matrix), `heif-info` opens every file,
and Apple ImageIO renders the AVC items (within 1 code at 63×61; its
usual saturated-pixel deltas at 96×80). Every r464 cell is unchanged
in r474 (no regressions).

#### Reader direction (producer → this crate; verdict vs the black-box video decoder)

| Feature | sips (Apple ImageIO) | heif-enc x265 | heif-enc aom | magick | ffmpeg (libsvtav1) |
|---|---|---|---|---|---|
| 1×1 | decodes; no black-box reference (the black-box decoder refuses the file) · 4:4:4 8b | decodes; no black-box reference (the black-box decoder refuses the file) · 4:4:4 8b | exact · 4:2:0 8b | decodes; no black-box reference (the black-box decoder refuses the file) · 4:4:4 8b | producer refused ([vost#0:0/libsvtav1 @ 0x93302c000] Terminating thread with return code -12 (Cann) |
| 7×5 | exact (black-box emits 6×4: even-cropped; overlap compared) · 4:4:4 8b | exact (black-box emits 6×4: even-cropped; overlap compared) · 4:4:4 8b | exact · 4:2:0 8b | exact (black-box emits 6×4: even-cropped; overlap compared) · 4:4:4 8b | exact · 4:2:0 8b |
| 63×61 | Δ max 50 mean 0.86 (black-box emits 62×60: even-cropped; overlap compared) · 4:4:4 8b | Δ max 41 mean 0.80 (black-box emits 62×60: even-cropped; overlap compared) · 4:4:4 8b | exact · 4:2:0 8b | Δ max 41 mean 0.79 (black-box emits 62×60: even-cropped; overlap compared) · 4:4:4 8b | exact · 4:2:0 8b |
| 96×80 | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b |
| 4032×3024 (12 MP) | exact (black-box emits the first 512×512 grid tile; that region compared) · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b | exact · 4:2:0 8b |
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

#### Writer direction (this crate → readers; render vs our decode, 8-bit units; refreshed r477)

| Written shape | sips | heif-convert | magick | ffmpeg | heif-info |
|---|---|---|---|---|---|
| hevc 1×1 | exact | exact | exact | exact | opens |
| av1 1×1 | exact | exact | exact | exact | opens |
| hevc 7×5 | exact | exact | exact | exact | opens |
| av1 7×5 | exact | exact | exact | exact | opens |
| hevc 63×61 | Δ max 2 mean 0.60 | exact | exact | exact | opens |
| av1 63×61 | Δ max 2 mean 0.61 | exact | exact | exact | opens |
| hevc 96×80 | Δ max 2 mean 0.61 | exact | exact | exact | opens |
| av1 96×80 | Δ max 2 mean 0.60 | exact | exact | exact | opens |
| avc 63×61 | REFUSED (Try 'sips --help' for help using this tool) | REFUSED (Could not decode image: Error while loading plugin: No decoding plugin installed) | REFUSED (magick: no decode delegate for this image format `/var/folders/8b/pm870yn91m90z2) | REFUSED (Error opening output files: Invalid argument) | opens |
| vvc 63×61 | REFUSED (Try 'sips --help' for help using this tool) | REFUSED (Could not decode image: Error while loading plugin: No decoding plugin installed) | REFUSED (magick: Error while loading plugin: No decoding plugin installed for this compre) | REFUSED (Error opening output files: Invalid argument) | opens |
| avc 96×80 | REFUSED (Try 'sips --help' for help using this tool) | REFUSED (Could not decode image: Error while loading plugin: No decoding plugin installed) | REFUSED (magick: no decode delegate for this image format `/var/folders/8b/pm870yn91m90z2) | REFUSED (Error opening output files: Invalid argument) | opens |
| vvc 96×80 | REFUSED (Try 'sips --help' for help using this tool) | REFUSED (Could not decode image: Error while loading plugin: No decoding plugin installed) | REFUSED (magick: Error while loading plugin: No decoding plugin installed for this compre) | REFUSED (Error opening output files: Invalid argument) | opens |
| hevc lossless (pcm) | Δ max 2 mean 0.61 | exact | exact | exact | opens |
| av1 lossless | Δ max 2 mean 0.61 | exact | exact | exact | opens |
| hevc 4:0:0 (grey) | exact | exact | exact | exact | opens |
| av1 4:0:0 (grey) | exact | exact | exact | exact | opens |
| av1 4:4:4 10-bit | Δ max 0 mean 0.00 | Δ max 0 mean 0.12 | Δ max 1 mean 0.27 | Δ max 1 mean 0.45 | opens |
| hevc alpha | Δ max 2 mean 0.61 | exact | exact | exact | opens |
| av1 alpha | Δ max 2 mean 0.60 | exact | exact | exact | opens |
| hevc thumbnail | Δ max 2 mean 0.61 | exact | exact | exact | opens |
| hevc Exif+XMP+ICC | Δ max 2 mean 0.61 | exact | exact | exact | opens |
| hevc irot | Δ max 2 mean 0.61 (ImageIO orientation 8 applied) | exact | exact | exact | opens |
| hevc imir | Δ max 2 mean 0.61 (ImageIO orientation 2 applied) | exact | exact | exact | opens |
| hevc clap | Δ max 1 mean 0.57 | exact | exact | exact | opens |
| hevc irot+imir | Δ max 2 mean 0.61 (ImageIO orientation 7 applied) | exact | exact | exact | opens |
| hevc iden (irot+imir on an iden) | Δ max 2 mean 0.61 (ImageIO orientation 7 applied) | exact | exact | opens; renders 96×80: rotation not applied | opens |
| av1 iden (irot+imir on an iden) | Δ max 2 mean 0.61 (ImageIO orientation 7 applied) | exact | exact | opens; renders 96×80: rotation not applied | opens |
| hevc depth auxiliary | Δ max 2 mean 0.61 | exact | exact | exact | opens |
| av1 depth auxiliary | Δ max 2 mean 0.61 | exact | exact | exact | opens |
| hevc overlay (2 layers, alpha) | Δ max 3 mean 0.48 | Δ max 3 mean 0.41 | Δ max 3 mean 0.41 | Δ max 201 mean 47.22 (renders 88×72; overlap compared) | opens |
| av1 overlay (2 layers, alpha) | Δ max 3 mean 0.48 | Δ max 3 mean 0.41 | Δ max 3 mean 0.41 | Δ max 201 mean 47.22 (renders 88×72; overlap compared) | opens |
| hevc grid 64 + thumbnail | Δ max 2 mean 0.62 | exact | exact | exact | opens |
| hevc collection (3 pictures, per-item Exif) | Δ max 2 mean 0.61 | opens | opens | exact | opens |

#### Round trip at the production defaults (PNG → file → PNG; PSNR vs the source, 8-bit RGB)

`oxideav convert src.png out.heic` / `out.avif` with no options (the
real binary, `OXIDEAV_CLI`), the file decoded back by this crate and
rendered by every reader; the 96×80 rows are bounded by the
picture's own 4:2:0 chroma (its lossless 4:2:0 coding is ~35 dB). The
AVIF rows moved with the AV1 crate between r464 (0.1.19) and r474
(0.1.20): 96×80 763 → 817 bytes at 33.01 → 32.91 dB, 12 MP 28066 →
28862 bytes at 47.35 → 47.31 dB — the codec's rate point, not this
container's doing; the HEIC rows are byte-identical.

| File | bytes | this crate | sips | heif-convert | magick | ffmpeg | heif-info |
|---|---|---|---|---|---|---|---|
| HEIC 96×80 | 1517 | 33.82 dB | 32.83 dB | 33.82 dB | 33.82 dB | 33.81 dB | opens |
| AVIF 96×80 | 817 | 32.91 dB | 32.13 dB | 32.92 dB | 32.92 dB | 32.91 dB | opens |
| HEIC 4032×3024 (12 MP) | 90131 | 47.69 dB | 46.84 dB | 47.69 dB | 47.69 dB | 47.57 dB | opens |
| AVIF 4032×3024 (12 MP) | 28862 | 47.31 dB | 46.65 dB | 47.30 dB | 47.30 dB | 47.19 dB | opens |

The `av1 4:4:4 10-bit` row is a real 10-bit 4:4:4 item since r464 (a
16-bit source used to drop to 8-bit 4:2:0 on the AV1 path; Apple
ImageIO renders the 4:4:4 item exactly). Every reader-direction cell
that is not `exact` is explained by the
producer (`no producer`, `dropped`) or by the reference decoder (the
1×1 refusals; the 63×61 4:4:4 HEIC files, where ffmpeg's even-cropped
output differs from ours by up to 50 codes at scattered pixels while
the producers' own PNG renders of the same files match this crate
within 1 code — `tests/interop.rs`, vendored oracles). Every writer
direction cell opens. The `sips` cells are Apple ImageIO's own colour
pipeline on 4:2:0 (within 2 codes, mean ≈ 0.6; 4:0:0 and 4:4:4 exact);
ImageIO maps `irot` / `imir` — on the coded item or on an `iden` —
onto the Exif orientation it reports (probed through ImageIO itself,
`tests/common/imageio.swift`; 8 for a 90° `irot`, 2 for a horizontal
`imir`, 7 for both) and hands out the un-rotated pixels, so the
`irot` / `imir` / `iden` cells apply the reported orientation to its
render before comparing (a viewer honouring the orientation shows the
intended picture). ffmpeg's HEIF demuxer does not apply an `iden`'s
transforms and does not compose an `iovl` (it renders one of the
inputs); the other readers do. An overlay's sRGB canvas fill is
composed in the item's 8-bit YCbCr here and in RGB by the readers, so
the fill and the pixels blended over it differ by that colour's YCbCr
quantisation (≤ 3 codes); the layers match exactly. A collection's
images come out of `heif-convert` / `magick` as several files
("opens"); the primary renders exactly in ffmpeg. Features no
tool here produces — XMP items, gain maps (`avifgainmaputil` is a
converter, not a still-image encoder), 12-bit / 4:2:2 / lossless from
Apple ImageIO, AVC / L-HEVC items — are covered by this crate's own
writer round-trips (`tests/gainmap.rs`, `tests/avc_lhevc.rs`,
`tests/writer.rs`) and, for gain maps, the vendored oracle files.


## Production defaults (r464)

`oxideav convert in.png out.heic` with no flags — equally
`EncodeOptions::default()` / the `"heif"` encoder with no options —
writes:

* **HEVC Main / Main Still Picture, 4:2:0 8-bit, `qp` 18, CABAC intra
  with deblocking + SAO** (`DEFAULT_QP`, `HEVC_FILTERS_DEFAULT`) —
  the historical one-CU-per-CTB coder for 8-bit 4:2:0 (the fast one);
  a source deeper than 8 bits codes **Main 10** (16-bit PNG → 10-bit
  4:2:0; `depth=8|10|12` overrides, `coded_depth` is the rule) on the
  quadtree coder at its level-0 search (`rd=1|2` buys bytes for CPU,
  see below).
* **Full-range `nclx`** (BT.709 primaries, sRGB transfer, BT.601
  matrix — the MIAF default) in the `colr` and in the VUI
  (`range=limited` flips both), refined by the source's colour
  signal when it carries one (a frame `ColorSignal` record, else the
  stream's: BT.2020 / PQ / limited-range YCbCr sources are labelled
  as such; an RGB source's matrix is not taken — the encoder derives
  the YCbCr itself).
* **Automatic 512-px `grid` above 4 MP** (`GRID_AUTO_TILE` /
  `GRID_AUTO_MIN_PIXELS`; the tile the OS producer uses for a 12 MP
  picture): the codec's working set scales with a tile, not the
  picture (501 → 95 MiB for 12 MP), and the tiles are the parallel
  unit. `grid=none` forces a single item, `grid=N` a tile size.
* **Thumbnail off**, **4:2:0 for packed RGB sources** (`chroma=444`
  keeps 4:4:4 on AV1; lossy HEVC items are always 4:2:0; the lossless
  mode — `mode=pcm`, AV1 at quality 100 — keeps a full-range RGB
  source as an identity-matrix 4:4:4 item so `decode(encode(rgb)) ==
  rgb`; `range=limited` asks for the YCbCr conversion), **Exif / ICC
  carried when the caller supplies them** (`EncodeOptions::exif` /
  `icc_profile` — the framework frame has no side-channel for either:
  core carries palette / significant-bits / colour-signal / layer
  records only, so the PNG decoder's `iCCP` / `eXIf` cannot reach
  this encoder through `oxideav convert` today; a core ask).
* `.avif` → **AV1 quality 60, `fast`**, the same grid rule (reported
  as measured; the AV1 encoder is its own crate's work).

**Measured against the OS encoder** (Apple ImageIO through `sips -s
format heic`, default quality) on a 3024×4032 iPhone photograph (PNG
source, PSNR in 8-bit RGB / luma against it, rendered through
`heif-convert`):

| Encoder | bytes | RGB PSNR | Y PSNR | wall (8 threads) | CPU |
|---|---|---|---|---|---|
| sips (default) | 1 251 803 | 44.98 dB | 47.40 dB | 0.17 s | — |
| this crate, defaults (`qp` 18, filters, 512 grid) | 1 429 494 (+14 %) | 45.07 dB | 47.66 dB | 0.71 s | 5.1 s |
| `qp=19` | 1 319 243 (+5 %) | 44.61 dB | 47.12 dB | 0.9 s | 5 s |
| `qp=20` | 1 178 719 (−6 %) | 43.90 dB | 46.33 dB | 0.9 s | 5 s |
| `qp=19 rd=1` (quadtree + wavefront) | 1 191 453 (−5 %) | 45.07 dB | 47.35 dB | 4.7 s | 26 s |
| `qp=19 rd=2` | 1 183 626 (−5 %) | 45.12 dB | 47.42 dB | 6.7 s | 42 s |
| `qp=18 filters=off` | 1 414 377 (+13 %) | 44.91 dB | 47.58 dB | 0.7 s | 3.4 s |

`qp` 18 is the lowest QP of the fast coder that is at or above the
OS encoder's PSNR (RGB and luma); it costs +14 % bytes, one QP step
more lands −6 % bytes at −1.1 dB. The quadtree coder's mode decision
(`rd=1`) reaches the OS encoder's size at its PSNR for 5× the CPU —
it is one option away, not the default. On a smooth 4032×3024
wallpaper the same defaults write 117 KB where the OS encoder writes
361 KB (−0.4 dB): its default is a quality factor, not a QP. AV1 at
the `quality` dial on the same photograph (8 threads): 60 → 512 KB /
39.25 dB in 5.9 s, 70 → 693 KB / 40.67 dB, 80 → 936 KB / 42.36 dB,
88 → 1 282 KB / 44.28 dB (~13 s each).

## Encode budget (r464; r463 baseline)

`examples/heifencbench` (`cargo run --release --example heifencbench
-- --raw in.rgb --size WxH [--fmt rgb24|rgb48le] [--threads N|auto]
[--framework] …`), same machine as the decode table (Apple M4 Max, 16
cores, macOS 26.6, rustc 1.98.1, release). Source: the 3024×4032
iPhone photograph as packed RGB (8-bit) and RGB48 (the 10-bit rows:
a 16-bit source coded Main 10). **library** = `packed_to_planar_for`
+ `encode_still` (the conversion is 0.07 s / 54 MiB of every row);
**framework** = the registry `"heif"` encoder's `send_frame` (what
the CLI runs). Wall and CPU are the process's (`getrusage`), peak RSS
its `ru_maxrss`. Every row's FNV-1a moved with the thread budget for
no configuration.

| configuration | wall | CPU | peak RSS | bytes |
|---|---|---|---|---|
| 8-bit hevc intra (defaults) library t1 | 4.58 s | 4.58 s | 95 MiB | 1 429 494 |
| 8-bit hevc intra (defaults) library t4 | 1.27 s | 4.86 s | 135 MiB | = |
| 8-bit hevc intra (defaults) library t8 | 0.71 s | 5.08 s | 159 MiB | = |
| 8-bit hevc intra (defaults) framework t1 | 4.56 s | 4.56 s | 77 MiB | = |
| 8-bit hevc intra (defaults) framework t4 | 1.25 s | 4.79 s | 127 MiB | = |
| 8-bit hevc intra (defaults) framework t8 | 0.68 s | 4.92 s | 148 MiB | = |
| 8-bit hevc intra grid=none t1 (library / framework) | 4.45 / 4.39 s | 4.45 / 4.38 s | 501 / 504 MiB | 1 414 606 |
| 8-bit hevc intra grid=1024 t8 (library / framework) | 0.86 / 0.87 s | 4.90 / 4.96 s | 351 / 335 MiB | 1 422 653 |
| 8-bit hevc pcm t1 (library / framework) | 0.47 / 0.47 s | 0.47 / 0.47 s | 95 / 95 MiB | 18 981 202 |
| 8-bit hevc pcm t8 (library / framework) | 0.12 / 0.12 s | 0.49 / 0.49 s | 116 / 114 MiB | = |
| 10-bit hevc intra (defaults) library t1 | 16.2 s | 16.2 s | 173 MiB | 1 308 705 |
| 10-bit hevc intra (defaults) library t4 | 4.36 s | 17.1 s | 231 MiB | = |
| 10-bit hevc intra (defaults) library t8 | 2.32 s | 17.7 s | 272 MiB | = |
| 10-bit hevc intra (defaults) framework t1 / t4 / t8 | 16.7 / 4.41 / 2.33 s | 16.6 / 17.4 / 18.0 s | 140 / 185 / 222 MiB | 1 307 995 |
| 10-bit hevc intra grid=none t1 (library / framework) | 15.9 / 16.0 s | 15.9 / 16.0 s | 602 / 613 MiB | 1 290 413 / 1 290 172 |
| 10-bit hevc intra grid=1024 t8 (library / framework) | 2.99 / 2.96 s | 17.6 / 17.5 s | 477 / 448 MiB | 1 299 052 / 1 298 228 |
| 10-bit hevc pcm t1 / t8 (library) | 0.61 / 0.17 s | 0.61 / 0.63 s | 169 / 185 MiB | 23 699 842 |
| 8-bit av1 quality 60 (defaults) library t1 / t4 / t8 | 86.5 / 21.7 / 11.4 s | 86.5 / 86.4 / 89.7 s | 137 / 208 / 263 MiB | 512 724 |
| 8-bit av1 quality 60 (defaults) framework t8 | 11.6 s | 91.5 s | 227 MiB | = |
| 8-bit av1 grid=none t8 (library) | 62.3 s | 91.8 s | 1 576 MiB | 484 692 |
| 10-bit av1 quality 60 (defaults) t8 (library / framework) | 11.6 / 11.6 s | 90.5 / 90.8 s | 364 / 303 MiB | 523 554 / 523 527 |

The AV1 rows run the published oxideav-av1 0.1.19 (the standalone
build); the CLI below is the umbrella build on the av1 sibling's
current master, which is 3–8× faster on the same picture (37.6 then
10.7 s serial during the round) — the still encoder is that crate's
work this round. A
single-item 12 MP AV1 still is 1.6 GB of the codec's state and 62 s
on 8 threads; the automatic grid is what makes the AVIF default
usable (263 MiB, 11 s).

(The 10-bit framework bytes differ from the library's by design: the
direct packed → 10-bit conversion rounds the 10-bit result of the
H.273 matrix, the bench's library path rounds a 16-bit result to 10
bits — one code apart on a few samples, pinned within 1 in
`tests/encode_budget.rs`.)

**Through the CLI** (`oxideav convert photo.png out.heic`, built
from the umbrella root in release; `/usr/bin/time -l`, the PNG
decode included):

| command | r463 | r464 serial (`--opt threads=1`) | r464 default (the executor's budget) | r464 `--opt threads=8` |
|---|---|---|---|---|
| PNG → HEIC (12 MP) | 3.4 s / 627 MiB | 4.30 s / 148 MiB | 1.63 s / 239 MiB | 0.92 s / 199 MiB |
| HEIC → PNG | — | — | 0.17 s / 162 MiB | — |
| PNG → AVIF (12 MP, quality 60) | ≈90 s | 10.7 s / 157 MiB | 10.7 s / 157 MiB (serial: see below) | 1.85 s / 222 MiB |
| AVIF → PNG | — | — | 0.21 s / 219 MiB | — |

The r463 wall was the unfiltered `qp` 26 single-item encode; the
r464 serial number is the production default (filters on, `qp` 18,
48 tiles) and the memory is the point: **627 → 148 MiB serial, 239
MiB at the executor's default budget** (the 16-thread auto budget
holds more tiles in flight; 8 threads 199 MiB), against the ≤ 250
MiB target. (The AVIF wall is the av1 sibling's master on the day —
it moved twice during this round, 37.6 → 10.7 s serial; the bytes
are identical for every budget on each build.) The `.heic` job goes through the pipeline executor,
which hands the encoder `ExecutionContext::auto`; the `.avif` job
carries an implied `codec=av1` option and runs through
`oxideav-cli-convert`'s frame tap, which builds the encoder without
an execution context — so AVIF is serial unless `--opt threads=N`
is given (a cli-convert ask; the encoder side is ready: the `--opt
threads=8` file is byte-identical to the serial one).

**Where the time and memory go** (12 MP 8-bit HEVC, defaults,
serial, library path): packed RGB → 4:2:0 conversion 0.07 s, +18
MiB (the coding picture; the packed source is 35 MiB); grid cut +
pad: one 512×512 tile at a time, cut straight from the picture with
edge replication (0.4 MiB; no padded canvas); the HEVC encoder 4.4 s
of the 4.6 s and ~40 MiB of working set per instance (the historical
coder's per-picture state; one instance per worker, which is the
t1 → t8 RSS growth: 95 → 159 MiB); the writer: the coded tiles
(1.4 MB) + the `ftyp` / `meta` bytes, streamed into one output
buffer — no assembled `mdat` copy. Single-item (`grid=none`) is the
same encoder over the whole 12 MP picture: 501 MiB, all of it the
codec's state. 10-bit sources run the quadtree coder (3.6× the CPU
at level 0; ~150 MiB per instance), AV1 its tile search (the
sibling crate's cost; the rows above).

What changed on this side (r464): coding pictures are moved into
the codecs instead of cloned (`encode_still_owned`, the alpha plane
split off without a copy), padding / thumbnail / alpha cuts
allocate only when they change something, packed RGB(A) converts
straight into the coding layout row pair by row pair
(`packed_to_planar_for` — the r463 path built a full-resolution
4:4:4 16-bit intermediate then converted again: 36 + 108 + 18 MiB
for 12 MP), the writer streams `mdat` from the item bodies
(`HeifWriter::write_to`), the muxer validates a still packet with
the borrowing parser, and the thread budget reaches the grid tiles
(independent jobs, handed to the writer in order as they complete —
the side buffer holds only out-of-order completions), the HEVC
wavefront / tile fan-out (`wpp` is always on when the quadtree coder
runs without tiles, so the bytes never depend on the budget) and the
AV1 tile search (over a layout derived from the picture size for
`AV1_TILE_LAYOUT_THREADS` workers — likewise budget-independent).

## Decode performance (r463; baseline r462)

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

Decode, phase 1 (r463, container side): grid / `tili` / `cexg` tiles
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
spent on independent items only.

## Constructing the crate's types

Every public record (typed properties, `meta` records, codec
configurations, descriptors, `EncodeOptions`, …) is
`#[non_exhaustive]`: build it with `Type::new(<every field, in
declaration order>)` — or `Default` where one exists — then read /
assign its public fields; the option records `EncodeOptions`,
`HeifEncoderOptions` and `GainMapSpec` also take `with_<field>`
setters (`EncodeOptions::default().with_qp(20).with_threads(Some(8))`).
The enums that grow with the standard (`Property`, `Colr`,
`HeifError`, `StillCodec`, `MiafProfile`, …) are `#[non_exhaustive]`
too: match them with a `_` arm. `Chroma`, `HeifFrame` / `Plane` /
`HeifPixelFormat`, `CropRect` and `Rational` stay plain (closed by the
standard, or the fundamental sample model). `HeifPlane` is the
deprecated alias of `Plane`; the interleaved 16-bit picture
`rgb::to_rgb` returns is `RgbImage16` (the contract's 8-bit
`RgbImage` / `RgbaImage` live at the root).

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

## License

MIT — see [LICENSE](./LICENSE).
