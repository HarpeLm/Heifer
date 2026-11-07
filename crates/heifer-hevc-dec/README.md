# heifer-hevc-dec

Pure-Rust HEVC (H.265) decoder for HEIF images and image sequences: Main, Main 10, Main Still
Picture and Range Extensions (8–16 bit, 4:0:0 to 4:4:4), tiles, WPP, PCM, scaling lists,
deblocking and SAO, and P and B frames with long-term references (`sequence::SequenceDecoder`,
in decoding or display order). No C, no `unsafe`, runs in WebAssembly.

Part of [Heifer](https://github.com/HarpeLm/Heifer). To decode HEIC photos, use the
[`heifer`](https://crates.io/crates/heifer) crate.

```rust
use heifer_hevc_dec::decoder::{DecodeOptions, decode_picture};

let frame = decode_picture(&annex_b_stream, DecodeOptions::default())?;
println!("{}×{}, {}-bit", frame.widths[0], frame.heights[0], frame.bit_depth[0]);
```

HEVC is covered by patents; you are responsible for any licensing your use requires.

Licensed under MIT or Apache-2.0, at your option.
