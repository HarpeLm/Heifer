<!-- Draft announcement for r/rust and users.rust-lang.org. Adapt it in your own words before posting.
     Suggested title: "heifer 0.1: a pure-Rust HEIF/HEIC decoder (no C, no unsafe, runs in the browser)" -->

# heifer 0.1: a pure-Rust HEIF/HEIC decoder (no C, no unsafe, runs in the browser)

**Try it:** https://harpelm.github.io/Heifer/. Drop a `.heic` photo from your phone and heifer,
compiled to WebAssembly, decodes it locally in your browser (nothing is uploaded).

**Repo:** https://github.com/HarpeLm/Heifer · **crates.io:** https://crates.io/crates/heifer

## Why

HEIC is the default photo format on iPhones and many Android phones. In Rust, the usual way to read it
is `libheif-rs`, which binds to libheif and libde265 (C/C++). That works, but it needs either a system
library or a C toolchain. It also makes cross-compilation harder, rules out
`wasm32-unknown-unknown`, and exposes a large C parser to whatever files users upload.

heifer is an independent implementation of both layers, in safe Rust:

- the HEIF container (ISOBMFF boxes, items, grids, overlays, alpha, transforms, metadata);
- an HEVC intra decoder (CABAC, intra prediction, transforms, deblocking, SAO, Range Extensions).

```rust
let image = heifer::decode(&std::fs::read("photo.heic")?)?;
let rgba = image.to_rgba8();
```

With the `image` feature, `heifer::image_crate::register_image_decoder_hooks()` makes
`image::open("photo.heic")` work.

## How I checked it is correct

A video decoder is easy to get subtly wrong, so validation was the main focus:

- **HEVC conformance:** I tested the first picture of each of 66 official ITU-T conformance bitstreams.
  - On the 62 streams that ffmpeg can also decode, heifer's output is identical to ffmpeg's, sample for sample.
  - 41 streams are also checked against the picture hashes (MD5/checksum SEI) embedded in the official streams. That includes 3 streams that ffmpeg cannot decode.
  - One stream is unsupported (16-bit extended precision).
- **Real photos:**
  - iPhone 13 Pro: 48-tile grid, with an HDR gain map and a depth map in the file;
  - rotated iPhone 8 Plus files;
  - an iPhone 15 Pro spatial photo;
  - a 200 MP Samsung Galaxy S24 Ultra file with 768 tiles, which ffmpeg fails to decode;
  - Xiaomi, Sony, 8/10/12-bit and alpha samples.
- **Robustness:**
  - `#![forbid(unsafe_code)]` in every crate, plus configurable size limits;
  - five cargo-fuzz targets run every night on CI: full decode, container, HEVC, a parallel-vs-sequential differential check, and a CABAC round trip;
  - fuzzing found a memory-exhaustion bug, now fixed;
  - corrupted and truncated files return errors, never panics.

## Performance

Median decode time on a 10-core Apple Silicon Mac:

| Image | heifer | heifer, 1 thread | ffmpeg |
|---|---|---|---|
| Xiaomi, 5 MP | 55 ms | 242 ms | 57 ms |
| iPhone 13 Pro, 12 MP | 102 ms | 454 ms | 96 ms |
| 8000×6000, 48 MP | 480 ms | 2.4 s | 583 ms |
| Galaxy S24 Ultra, 200 MP | 1.6 s | 5.7 s | fails |

heifer is on par with ffmpeg on phone photos because it decodes the tiles in parallel. On a single
thread it is still about 4× slower, and that is the main thing I want to improve.

## How it was made

AI coding tools are part of how software gets written today, and I used them on this project. They
saved me a lot of time, but everything went through review and testing, and correctness was checked
against external references (conformance streams, ffmpeg, real photos, fuzzing) rather than taken on
trust. If something looks wrong, please open an issue.

## What's missing / next

- HDR gain maps (Apple and ISO 21496-1) and ICC colour management;
- faster single-threaded decoding;
- an encoder: HEIF writing plus an HEVC intra encoder. As far as I know, it would be the first
  pure-Rust HEIC encoder;
- image sequences (inter prediction) are out of scope for now.

## Feedback welcome

Bug reports are most useful when they come with a sample file. HEIC files from unusual cameras or
apps are especially welcome. API feedback, especially from people currently using `libheif-rs`, would
help shape 0.2.

Licensed MIT OR Apache-2.0. HEVC is covered by patents; see the README.
