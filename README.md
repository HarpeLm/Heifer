# heifer

Pure-Rust HEIF/HEIC image **decoder and encoder**. No C dependencies, `#![forbid(unsafe_code)]`, WebAssembly-ready.

> Status: early development — nothing works yet.

## Why

HEIC is the default photo format on iPhones. In Rust, reading it usually means binding to `libheif` (C/C++),
and there is no pure-Rust encoder at all. heifer aims to be a safe, portable, well-tested alternative.

## Crates

| Crate | Role |
|---|---|
| `heifer` | Public API |
| `heifer-isobmff` | HEIF container (ISOBMFF boxes, items, tiles, EXIF) |
| `heifer-hevc-dec` | HEVC intra decoder |
| `heifer-hevc-enc` | HEVC intra encoder |

## Roadmap

- [ ] **Phase 1 — Decoder**
  - [x] Parse container: `ftyp`, `meta`, `iinf`, `iloc`, `iref`, `iprp`/`ipco`/`ipma`, `idat`, grids, rotation/mirror, alpha
  - [x] Extract HEVC bitstreams (Annex B) for each image item
  - [ ] Overlays (`iovl`), EXIF/XMP access, `iloc` construction method 2
  - [ ] HEVC intra decoding (8-bit 4:2:0, then 10-bit, 4:0:0 alpha)
  - [ ] Real-world test corpus + pixel comparison against libheif
  - [ ] Fuzzing, WebAssembly, `image` crate integration
- [ ] **Phase 2 — Encoder**
  - [ ] Write HEIF container
  - [ ] Minimal HEVC intra encoder (fixed block size, DC prediction, fixed QP) producing valid files
  - [ ] All 35 intra modes, block-size decisions, rate-distortion optimisation
  - [ ] 10-bit, alpha, HDR

## Patents

HEVC is covered by patents. This project is a clean-room implementation for research and interoperability;
users are responsible for complying with applicable patent licensing in their jurisdiction.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
