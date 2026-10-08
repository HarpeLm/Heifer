# Changelog

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
