# Roadmap

heifer is a pure-Rust HEIC/HEIF-HEVC decoder and encoder. The goal is not to copy every codec of
libheif: it is to be the best library for HEVC-coded HEIF files, with a small wasm build, no C
toolchain and a permissive licence.

This roadmap lists the work that closes real gaps against libheif without changing that promise.
Items are ordered by user impact.

## Status

- Latest release: 0.9.0 — bounded-memory tile-by-tile preview decoding (`PreviewOptions`,
  `decode_preview_rgba8`, `decode_reader_preview_rgba8`), `decode_reader_srgb_rgba8` +
  `Options::fallback_to_coded_color`, ISO 21496-1 `tmap` + Apple `hdrgainmap` extraction in
  `read_gain_map`, still-image `EncodeSpeed::Fast` (~2.2× faster end-to-end on 12 MP: `371 ms` at
  q=75, `456 ms` at q=85), and safe-Rust encoder/decoder hot-loop speedups (`12 MP` q=82 `Medium`
  `823 ms` vs `903 ms` in `0.8.0`).
- Decoding: HEVC still images, grids, overlays, alpha planes, depth maps, gain maps, uncompressed
  images (`unci`), image sequences (114 conformance bitstreams match their MD5).
- Encoding: 4:2:0, 4:2:2, 4:4:4, lossless, alpha, grids, `tmap` gain maps, and `.heics` image
  sequences (`encode_sequence`, `encode_sequence_with_format`, `encode_sequence_to_size_with_format`).
- Conformance: 66 official still HEVC bitstreams + 114 sequence bitstreams (7115 pictures), no
  unsupported stream.

## Done

### Auxiliary images in the CLI

The CLI extracts the auxiliary images that the library already lists and decodes.

- `--aux` writes every auxiliary image next to the output, as `photo.depth.png`,
  `photo.gain-map.png`, `photo.alpha.png` or `photo.aux-<id>.png`.
- `--depth` and `--gain-map` select one type.
- Richer `info`: auxiliary images with their URN, size and bit depth, depth metadata, gain map
  metadata and IPTC presence.

### Encoding options in the CLI

The CLI now codes HEIC in 4:2:0, 4:4:4 or lossless, with the default unchanged.

- `--format 420|444|lossless`.
- `--lossless` as a shortcut.

### Uncompressed images (`unci`)

Decode ISO/IEC 23001-17 uncompressed items, as primary images and as auxiliary images (depth
maps, alpha planes, gain maps). This is the one deliberate exception to heifer being an HEVC
codec: AVIF, VVC, AVC, JPEG and JPEG 2000 stay out of scope.

- Parse the `uncC` frame configuration and the `cmpd` component descriptions.
- Support one to four unsigned components of 1 to 16 bits, the component, pixel, row, mixed and
  tile-component interleave modes, tiles, 4:2:2 / 4:2:0 / 4:1:1 subsampling, and the alignment
  fields.
- Refuse blocks, floating-point or signed samples, little-endian samples and generic compression
  (`cmpC`) with a clear `Unsupported` error.

### IPTC metadata (#4)

- Bounded IPTC-IIM parser: `read_iptc`, `item_iptc` and `Iptc`, from an `iptc` item or EXIF.
- Exposed as separate functions rather than a `Metadata` field, so that code building
  `Metadata` keeps compiling. Writing IPTC is not supported yet.

### Decoding from a reader (#5)

- `decode_reader`, `decode_reader_with_options` and `read_metadata_reader` over
  `Read + Seek`, reading only the boxes and item data a call needs.
- The slice API is unchanged; the decode fuzzer checks that both paths agree.

### Region annotations (`rgan`)

- `read_regions` / `item_regions` with every geometry and the `udes` descriptions, and
  `HeifWriter::add_region_item` to write them.

### Image sequences and encoding presets (`0.5`–`0.7.0`)

- Decoding: P and B frames (DPB, merge/AMVP, TMVP, bi-prediction, weighted prediction, long-term
  references, explicit RDPCM), display order (`ctts`), auxiliary alpha sequence tracks (`auxl`);
  114 conformance streams (7115 pictures) match their MD5.
- Encoding: `.heics` sequences (`encode_sequence`, `encode_sequence_with_format`,
  `encode_sequence_to_size_with_format`), hierarchical B-frames, TMVP, per-slice QP offsets,
  `ctts` v0 + `edts`/`elst` edit lists (`PTS >= DTS`), alpha auxiliary tracks (`auxi`/`auxl`),
  4:2:2 / 4:4:4 / lossless & 10-bit formats, `EncodeSpeed` (`Fast`/`Medium`/`Slow`), and full
  CLI support (`--speed`, `--keyframe-interval`, `--b-frames`, `--fps`, `frames/*.png out.heics`).
- Size targets (`encode_to_size`, `--max-size`) and 4:2:2 encoding for stills and sequences.
- Fuzz targets covering container and HEVC sequence decoding, B-frame reordering, TMVP and
  sequence round trips.

### Header-only info, lazy `image` decoder, and pre-1.0 hardening

- `read_info` / `item_info` / `read_info_reader` + `decode_reader_rgba8` (`FileKind`,
  `ImageInfo`), reading post-transform dimensions, bit depth, alpha presence and container kind
  without decoding pixel payloads.
- Lazy `HeifDecoder` and seekable `HookDecoder`: header-only construction, `image::Limits`
  enforced before decoding, and 8-bit `decode_rgba8` fast path.
- Pre-1.0 API hardening: `#[non_exhaustive]` on `Error`, `ReadError`, `Options`, `EncodeOptions`,
  `Metadata` (with builders `new()` / `with_*`) and `DepthRepresentation`; differential fuzz targets
  `info` and `image_decoder`.

### Bounded previews, fast still encoding, and ISO 21496-1 gain map reader (`0.9.0`)

- Bounded-memory tile-by-tile preview decoding (`PreviewOptions`, `decode_preview_rgba8`,
  `decode_reader_preview_rgba8`) for large grids (48–200 MP), servers, and WASM.
- `decode_reader_srgb_rgba8` and `Options::with_fallback_to_coded_color(bool)`.
- `read_gain_map` / `read_gain_map_with_options` extended to ISO 21496-1 (`tmap`) in addition to
  Apple (`urn:com:apple:photo:2020:aux:hdrgainmap`), with crate-root re-exports.
- Still-image `EncodeSpeed::Fast` / `Slow` wired up in `heifer-hevc-enc` (`12 MP` q=75 `371 ms`
  vs `806 ms` in `Medium`) plus safe-Rust encoder/decoder hot-loop optimizations.

## Next

### 1. Preparing `heifer 1.0.0`

- Add `read_gain_map_reader` (streaming `Read + Seek` HDR gain map extraction, consistent with
  `read_metadata_reader` and `decode_reader_preview_rgba8`).
- Full public API audit (`#[non_exhaustive]`, builders, re-exports, error types, naming
  consistency) before locking SemVer compatibility for `1.0.0`.
- Complete `docs.rs` documentation pass, verifying all rustdoc links (`-D warnings`), examples,
  and pre-1.0 invariants across all feature combinations (`default`, `--no-default-features`,
  `--all-features`).

### 2. Single-thread hot loops

Multi-threaded decoding is already faster than libheif. Safe-Rust optimization passes across CABAC,
residual parsing, transforms/intra prediction, lazy motion allocation, deblocking/SAO, inter
interpolation, buffer reuse, direct `Frame8` planes, zero-copy `CropWindow` decoding, and fused
`clap`/`irot`/`imir` grid tile transforms cut both single-thread and multi-core decode times
(`0.8.0` on Apple M5: 12 MP Apple photo `83 ms` `heifer8` vs `108 ms` for `heic-rs` — `1.30×`
faster, `14 ms` on 10 cores; 13-file suite `582` vs `738 ms`, `127 ms` on 10 cores; 5 MP Xiaomi
`64 ms` / `13 ms`; 48 MP `622 ms` / `106 ms`; 200 MP Galaxy `885 ms` / `185 ms`; 114 conformance
streams / 7115 pictures bit-exact). Remaining single-thread hotspots (such as `residual_coding`)
remain targets for further gains without `unsafe`.

### 3. More encoding knobs

- ~~Size targets~~: done (`encode_to_size`, `--max-size`).
- ~~4:2:2 encoding~~: done.
- ~~Speed presets~~: done (`EncodeSpeed`, `--speed`).
- Advanced HEVC parameters for users who need them.

### 4. Fixtures and fuzzing

- More real files from phones and cameras.
- Keep the fuzzing corpus growing.
- Treat every new fixture as a regression test.

## Out of scope

These codecs and features belong to other libraries. Adding them to the core would break the
pure-Rust, lightweight and HEVC-only promise.

- AVIF / AV1: a separate crate could reuse the container, not the core codec.
- VVC / H.266, AVC / H.264: no mature pure-Rust implementation.
- JPEG and JPEG 2000 inside HEIF.

The target is a credible HEIC/HEVC alternative to libheif, not a libheif clone.
