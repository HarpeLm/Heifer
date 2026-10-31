# heifer-isobmff

Pure-Rust reader and writer for the ISOBMFF container used by HEIF/HEIC images (ISO/IEC 14496-12
and ISO/IEC 23008-12): boxes, items, properties, references, grids and metadata. No C, no `unsafe`.

Part of [Heifer](https://github.com/HarpeLm/Heifer). To decode images, use the
[`heifer`](https://crates.io/crates/heifer) crate.

```rust
let bytes = std::fs::read("photo.heic")?;
let file = heifer_isobmff::HeifFile::parse(&bytes)?;
let tiles = file.referenced_items(file.primary_id, b"dimg");
let hevc = file.hevc_bitstream(tiles[0])?; // Annex B, decodable by heifer-hevc-dec or ffmpeg
```

Writing goes through `writer::HeifWriter`: HEVC image items, grids, alpha planes, EXIF, `clap`.

Licensed under MIT or Apache-2.0, at your option.
