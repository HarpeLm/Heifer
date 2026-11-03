# Migrating from `libheif-rs` to `heifer`

This guide is for crates and applications that decode or encode HEIC/HEIF through
[`libheif-rs`](https://crates.io/crates/libheif-rs) (a safe wrapper around the C/C++
`libheif` and `libde265`) and want to switch to `heifer`, a pure-Rust implementation.

You usually switch for one of these reasons:

- **Licence**: `libheif` is LGPL-3. `heifer` is `MIT OR Apache-2.0`.
- **No C toolchain**: `heifer` has no build script, no `pkg-config`, no system library.
  It cross-compiles and builds for `wasm32-unknown-unknown` and `musl` out of the box.
- **Memory safety**: `heifer` uses `#![forbid(unsafe_code)]`.
- **Correctness**: bit-exact with `ffmpeg` on 62/62 comparable HEVC conformance streams.

The two APIs are shaped differently, so the switch is a rewrite of the call sites, not a
one-line rename. This guide keeps the change mechanical.

> `heifer` targets **HEIC/HEIF with HEVC**. It is not a drop-in for `libheif` pipelines
> that rely on AVIF, VVC, AVC, JPEG/J2K, image sequences, region annotations, runtime
> plugins or a streaming reader. See [What you give up](#what-you-give-up) before you start.

## Contents

- [Cargo.toml](#cargotoml)
- [Decode a file](#decode-a-file)
- [Check the file type](#check-the-file-type)
- [Multiple images](#multiple-images)
- [Metadata: EXIF, XMP, ICC](#metadata-exif-xmp-icc)
- [Encode a file](#encode-a-file)
- [The `image` crate hooks](#the-image-crate-hooks)
- [HDR gain maps](#hdr-gain-maps)
- [Error handling](#error-handling)
- [API map](#api-map)
- [Behavioural differences](#behavioural-differences)
- [What you give up](#what-you-give-up)
- [Migration checklist](#migration-checklist)

## Cargo.toml

```toml
# before
[dependencies]
libheif-rs = "3"
libheif-sys = "1"          # only if referenced directly

# after
[dependencies]
heifer = "0.3"
```

`heifer` needs Rust **1.85** (edition 2024) and has no non-Rust dependencies. To confirm
the C dependency really is gone:

```sh
cargo tree | grep -i heif          # only `heifer` and its crates
cargo build --target wasm32-unknown-unknown   # builds without a C toolchain
```

## Decode a file

`libheif-rs` opens a context, gets an image handle, then decodes that handle into a
`ColorSpace`. `heifer` decodes the primary image in one call.

```rust
// before — libheif-rs
use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};

let lib_heif = LibHeif::new();
let ctx = HeifContext::read_from_file("./photo.heic")?;
let handle = ctx.primary_image_handle()?;
let image = lib_heif.decode(&handle, ColorSpace::Rgb(RgbChroma::Rgba), None)?;

let planes = image.planes();
let plane = planes.interleaved.unwrap();   // RGBA bytes, `stride` per row
```

```rust
// after — heifer
let bytes = std::fs::read("./photo.heic")?;
let image = heifer::decode(&bytes)?;       // interleaved RGBA, u16 samples

let rgba: Vec<u8> = image.to_rgba8();      // or image.to_rgb8()
println!("{}x{} {}bpp alpha={}", image.width, image.height, image.bit_depth, image.has_alpha);
```

`heifer::Image` is **interleaved RGBA**, always four channels, so there is no plane
juggling and no `stride` to track: the samples are contiguous, `4` per pixel, row-major.
`Image::data` holds `u16` samples at `Image::bit_depth` bits; `to_rgba8`/`to_rgb8` narrow
to 8-bit `u8`.

Decode straight to 8-bit (half the memory) with `decode_rgba8`:

```rust
let image: heifer::Image<u8> = heifer::decode_rgba8(&bytes, &heifer::Options::default())?;
let rgba: &[u8] = &image.data;              // ready to upload / save
```

## Check the file type

`libheif-rs` exposes `check_file_type(&bytes)` to validate a file before decoding it.
`heifer::check_file_type` returns the same four variants, with `Supported` meaning an HEVC
HEIF file that `heifer` can decode.

```rust
// before — libheif-rs
use libheif_rs::{check_file_type, FileTypeResult};

// after — heifer
use heifer::{check_file_type, FileTypeResult};

let mut file = std::fs::File::open("./photo.heic")?;
let mut head = [0; 12];
std::io::Read::read_exact(&mut file, &mut head)?;

match check_file_type(&head) {
    FileTypeResult::Supported => { /* decode it */ }
    FileTypeResult::No => { /* not a HEIF file */ }
    FileTypeResult::Unsupported | FileTypeResult::MayBe => { /* unsupported or unknown */ }
}
```

Only the first 12 bytes are needed (box size, `ftyp`, major brand); any more are ignored. As
in `libheif`, only the major brand is read: `heic`/`heix` are `Supported`, a generic
`mif1`/`mif2` is `MayBe`, and AVIF, JPEG, JPEG 2000 and image sequences are `Unsupported`
because `heifer` does not decode them.

## Multiple images

Some files hold several top-level pictures: bursts, "collections", and Apple dynamic HEIF
(a still plus its variants). `libheif-rs` enumerates them with `top_level_image_ids` and
decodes each handle. `heifer` does the same in one call, skipping each picture's
thumbnails, grid tiles and auxiliary planes:

```rust
// before — libheif-rs
let ids = ctx.top_level_image_ids()?;
for id in ids {
    let handle = ctx.image_handle(&id)?;
    let image = lib_heif.decode(&handle, ColorSpace::Rgb(RgbChroma::Rgba), None)?;
    // ...
}
```

```rust
// after — heifer
let images: Vec<heifer::Image> = heifer::decode_all(&bytes)?;   // one per picture
```

Most files hold a single image, so `decode_all` returns one image, identical to `decode`.
For 8-bit sRGB output, use `decode_all_srgb_rgba8(&bytes, &Options::default())`. If you need
to inspect the item list first, the container exposes it directly:

```rust
let file = heifer_isobmff::HeifFile::parse(&bytes)?;
for id in file.top_level_image_ids() {
    let image = heifer::decode_item(&file, id, 0)?;
}
```

## Metadata: EXIF, XMP, ICC

`libheif-rs` walks metadata blocks by type on the handle. `heifer` reads all three kinds
at once, without decoding pixels.

```rust
// before — libheif-rs
let handle = ctx.primary_image_handle()?;
let mut ids = vec![0; 1];
handle.metadata_block_ids(&mut ids, b"Exif");
let exif: Vec<u8> = handle.metadata(ids[0])?;
```

```rust
// after — heifer
let meta = heifer::read_metadata(&bytes)?;
let exif: Option<Vec<u8>> = meta.exif;         // raw TIFF
let xmp:  Option<Vec<u8>> = meta.xmp;          // XML packet
let icc:  Option<Vec<u8>> = meta.icc_profile;  // embedded ICC profile
```

`heifer::read_metadata` never decodes the picture, so it is cheap. There is also a tiny
bounds-checked EXIF reader for common tags:

```rust
if let Some(e) = meta.exif_fields() {
    // e.g. e.orientation(), camera make/model, date...
}
```

## Encode a file

`libheif-rs` builds an `Image` plane by plane, picks an encoder, then `encode_image`.
`heifer` takes the pixels directly.

```rust
// before — libheif-rs
use libheif_rs::{
    Channel, ColorSpace, CompressionFormat, EncoderQuality, HeifContext, Image, RgbChroma,
};

let mut image = Image::new(w, h, ColorSpace::Rgb(RgbChroma::C444))?;
image.create_plane(Channel::R, w, h, 8)?;
image.create_plane(Channel::G, w, h, 8)?;
image.create_plane(Channel::B, w, h, 8)?;
// ... fill planes.r/g/b ...

let mut ctx = HeifContext::new()?;
let mut encoder = ctx.encoder_for_format(CompressionFormat::Hevc)?;
encoder.set_quality(EncoderQuality::Lossy(80))?;
ctx.encode_image(&mut image, &mut encoder, None)?;
let bytes = ctx.write_to_bytes()?;
```

```rust
// after — heifer, from 8-bit RGB(A) slices
let bytes = heifer::encode_rgb8(w, h, &rgb, &heifer::EncodeOptions::default())?;
// or: heifer::encode_rgba8(w, h, &rgba, &options)?

// or from a decoded / built `heifer::Image` (keeps 10-bit when bit_depth > 8)
let bytes = heifer::encode(&image, &heifer::EncodeOptions { quality: 80, ..Default::default() })?;
```

`EncodeOptions` is `{ quality: u8 /* 0..=100, default 80 */, max_threads: usize }`. Keeping
the original metadata is one more call:

```rust
let (image, meta) = (heifer::decode(&bytes)?, heifer::read_metadata(&bytes)?);
let out = heifer::encode_with_metadata(&image, &meta, &heifer::EncodeOptions::default())?;
```

`libheif-rs` chooses 4:4:4 with `RgbChroma::C444` and lossless coding with
`EncoderQuality::Lossless`; in `heifer` both are an `EncodeFormat`:

```rust
// Full-resolution chroma (HEVC Range Extensions profile 4).
let sharp = heifer::encode_rgb8_with_format(
    w, h, &rgb, heifer::EncodeFormat::Ycbcr444, &options)?;
// Exact RGB, coded as GBR 4:4:4 without transform or quantization.
let exact = heifer::encode_rgb8_with_format(
    w, h, &rgb, heifer::EncodeFormat::Lossless, &options)?;
```

The `_with_format` variants of `encode`, `encode_with_metadata` and `encode_rgba8` take the same
argument. The original functions are unchanged and write 4:2:0.

The pixels of a decoded image are already upright, so `encode_with_metadata` rewrites the
EXIF orientation tag to `1`; no viewer will rotate the result twice.

## The `image` crate hooks

`libheif-rs` registers decoding hooks through `libheif_rs::integration::image`. `heifer`
mirrors that so `image::open` / `ImageReader` can read `.heic`:

```rust
// before
use libheif_rs::integration::image::register_all_decoding_hooks;
register_all_decoding_hooks();
```

```rust
// after
heifer::image_crate::register_image_decoder_hooks();     // returns bool
// with options: register_image_decoder_hooks_with_options(heifer::Options::default())
```

If you use `libheif-rs`'s `ImageEncoder`, the equivalent is
`heifer::image_crate::HeifEncoder`, which implements `image::ImageEncoder`:

```rust
use heifer::image_crate::HeifEncoder;
use image::ImageEncoder;
```

## HDR gain maps

`libheif` reads gain maps through its auxiliary/metadata machinery. `heifer` exposes them
directly:

```rust
let hdr: heifer::HdrImage = heifer::decode_hdr(&bytes)?;   // f32 linear light + headroom
// or inspect the map without applying it:
let map: Option<heifer::GainMap> = heifer::read_gain_map(&bytes)?;
```

Re-encoding an HDR photo as a HEIC with an ISO 21496-1 `tmap` gain map:

```rust
let out = heifer::encode_hdr(&base, &hdr, &heifer::EncodeOptions::default())?;
```

## Auxiliary images (depth maps, alpha planes)

`libheif` exposes auxiliary images through `HeifImageHandle::auxiliary_images` and
`get_aux_image`. `heifer` lists them and decodes one by ID or by type:

```rust
for aux in heifer::auxiliary_images(&bytes)? {
    // id, item type, auxC URN, width, height, bit depth
    println!("{}: {}x{} ({} bits)", aux.urn, aux.width, aux.height, aux.bit_depth);
    if aux.is_depth() {
        let map = heifer::decode_auxiliary(&bytes, aux.id)?;
    }
}

// Or directly by type, None when the file has no such auxiliary image:
let depth = heifer::decode_auxiliary_urn(&bytes, heifer::DEPTH_URN)?;
```

Each auxiliary image is decoded with its own `clap`/`irot`/`imir` properties; the transforms of
the primary image are not applied on top. `item_auxiliary_images` does the same for an image
other than the primary, and `decode_all` still returns only top-level images.

## Error handling

`libheif-rs` returns `HeifError` (an error code + sub-code from C). `heifer` returns a
typed [`heifer::Error`](https://docs.rs/heifer/latest/heifer/enum.Error.html):

| Variant | Meaning |
|---|---|
| `Error::Container` | the HEIF container is invalid |
| `Error::Hevc` | the HEVC bitstream could not be decoded |
| `Error::Encode` | HEVC encoding failed |
| `Error::Unsupported(String)` | a feature `heifer` does not implement yet (e.g. an ICC profile whose input space is not RGB, or PQ/HLG through `decode_srgb`) |
| `Error::Invalid(&'static str)` | the file is internally inconsistent |

```rust
match heifer::decode(&bytes) {
    Ok(image) => { /* ... */ }
    Err(heifer::Error::Unsupported(feature)) => { /* fall back, or report */ }
    Err(e) => return Err(e.into()),
}
```

## API map

| `libheif-rs` | `heifer` |
|---|---|
| `LibHeif::new()` | — (no global state; nothing to initialise) |
| `HeifContext::read_from_file(path)` | `std::fs::read(path)` → `heifer::decode(&bytes)` |
| `HeifContext::read_from_bytes(bytes)` | `heifer::decode(bytes)` |
| `ctx.primary_image_handle()` | — (`decode` targets the primary image) |
| `ctx.image_handle(id)` | `HeifFile::parse` → `heifer::decode_item(...)` (low-level) |
| `ctx.top_level_image_ids()` / `number_of_top_level_images()` | `heifer::decode_all(&bytes)` (`HeifFile::top_level_image_ids()` for the IDs) |
| `lib_heif.decode(&handle, ColorSpace::Rgb(_), None)` | `heifer::decode(&bytes)` |
| `check_file_type(&bytes)` / `FileTypeResult` | `heifer::check_file_type(&bytes)` / `heifer::FileTypeResult` |
| `image.planes()` / `.interleaved` / `stride` | `image.data` (interleaved RGBA), `to_rgba8()`, `to_rgb8()` |
| `handle.metadata_block_ids(b"Exif")` / `handle.metadata(id)` | `heifer::read_metadata(&bytes)` → `.exif` / `.xmp` / `.icc_profile` |
| `image.scale(w, h, None)` | — (resize with the `image` crate) |
| `HeifContext::new()` + `encode_image(...)` | `heifer::encode(&image, &EncodeOptions)`, `encode_rgb8`, `encode_rgba8` |
| `RgbChroma::C444` / `EncoderQuality::Lossless` | `EncodeFormat::Ycbcr444` / `EncodeFormat::Lossless` with the `_with_format` functions |
| `EncodingOptions` / `EncoderQuality` | `EncodeOptions { quality, max_threads }` |
| `DecodingOptions::set_max_decoding_threads` | `Options { max_threads, max_pixels }` |
| `integration::image::register_all_decoding_hooks()` | `heifer::image_crate::register_image_decoder_hooks()` |
| `Image` (planar, `Channel`s) | `heifer::Image` (interleaved RGBA, `u16`/`u8`) |

## Behavioural differences

- **Transformations are always applied.** `heifer::decode` applies the crop, rotation and
  mirror stored in the file, and there is no "ignore transformations" switch (libheif's
  `DecodingOptions::set_ignore_transformations`). If you need the untransformed pixels,
  decode the item directly with the low-level API.
- **Colour space is explicit.** `heifer::decode` returns the pixels in the file's own
  colour space. Call `heifer::decode_srgb(&bytes)` to convert to sRGB (this is where the
  ICC profile, `nclx` or the HEVC VUI is applied; matrix/TRC and LUT-based RGB profiles are
  read, other input spaces are refused with `Error::Unsupported`, and PQ/HLG belong to
  `heifer::decode_hdr`). libheif converts to sRGB by default, so if you relied on that, add
  the `_srgb` call.
- **Always RGB(A), never planar.** There is no `ColorSpace::YCbCr` output and no `RgbChroma`
  variant; you get interleaved RGBA (or RGB via `to_rgb8`). Conversion to RGB is done for
  you.
- **`u16` samples by default.** `decode` keeps the coded precision in `u16`; use
  `decode_rgba8` for 8-bit output. `bit_depth` tells you what is meaningful.
- **No stride.** Rows are packed contiguously; `width * 4` samples per row.
- **No sequences.** `has_sequence`, `sequence_timescale`, `track_ids` have no equivalent.

## What you give up

`heifer` is HEIC/HEIF-with-HEVC only. If your project needs any of the following, keep
`libheif` (or use a dedicated pure-Rust crate) until these land:

| Capability | `libheif` | `heifer` | Verdict |
|---|---|---|---|
| HEIC / HEVC (8–12 bit, alpha, grids, overlays, transforms) | ✓ | ✓ | ready |
| Apple + ISO 21496-1 HDR gain maps | ✓ | ✓ | ready |
| EXIF / XMP / ICC | ✓ | ✓ | ready |
| Multiple top-level images (burst, collection, dynamic HEIF) | ✓ | ✓ (`decode_all`) | ready |
| AVIF (AV1), VVC, AVC, JPEG-in-HEIF, JPEG 2000 / HTJ2K | ✓ | — | out of scope; different codecs |
| Uncompressed (ISO/IEC 23001-17) | ✓ | — | not yet |
| 15/16-bit extended precision tools | ✓ | partial | 16-bit coding passes conformance; the extended-precision tool is not implemented |
| Inter-prediction / image sequences (tracks, MP4) | ✓ | — | out of scope (still images only) |
| Depth maps, non-alpha auxiliary images | ✓ | ✓ (`auxiliary_images`, `decode_auxiliary`) | ready |
| Region annotations / masks | ✓ | — | not implemented |
| Streaming / network `Reader` | ✓ | — (whole `&[u8]`) | not implemented |
| Runtime codec plugins | ✓ | — | by design (pure Rust, wasm, no `unsafe`) |
| LUT-based ICC for an RGB input (`A2B0`/`A2B1`, `lut8`/`lut16`/`lutAtoB`) | ✓ | ✓ | ready |
| ICC output tables (`B2A`) / soft proofing | ✓ | — | not needed to decode |
| PQ / HLG transfer functions | ✓ (HDR) | ✓ (`decode_hdr`, no tone mapping) | ready |
| IPTC | ✓ | — | not implemented |

If something in this table blocks you, that is worth an issue — it tells us what to build
next.

## Migration checklist

1. `cargo remove libheif-rs libheif-sys` / `cargo add heifer`.
2. Replace `HeifContext::read_from_file` + `primary_image_handle` + `decode` with one
   `std::fs::read` + `heifer::decode` (or `decode_srgb` if you expected sRGB, or `decode_all`
   if the file may hold several images).
3. Replace `planes()` / `stride` access with `image.data` / `to_rgba8()` / `to_rgb8()`.
4. Replace metadata block walks with `heifer::read_metadata`.
5. Replace `encode_image` + `Encoder` with `heifer::encode*`.
6. Replace `integration::image::register_all_decoding_hooks()` with
   `heifer::image_crate::register_image_decoder_hooks()`.
7. Map `HeifError` to `heifer::Error` at your API boundary.
8. Run your test suite; only files with the unsupported features above should change.
9. Check `cargo tree` for the missing `libheif-sys` and (optionally) build for
   `wasm32-unknown-unknown`.

If step 8 fails unexpectedly, that is a bug or a missing feature: please open an issue with
the file's characteristics (not the file itself).
