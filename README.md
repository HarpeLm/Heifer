# heifer

Pure-Rust HEIF/HEIC image **decoder and encoder**. No C dependencies, `#![forbid(unsafe_code)]`, WebAssembly-ready.

> **Status: early development.** HEVC image items decode **bit-exactly against ffmpeg**, in-loop filters
> included. Grid assembly and RGB output are next.

## Why

HEIC is the default photo format on iPhones. In Rust, reading it usually means binding to `libheif` (C/C++),
and there is no pure-Rust encoder at all. heifer aims to be a safe, portable, well-tested alternative.

## What works today

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
| `heifer` | Public API | placeholder |
| `heifer-isobmff` | HEIF container: boxes, items, properties, references, grids | ✅ reading |
| `heifer-hevc-dec` | HEVC intra decoder | ✅ bit-exact vs ffmpeg (8-bit 4:2:0 tested) |
| `heifer-hevc-enc` | HEVC intra encoder | 🚧 CABAC encoder only |

## Roadmap

- [ ] **Phase 1 — Decoder**
  - [x] Parse container: `ftyp`, `meta`, `iinf`, `iloc`, `iref`, `iprp`/`ipco`/`ipma`, `idat`, grids, rotation/mirror, alpha
  - [x] Extract HEVC bitstreams (Annex B) for each image item
  - [ ] Overlays (`iovl`), EXIF/XMP access, `iloc` construction method 2
  - [ ] HEVC intra decoding
    - [x] Bit reader, Exp-Golomb codes, NAL units, emulation prevention
    - [x] VPS, SPS (VUI, HRD, scaling lists, range extension), PPS (tiles, deblocking)
    - [x] Slice segment header (I slices, entry points, dependent slices)
    - [x] CABAC entropy decoding (+ encoder, verified by round-trip)
    - [x] Coding tree syntax: SAO, quadtree, intra modes, transform tree, residuals, QP, PCM, tiles, WPP
    - [x] Intra prediction, scaling, inverse DCT/DST — bit-exact vs ffmpeg (`-skip_loop_filter all`)
    - [x] Deblocking filter, SAO — bit-exact vs ffmpeg
    - [ ] 10-bit, 4:0:0 (alpha), 4:2:2 / 4:4:4
  - [ ] Real-world test corpus + pixel comparison against libheif
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
```

Decoding is checked against `ffmpeg -i tile.h265 -f rawvideo -pix_fmt yuv420p ref.yuv`
(and with `-skip_loop_filter all` / `--no-filters` to check reconstruction alone).

Header parsing is checked against `ffmpeg -i tile.h265 -c copy -bsf:v trace_headers -f null -`:
the `params` example prints fields with the same names as the specification and ffmpeg.

## Patents

HEVC is covered by patents. This project is a clean-room implementation for research and interoperability;
users are responsible for complying with applicable patent licensing in their jurisdiction.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
