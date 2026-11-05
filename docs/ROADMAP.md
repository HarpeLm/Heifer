# Roadmap

heifer is a pure-Rust HEIC/HEIF-HEVC decoder and encoder. The goal is not to copy every codec of
libheif: it is to be the best library for HEVC-coded HEIF files, with a small wasm build, no C
toolchain and a permissive licence.

This roadmap lists the work that closes real gaps against libheif without changing that promise.
Items are ordered by user impact.

## Status

- Latest release: 0.3.29, IPTC metadata, decoding from a reader, faster CABAC.
- Decoding: HEVC still images, grids, overlays, alpha planes, depth maps, gain maps, uncompressed
  auxiliary planes.
- Encoding: 4:2:0, 4:4:4, lossless, alpha, grids and `tmap` gain maps.
- Conformance: 66 official HEVC bitstreams, no unsupported stream.

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

### Uncompressed auxiliary images (`unci`)

Decode ISO/IEC 23001-17 uncompressed items when they are auxiliary images (depth maps, alpha
planes, gain maps). Uncompressed items stay unsupported as primary images: heifer remains an
HEVC codec, not a general uncompressed image container.

- Parse the `uncC` frame configuration.
- Support unsigned single-component frames, 1 to 16 bits, no subsampling, one tile.
- Honour component alignment, pixel size and row alignment.
- Keep a clear `Unsupported` error for layouts outside that subset.

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

## Next

### 1. Single-thread hot loops

Multi-threaded decoding is already faster than libheif, but one thread is slower than ffmpeg on
large grids. Optimise the per-tile and colour-conversion loops without adding `unsafe`.

### 2. More encoding knobs

- Explicit quality and bitrate controls.
- 4:2:2 encoding.
- Advanced HEVC parameters for users who need them.

### 3. Fixtures and fuzzing

- More real files from phones and cameras.
- Keep the fuzzing corpus growing.
- Treat every new fixture as a regression test.

## Out of scope

These codecs and features belong to other libraries. Adding them to the core would break the
pure-Rust, lightweight and HEVC-only promise.

- AVIF / AV1: a separate crate could reuse the container, not the core codec.
- VVC / H.266, AVC / H.264: no mature pure-Rust implementation.
- JPEG and JPEG 2000 inside HEIF.
- Image sequences and inter-prediction.
- Uncompressed images as primary images.

The target is a credible HEIC/HEVC alternative to libheif, not a libheif clone.
