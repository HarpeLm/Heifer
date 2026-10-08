<div align="center">

# Heifer

**A pure-Rust HEIF/HEIC image decoder.**<br>
No C dependencies · no `unsafe` code · runs everywhere Rust runs, including the browser.

[![crates.io](https://img.shields.io/crates/v/heifer.svg)](https://crates.io/crates/heifer)
[![docs.rs](https://img.shields.io/docsrs/heifer)](https://docs.rs/heifer)
[![CI](https://github.com/HarpeLm/Heifer/actions/workflows/ci.yml/badge.svg)](https://github.com/HarpeLm/Heifer/actions/workflows/ci.yml)
[![Fuzz](https://github.com/HarpeLm/Heifer/actions/workflows/fuzz.yml/badge.svg)](https://github.com/HarpeLm/Heifer/actions/workflows/fuzz.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**[▶ Try it in your browser](https://harpelm.github.io/Heifer/)** — drop a `.heic` photo; it is decoded locally by heifer compiled to WebAssembly.

</div>

---

HEIC is the default photo format on iPhones and many Android phones. In Rust, reading it has meant
binding to libheif and libde265 (C/C++). **Heifer** is an independent implementation of HEIF and HEVC
written entirely in safe Rust, validated against the official conformance bitstreams.

## Highlights

- ✅ **Correct** — identical to ffmpeg on 62/62 comparable HEVC conformance bitstreams; 41 also verified against the reference decoder's MD5 hashes.
- 📱 **Real photos** — iPhone (48-tile grids, rotation, spatial photos), Samsung 200 MP, Xiaomi.
- 🛡️ **Safe** — `#![forbid(unsafe_code)]`, size limits against malicious files, fuzzed every night.
- ⚡ **Fast enough** — grid tiles decoded in parallel: a 12 MP iPhone photo in about 100 ms.
- 🧩 **Complete** — alpha, overlays, crop/rotation/mirror, 8–12 bit, EXIF/XMP/ICC metadata.
- 🖼️ **Plays well with others** — optional [`image`](https://crates.io/crates/image) integration: `image::open("photo.heic")`.
- 🌐 **Portable** — compiles to WebAssembly (≈ 200 KB).

## Quick start

```toml
[dependencies]
heifer = "0.1"
```

```rust
let bytes = std::fs::read("photo.heic")?;
let image = heifer::decode(&bytes)?;    // primary image, with alpha, crop/rotation/mirror applied

println!("{}×{}, {}-bit, alpha: {}", image.width, image.height, image.bit_depth, image.has_alpha);
let rgba: Vec<u8> = image.to_rgba8();   // or to_rgb8(); image.data keeps 16-bit samples
```

### Metadata

Read EXIF, XMP and the ICC profile without decoding pixels:

```rust
let meta = heifer::read_metadata(&bytes)?;
if let Some(exif) = meta.exif_fields() {
    println!("{:?} {:?} taken {:?}", exif.make(), exif.model(), exif.date_time());
}
let icc: Option<Vec<u8>> = meta.icc_profile;   // also meta.exif (raw TIFF) and meta.xmp
```

### With the `image` crate

```toml
heifer = { version = "0.1", features = ["image"] }
```

```rust
heifer::image_crate::register_image_decoder_hooks();
let img = image::open("photo.heic")?;   // .heic / .heif / .hif, plus content detection
```

`HeifDecoder` implements `image::ImageDecoder` (8- and 16-bit RGB/RGBA, EXIF/XMP/ICC). The HEIF
orientation is already applied, so it is never applied twice.

### Options

```rust
let options = heifer::Options {
    max_threads: 1,            // 0 = all cores (default); grid tiles are decoded in parallel
    max_pixels: 50_000_000,    // reject larger images (default: 2^28 pixels)
};
let image = heifer::decode_with_options(&bytes, &options)?;
```

### Command line

```sh
cargo run --release -p heifer --example heic2png -- photo.heic photo.png
cargo run --release -p heifer --example metadata -- photo.heic
```

## What is supported

| | Supported | Not yet |
|---|---|---|
| **Container** | grids, overlays (`iovl`), alpha planes, `clap`/`irot`/`imir`, thumbnails, multiple images | `iloc` construction method 2 |
| **HEVC** | Main, Main 10, Main Still Picture, intra Range Extensions: 8–12 bit, 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4, tiles, WPP, PCM, scaling lists, deblocking, SAO | extended precision (16-bit), image sequences (inter prediction) |
| **Colour** | YCbCr → RGB, BT.601 / 709 / 2020, full and limited range (`colr` nclx, else HEVC VUI) | applying ICC profiles, HDR gain maps |
| **Metadata** | EXIF (with a small reader for camera, date, orientation, GPS), XMP, ICC | IPTC |
| **Encoding** | — | planned (see [roadmap](#roadmap)) |

## Quality

### HEVC conformance

The first picture of 66 official conformance bitstreams (ITU-T H.265.1) covering Main, Main 10 and
the Range Extensions:

| Check | Result |
|---|---|
| Identical to ffmpeg, sample for sample | **62 / 62** comparable streams |
| Verified against the reference decoder's picture hash (MD5 / checksum SEI) | **41** streams, including 3 that ffmpeg cannot decode |
| Not supported | 1 (16-bit extended precision tools) |

### Real-world photos

| Photo | Content | Result |
|---|---|---|
| iPhone 13 Pro | 4032×3024, 48-tile grid, HDR gain map, depth map | ≈ 61 dB vs ffmpeg |
| iPhone 8 Plus | rotated 270° | correct orientation |
| iPhone 15 Pro | spatial (stereo) photo | ≈ 62 dB |
| Samsung Galaxy S24 Ultra | 200 MP, 768 tiles | decoded (ffmpeg fails) |
| Xiaomi, Sony, others | grids, 8/10/12-bit, alpha | ✓ |
| Corrupted / truncated files | | clean errors, never a panic |

The remaining differences with ffmpeg are in heifer's favour: it honours the declared image sizes and
grid crops, and decodes the file's primary image rather than the largest one.

### Fuzzing

Five [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets with HEIF/HEVC dictionaries run
every night on GitHub Actions: full decoding, container parsing, HEVC decoding, a parallel-vs-sequential
differential check and a CABAC round trip. Fuzzing already found and fixed a memory exhaustion bug.

## Performance

Measured on a 10-core Apple Silicon Mac (release build):

| Image | Time |
|---|---|
| iPhone photo, 4032×3024 (48 tiles) | ~100 ms |
| 8000×6000 | ~550 ms |
| Samsung 200 MP (768 tiles) | ~2 s |
| In the browser (WebAssembly, single thread), 1280×854 | ~260 ms |

Single-image decoding is not optimised yet; there is room for improvement.

## Architecture

```text
heifer               decode(), read_metadata(), image crate integration
├── heifer-isobmff   HEIF container: boxes, items, properties, grids, metadata
└── heifer-hevc-dec  HEVC decoder: CABAC, intra prediction, transforms, deblocking, SAO
    heifer-hevc-enc  HEVC encoder (work in progress: CABAC encoder)
```

The lower-level crates can be used directly, for example to extract an HEVC stream:

```rust
let file = heifer_isobmff::HeifFile::parse(&bytes)?;
let tiles = file.referenced_items(file.primary_id, b"dimg");
let hevc = file.hevc_bitstream(tiles[0])?;   // Annex B, decodable by heifer-hevc-dec or ffmpeg
```

## Roadmap

- [x] **0.1 — Decoder**: container, HEVC, conformance, real photos, metadata, `image`, WebAssembly, fuzzing
- [ ] **Next**: HDR gain maps (Apple / ISO 21496-1), ICC colour management, faster single-image decoding
- [ ] **Encoder**: HEIF writing, minimal HEVC intra encoder, then better compression, 10-bit and alpha

See [CHANGELOG.md](CHANGELOG.md) for details.

## Development

<details>
<summary>Building, testing and validating</summary>

```sh
./scripts/fetch-fixtures.sh      # sample HEIC files (not committed)
./scripts/fetch-real.sh          # real-world photos from pillow-heif's test suite (not committed)
cargo test --workspace --all-features
```

**Inspecting files**

```sh
cargo run -p heifer-isobmff --example dump -- photo.heic                       # box tree
cargo run -p heifer-isobmff --example info -- photo.heic --extract 1002 t.h265 # items, HEVC stream
cargo run -p heifer-hevc-dec --example params -- t.h265                        # parameter sets
cargo run --release -p heifer-hevc-dec --example decode -- t.h265 t.yuv        # raw YUV
```

**Conformance** (≈ 23 MB from itu.int, not committed)

```sh
./scripts/fetch-conformance.sh
cargo test --release -p heifer-hevc-dec --test conformance   # reference picture hashes
cargo build --release -p heifer-hevc-dec --example decode
./scripts/conformance.py                                     # comparison with ffmpeg
```

**Fuzzing** (nightly toolchain and `cargo install cargo-fuzz`)

```sh
./scripts/fetch-fuzz-corpus.sh   # libheif's fuzzing corpus as extra seeds
./scripts/fuzz.sh 8h             # all targets on all cores; Ctrl+C to stop
```

**WebAssembly demo** (`rustup target add wasm32-unknown-unknown`, `cargo install wasm-bindgen-cli --version 0.2.129`)

```sh
./scripts/build-demo.sh && python3 -m http.server -d demo/web 8000
```

</details>

## Acknowledgements

Validation relies on the ITU-T HEVC conformance bitstreams, the test files of
[pillow-heif](https://github.com/bigcat88/pillow_heif) and [Nokia's HEIF samples](https://github.com/nokiatech/heif),
the fuzzing corpus of [libheif](https://github.com/strukturag/libheif), and [ffmpeg](https://ffmpeg.org) as a reference decoder.

## Patents

HEVC is covered by patents. Heifer is an independent implementation for research and interoperability;
users are responsible for complying with applicable patent licensing in their jurisdiction.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
