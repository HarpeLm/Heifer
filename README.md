# heifer

Pure-Rust HEIF/HEIC image **decoder and encoder**. No C dependencies, `#![forbid(unsafe_code)]`, WebAssembly-ready.

> **Status: early development.** The container and all HEVC headers are parsed and verified against
> ffmpeg; pixel decoding is not there yet. Not usable for real images today.

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

On the HEVC side, `heifer-hevc-dec` splits NAL units and parses the VPS, SPS, PPS and slice headers.

## Crates

| Crate | Role | State |
|---|---|---|
| `heifer` | Public API | placeholder |
| `heifer-isobmff` | HEIF container: boxes, items, properties, references, grids | ✅ reading |
| `heifer-hevc-dec` | HEVC intra decoder | 🚧 headers done, CABAC next |
| `heifer-hevc-enc` | HEVC intra encoder | ⏳ not started |

## Roadmap

- [ ] **Phase 1 — Decoder**
  - [x] Parse container: `ftyp`, `meta`, `iinf`, `iloc`, `iref`, `iprp`/`ipco`/`ipma`, `idat`, grids, rotation/mirror, alpha
  - [x] Extract HEVC bitstreams (Annex B) for each image item
  - [ ] Overlays (`iovl`), EXIF/XMP access, `iloc` construction method 2
  - [ ] HEVC intra decoding
    - [x] Bit reader, Exp-Golomb codes, NAL units, emulation prevention
    - [x] VPS, SPS (VUI, HRD, scaling lists, range extension), PPS (tiles, deblocking)
    - [x] Slice segment header (I slices, entry points, dependent slices)
    - [ ] CABAC entropy decoding
    - [ ] Coding tree, intra prediction, transforms
    - [ ] Deblocking filter, SAO
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
```

Header parsing is checked against `ffmpeg -i tile.h265 -c copy -bsf:v trace_headers -f null -`:
the `params` example prints fields with the same names as the specification and ffmpeg.

## Patents

HEVC is covered by patents. This project is a clean-room implementation for research and interoperability;
users are responsible for complying with applicable patent licensing in their jurisdiction.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
