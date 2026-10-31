# heifer-hevc-enc

Pure-Rust HEVC (H.265) intra encoder for still images: Main profile, 8-bit 4:2:0 or monochrome,
blocks of 4×4 to 32×32, prediction modes and transform splits chosen by rate-distortion cost, RDOQ,
sign data hiding. At equal quality, its files are slightly smaller than x265's `medium` preset. No C, no `unsafe`, runs in
WebAssembly. The pictures are reconstructed with the code of
[`heifer-hevc-dec`](https://crates.io/crates/heifer-hevc-dec), so the decoder reproduces them
exactly.

Part of [Heifer](https://github.com/HarpeLm/Heifer). To write HEIC files, use the
[`heifer`](https://crates.io/crates/heifer) crate.

```rust
use heifer_hevc_enc::encoder::{EncoderConfig, Planes, encode_picture};

let planes = Planes { y: &luma, chroma: Some((&cb, &cr)) };
let picture = encode_picture(width, height, planes, &EncoderConfig { qp: 24, ..Default::default() })?;
std::fs::write("picture.h265", picture.annex_b())?; // decodable by ffmpeg
```

HEVC is covered by patents; you are responsible for any licensing your use requires.

Licensed under MIT or Apache-2.0, at your option.
