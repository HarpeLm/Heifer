# heifer

Pure-Rust HEIF/HEIC image **decoder** (encoder planned). No C dependencies, `#![forbid(unsafe_code)]`.

> **Status: early development.** Complete HEIC files decode to RGB(A), with grids, overlays, alpha and
> transforms. The HEVC decoder is validated against the official conformance bitstreams (see below).
> Real iPhone photos (large grids, HDR) are not validated yet.

## Conformance

The first picture of 66 official HEVC conformance bitstreams (ITU-T H.265.1: Main, Main 10 and Range
Extensions, 8 to 12 bits, 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4, tiles, WPP, PCM, scaling lists, slices...):

| Check | Result |
|---|---|
| Identical to ffmpeg, sample for sample | **62 / 62** comparable streams |
| Verified against the reference decoder's picture hash (MD5 / checksum SEI) | **41** streams, including 3 that ffmpeg cannot decode |
| Not supported | 1 (extended precision / CABAC bypass alignment, 16-bit video tools) |

## Why

HEIC is the default photo format on iPhones. In Rust, reading it usually means binding to `libheif` (C/C++),
and there is no pure-Rust encoder at all. heifer aims to be a safe, portable, well-tested alternative.

## Usage

```rust
let bytes = std::fs::read("photo.heic")?;
let image = heifer::decode(&bytes)?;           // primary image, alpha, crop/rotation/mirror applied
let rgba: Vec<u8> = image.to_rgba8();           // or to_rgb8(); image.data holds 16-bit samples
println!("{}x{}, alpha: {}", image.width, image.height, image.has_alpha);

// Grid tiles are decoded in parallel on all cores (no dependency: std threads).
// Limit or disable it with options:
let image = heifer::decode_with_options(&bytes, &heifer::Options { max_threads: 1 })?;
```

Convert a file from the command line:

```sh
cargo run --release -p heifer --example heic2png -- photo.heic photo.png [--threads N]
```

### Lower-level APIs

```rust
use heifer_isobmff::HeifFile;

let bytes = std::fs::read("photo.heic")?;
let file = HeifFile::parse(&bytes)?;
let primary = file.primary_item()?;              // e.g. a `grid` of `hvc1` tiles
let size = file.image_size(primary.id)?;         // Some((4032, 3024))
let tiles = file.referenced_items(primary.id, b"dimg");
let hevc = file.hevc_bitstream(tiles[0])?;       // Annex B stream, decodable by ffmpeg
```

Decoding one HEVC image item to pixels:

```rust
use heifer_hevc_dec::decoder::{decode_picture, DecodeOptions};

let frame = decode_picture(&hevc, DecodeOptions::default())?;
let (y, cb, cr) = (&frame.planes[0], &frame.planes[1], &frame.planes[2]);   // u16 samples
```

## Crates

| Crate | Role | State |
|---|---|---|
| `heifer` | Public API: `decode()`, grids, overlays, alpha, transforms, YCbCr → RGB | ✅ |
| `heifer-isobmff` | HEIF container: boxes, items, properties, references, grids | ✅ reading |
| `heifer-hevc-dec` | HEVC intra decoder, picture hash verification | ✅ conformance-tested (8–12 bit, all chroma formats) |
| `heifer-hevc-enc` | HEVC intra encoder | 🚧 CABAC encoder only |

## Roadmap

- [ ] **Phase 1 — Decoder**
  - [x] Parse container: `ftyp`, `meta`, `iinf`, `iloc`, `iref`, `iprp`/`ipco`/`ipma`, `idat`, grids, rotation/mirror, alpha
  - [x] Extract HEVC bitstreams (Annex B) for each image item
  - [x] Grid assembly (tiles decoded in parallel), overlays (`iovl`), alpha planes, `clap`/`irot`/`imir`
  - [x] YCbCr → RGB (BT.601/709/2020, full/limited range; `colr` nclx, else HEVC VUI)
  - [ ] EXIF/XMP access, ICC profiles, `iloc` construction method 2, bilinear chroma upsampling
  - [x] HEVC intra decoding
    - [x] Bit reader, Exp-Golomb codes, NAL units, emulation prevention
    - [x] VPS, SPS (VUI, HRD, scaling lists, range extension), PPS (tiles, deblocking)
    - [x] Slice segment header (I slices, entry points, dependent slices)
    - [x] CABAC entropy decoding (+ encoder, verified by round-trip)
    - [x] Coding tree syntax: SAO, quadtree, intra modes, transform tree, residuals, QP, PCM, tiles, WPP
    - [x] Intra prediction, scaling, inverse DCT/DST — bit-exact vs ffmpeg (`-skip_loop_filter all`)
    - [x] Deblocking filter, SAO — bit-exact vs ffmpeg
    - [x] 10/12-bit, 4:0:0, 4:2:2, 4:4:4, unequal luma/chroma bit depths
    - [x] Range extension tools: cross-component prediction, implicit RDPCM, transform-skip rotation/contexts, persistent Rice adaptation
    - [x] Decoded picture hash SEI (MD5, CRC, checksum) verification
    - [x] Validated on official HEVC conformance bitstreams
    - [ ] Extended precision processing, CABAC bypass alignment (16-bit profiles)
  - [ ] Real-world HEIC corpus (iPhone/Android photos, HDR) + comparison against libheif
  - [ ] Faster single-image decoding (allocation-free reconstruction, parallel filters, WPP/tiles)
  - [ ] Fuzzing, WebAssembly, `image` crate integration
- [ ] **Phase 2 — Encoder**
  - [ ] Write HEIF container
  - [ ] Minimal HEVC intra encoder (fixed block size, DC prediction, fixed QP) producing valid files
  - [ ] All 35 intra modes, block-size decisions, rate-distortion optimisation
  - [ ] 10-bit, alpha, HDR

## Development

```sh
./scripts/fetch-fixtures.sh   # downloads sample HEIC files into tests/fixtures/ (not committed)
cargo test --workspace

# Inspect a file
cargo run -p heifer-isobmff --example dump -- tests/fixtures/grid.heic     # box tree
cargo run -p heifer-isobmff --example info -- tests/fixtures/grid.heic     # items and properties
cargo run -p heifer-isobmff --example info -- tests/fixtures/grid.heic --extract 1002 tile.h265
cargo run -p heifer-hevc-dec --example params -- tile.h265                 # parameter sets, slice headers
cargo run -p heifer-hevc-dec --example parse -- tile.h265                  # full syntax parse statistics
cargo run --release -p heifer-hevc-dec --example decode -- tile.h265 tile.yuv   # raw planar YUV
cargo run --release -p heifer --example heic2png -- tests/fixtures/grid.heic grid.png
```

Conformance bitstreams (not committed, ~23 MB download from itu.int):

```sh
./scripts/fetch-conformance.sh
cargo test --release -p heifer-hevc-dec --test conformance      # checks reference picture hashes
cargo build --release -p heifer-hevc-dec --example decode
./scripts/conformance.py                                        # compares with ffmpeg
```

Decoding is checked against `ffmpeg -i tile.h265 -f rawvideo -pix_fmt yuv420p ref.yuv`
(and with `-skip_loop_filter all` / `--no-filters` to check reconstruction alone).

RGB output is compared with `ffmpeg -i file.heic -pix_fmt rgb24 ref.png` (≈ 53 dB PSNR; the remaining
difference comes from chroma upsampling and rounding).

Header parsing is checked against `ffmpeg -i tile.h265 -c copy -bsf:v trace_headers -f null -`:
the `params` example prints fields with the same names as the specification and ffmpeg.

## Patents

HEVC is covered by patents. This project is a clean-room implementation for research and interoperability;
users are responsible for complying with applicable patent licensing in their jurisdiction.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
