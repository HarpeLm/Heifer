# Changelog

## 0.1.0 — unreleased

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
