# Changelog

## 0.3.2 — 2026-10-31

**Smaller HEIC files.** At equal quality, the encoder now makes files slightly smaller than x265's
`medium` preset (BD-rate −0.6% on a photo, −0.5% on an image with text), against +11% and +13%
before. Encoding is about 4× slower (a 12 MP image in 1.6 s on 10 cores).

- 4×4 prediction blocks in 8×8 coding units, and transform blocks split in four (with the 4×4
  DST for luma); each coding unit tries these against one transform and keeps the cheapest.
- The three most promising luma modes are coded for real and compared by rate-distortion cost;
  the chroma mode is chosen too, instead of always following luma.
- RDOQ: levels, sub-blocks and the last position chosen with bit costs estimated from the CABAC
  contexts. Sign data hiding.
- Coding units that need no residual are not split further.
- `heifer-hevc-dec`: `Reconstructor::set_reconstructed` (for encoders that try several codings of
  an area) and the `DST4` matrix are public. `heifer-hevc-enc`: `transform::forward_dst4` and
  `quantize_exact`, and an `encode_yuv` example for rate-distortion measurements.

## heifer-cli 0.3.1 — 2026-10-31

- New `heifer-cli` crate: the `heifer` command converts HEIC/HEIF to and from PNG, JPEG, WebP,
  TIFF, BMP and GIF, one file or many (`--to jpg -o dir`), keeps EXIF and ICC profiles, writes
  photos upright, and never overwrites files without `--force`. `heifer info` shows the size,
  tiles, camera, date and metadata of a file.
- Web demo: PNG, JPEG and WebP images are converted to HEIC in the browser (in a worker), with a
  quality slider and the result shown as decoded from the HEIC file.

## 0.3.1 — 2026-10-31

- `image` crate: `HeifEncoder` implements `image::ImageEncoder`, so that
  `img.write_with_encoder(HeifEncoder::new(file))` writes HEIC files. 8- and 16-bit grey and
  RGB, with or without alpha; the ICC profile and EXIF set by `image` are stored.
- `encode_with_metadata`: encoding with EXIF, XMP and the ICC profile, e.g. those of the
  original file. The EXIF orientation is reset to 1, since decoded pixels are already upright.
- `heifer-isobmff`: `HeifWriter::add_xmp` and `add_icc_profile`.

## 0.3.0 — 2026-10-31

**Encoding.** heifer now writes HEIC files, in pure Rust.

- `heifer::encode`, `encode_rgba8` and `encode_rgb8`, with `EncodeOptions` (quality 0–100,
  threads): 8-bit RGB(A) to YCbCr 4:2:0 (sRGB, BT.601 full range), 512×512 grid tiles encoded in
  parallel, alpha as a monochrome auxiliary image, any size (odd sizes cropped with `clap`). A 12 MP
  image takes about 0.4 s on 10 cores. The files are decoded by Apple's ImageIO and by ffmpeg.
- `heifer-hevc-enc`: HEVC Main intra encoder (`encoder::encode_picture`). Coding blocks of 8×8 to
  32×32 and the luma mode are chosen by rate-distortion cost; the picture is reconstructed with the
  decoder's code, so that the decoder reproduces it sample for sample (checked by the tests).
  `ArithmeticEncoder` gains `bit_count` and checkpoints.
- `heifer-isobmff`: `writer::HeifWriter` writes HEIF files: HEVC image items, grids, alpha planes,
  EXIF, `colr`, `pixi`, `clap`, shared properties.
- Decoding: alpha planes of grid images (an alpha grid referencing the image grid).
- **Container**: `iloc` construction method 2: item data built from ranges of other items
  (`iloc` references), nested or not, with limits against reference cycles and data blow-up.
- New fuzz target `encode` (encode then decode random images), and a `to_heic` example.
- `heifer_hevc_dec::recon::DCT32` is public.
- **Breaking:** `heifer_isobmff::boxes::Extent` has a new `index` field (`extent_index`);
  `heifer::Error` has a new `Encode` variant.

## 0.2.1 — 2026-10-31

- **HDR**: ISO 21496-1 gain maps stored as HEIF `tmap` items (per-channel gain, gamma, offset
  and headroom). `decode_hdr` uses them in preference to Apple gain maps when both are present.
- crates.io metadata: documentation links, a README for each crate, and keywords and
  categories per crate (the decoders are no longer listed under encoding).

## 0.2.0 — 2026-10-08

About 2× faster than 0.1.3 for `decode` + `to_rgba8`, and faster again on large photos.

- CABAC: `ivlOffset` kept scaled with bytes read ahead (one shift per renormalization) and
  branchless bypass bins.
- Grid tiles without transforms or alpha are colour-converted straight into the final image,
  each tile into its own row segments (no intermediate image, no shared lock).
- Fixed-point YCbCr → RGB: 116 of 195 million samples of the test files change by 1.
- `to_rgba8`/`to_rgb8`: plain copy for 8-bit images, lookup table otherwise.
- Intra reference samples gathered per 4×4 block; angular prediction transposed in place;
  scaling list factors from a lookup table instead of a search.
- **Breaking:** `heifer_hevc_dec::syntax::TransformBlock` has a new `coeff_bounds` field;
  `heifer::color::frame_to_rgba_into` is new.

## 0.1.3 — 2026-10-08

- **HDR photos**: Apple HDR gain maps (iPhone). `decode_hdr` returns linear-light RGB with the
  gain map applied, matching Apple's decoder (ImageIO) within 0.3% on average; `read_gain_map`
  gives the gain map and headroom to apply them yourself. `Exif::apple_hdr_headroom` reads the
  headroom from the Apple MakerNote.

## 0.1.2 — 2026-10-08

Faster again, still without any `unsafe` code and with bit-identical output.

- The inverse transform works on fixed-size, contiguous rows and the intra prediction writes whole
  rows, so the compiler vectorizes them (NEON, SSE/AVX, wasm simd128).
- About 10% faster single-threaded (12 MP iPhone photo: 208 → 187 ms on one thread, 42 → 36 ms on
  ten). In the browser, a 1280×854 photo decodes in ~80 ms instead of ~115 ms.

## 0.1.1 — 2026-10-08

Faster decoding, with bit-identical output.

- About 2.2× faster single-threaded and 2.4× faster on 10 cores (12 MP iPhone photo: 454 → 208 ms
  on one thread, 102 → 42 ms on ten). In the browser, a 1280×854 photo decodes in ~115 ms instead
  of ~260 ms.
- Inverse transform without allocations, using only the non-zero coefficients and an even/odd
  decomposition of the DCT.
- Intra prediction without allocations; neighbour availability computed once per 4×4 block.
- Table-based YCbCr → RGB conversion, cache-friendly rotation, fewer image copies.
- Fuzzing now runs almost continuously on CI.

## 0.1.0 — 2026-10-08

First release: a pure-Rust HEIF/HEIC decoder.

- **HEIF container** (`heifer-isobmff`): items, properties, references, grids, overlays, alpha,
  `clap`/`irot`/`imir`, EXIF, XMP and ICC access.
- **HEVC decoder** (`heifer-hevc-dec`): Main, Main 10, Main Still Picture and intra Range
  Extensions (8–12 bit, 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4), tiles, WPP, PCM, scaling lists, deblocking
  and SAO, decoded picture hash verification. Identical to ffmpeg on 62/62 comparable conformance
  bitstreams; 41 verified against the reference decoder's hashes.
- **`heifer::decode`**: RGB(A) output with grid assembly (tiles decoded in parallel), overlays,
  alpha, transforms and YCbCr→RGB conversion; `read_metadata` for EXIF/XMP/ICC.
- **`image` integration** (feature `image`): `HeifDecoder` and hooks so that `image::open` reads
  `.heic` files.
- **Safety**: `#![forbid(unsafe_code)]`, configurable size limits, fuzzed nightly (5 targets).
- **WebAssembly**: builds for `wasm32-unknown-unknown` (≈ 250 KB demo).

Not yet supported: HDR gain maps, applying ICC profiles, extended precision (16-bit) HEVC
tools, inter-coded image sequences, encoding (`heifer-hevc-enc` only contains the CABAC encoder).
