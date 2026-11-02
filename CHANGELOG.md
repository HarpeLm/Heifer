# Changelog

## Unreleased

**New**

- `decode_srgb`, `decode_srgb_with_options` and `decode_srgb_rgba8` now apply LUT-based RGB ICC
  profiles (`A2B0`/`A2B1`), of type `lut8`, `lut16` and `lutAtoB`, with an XYZ or Lab connection
  space. The stages run in the order the ICC specification defines, the CLUT is interpolated
  tetrahedrally as littleCMS does, and a relative-colorimetric table is preferred over a
  perceptual one. Camera and capture profiles that only ship a LUT now take the same sRGB path as
  the matrix/TRC profiles already supported. The API is additive; existing signatures are
  unchanged.
- Validated against littleCMS 2.19 on synthetic `lut8`, `lut16` and `lutAtoB` profiles, including
  the legacy v2 Lab encoding of `lut16` and tetrahedral CLUT interpolation, within 2/255 at
  8 bits.

**Correctness**

- Profiles whose input space is not RGB are still refused with `Error::Unsupported`, as are
  malformed LUTs and CLUTs too large to allocate.

## 0.3.19 — 2026-11-02

**New**

- `heifer::check_file_type` reports whether a file's leading bytes are a HEIC/HEIF file that
  heifer decodes, without decoding it: the equivalent of `libheif`'s `heif_check_filetype`.
  `FileTypeResult` mirrors `libheif-rs`, so a caller that gates on `check_file_type` can
  switch without changing its logic. The API is additive; existing signatures are unchanged.

## 0.3.18 — 2026-11-02

**New**

- `heifer::decode_all` and `decode_all_srgb_rgba8` decode every top-level image of a file, not
  only the primary one. Bursts, "collections" and Apple dynamic HEIF hold several; each is
  returned without its thumbnails, grid tiles or auxiliary planes. A single-image file yields
  one image, identical to `decode`.
- `heifer_isobmff::HeifFile::top_level_image_ids` and `is_image_item` expose the item list
  behind it. The API is additive; existing signatures are unchanged.

## 0.3.17 — 2026-11-02

**Performance**

- Decoding is faster, with byte-identical output. The inverse transform runs its column and row
  passes over only the non-zero rectangle of a block, with dedicated single-row and single-column
  paths, and the coefficient scaling is branchless with one 64-bit multiply.
- `residual_coding` derives the significance context of each 4×4 sub-block once instead of per
  coefficient, and reads a sub-block's sign flags in a single bypass run.
- The deblocking filter is specialised at compile time for vertical and horizontal edges, dropping
  a per-sample branch, and SAO copies a colour plane only when it actually holds edge-offset CTBs;
  a band-offset-only plane is filtered in place.

No API change: existing signatures are unchanged.

## 0.3.16 — 2026-11-01

**New**

- `decode_srgb`, `decode_srgb_with_options` and `decode_srgb_rgba8` decode the primary image with
  its samples converted to sRGB. Most iPhone and many camera photos are `Display P3`; shown as
  sRGB their colours are wrong. The API is additive; existing signatures are unchanged.
- Matrix/TRC RGB ICC profiles are read and applied: per-channel curves (`curv`, and the parametric
  `para` types), the colourant matrix (`rXYZ`/`gXYZ`/`bXYZ`), and a Bradford adaptation from the
  profile's white to D65, then the sRGB transfer. Validated against littleCMS on `Display P3`,
  `sRGB`, `Adobe RGB`, DCI-P3 and `ITU-2020` profiles (mean ≤ 0.25/255, max ≤ 0.6/255 at 8 bits).
- When a file carries neither an ICC profile nor `nclx`, the samples are returned as coded, as
  `decode` does. LUT-based (`A2B0`/`B2A0`) profiles and PQ/HLG transfers (which are HDR, see
  `decode_hdr`) are refused with `Error::Unsupported`.
- The `nclx` code points the conversion understands are BT.709, BT.601, BT.2020, DCI-P3 and
  Display P3 primaries, with sRGB, Rec.709, γ2.2, γ2.8 or linear transfers.

**Correctness**

- Encoding with an ICC profile in the metadata now attaches it to **every** image item (tiles, the
  grid, the thumbnail and the `tmap` item), as iPhone photos do, and does not write `nclx` next to
  it. Previously the profile was on one item only, and a `nclx` box claimed sRGB.

**Tests**

- Colour tests: a Display P3 fixture converts (the profile is applied), our own `nclx`-tagged
  files and files without colour information are left as coded, an sRGB profile round trips, and
  the profile replaces `nclx` on every item of a grid.
- Unit tests cross-check the conversion chain against the colourant matrix and the primaries.

## 0.3.15 — 2026-11-01

**New**

- `encode_hdr` and `encode_hdr_with_metadata` write ISO 21496-1 HDR gain maps: a monochrome,
  half-resolution gain map item referenced by a `tmap` item, an `altr` entity group, and the
  `tmap` compatible brand. The API is additive; existing signatures are unchanged.
- `heifer-isobmff` exposes the pieces: `HeifWriter::add_tmap`, `add_hidden_grid` and
  `add_alternative_group`.

**Correctness**

- Entity group IDs (`grpl`) are numbered after the last item ID, as ISO/IEC 23008-12 requires.
  ImageIO ignored an `altr` group whose ID was also an item ID, so it did not apply the gain map.

**Tests**

- Round trips of synthetic HDR images, of a half-resolution gain map grid, with metadata, and of
  invalid inputs, plus a re-encode of a Samsung Galaxy photo carrying a real ISO 21496-1 gain map
  (fetched by `scripts/fetch-fixtures.sh`).
- On macOS, ImageIO reports the declared headroom for both the synthetic file and the Samsung
  re-encode, and decodes them to 10-bit PQ.

## 0.3.14 — 2026-11-01

Contributed by [@zienag](https://github.com/zienag) in
[#2](https://github.com/HarpeLm/Heifer/pull/2).

**Correctness**

- `imir` is applied the right way round: code 0 exchanges top and bottom, as ISO/IEC 23008-12
  6.5.12 specifies. Phones write this property on upright photos, so mirrored ones showed flipped.
- Chroma is no longer interpolated into the padding past the picture's edge, which left a wrong
  line along the right and bottom of grid tiles. On a Xiaomi sample the worst error against
  libheif drops from 185/255 to 5/255.
- `iden` (identity) items decode, so files whose primary item is a transformed copy of another
  item now open (C008, C014 and C039 of the conformance set).
- A coded picture larger than `ispe` is converted at the `ispe` size, without a crop copy.

**New**

- `decode_rgba8` returns `Image<u8>`, half the memory. On 8-bit files the samples are identical
  to `to_rgba8`; on 10- and 12-bit files they can differ by one.
- `Image` is generic over its sample type, `u16` by default. `Image::filled_samples` and
  `frame_to_rgba_as` build the `u8` form; `Image::filled` and `frame_to_rgba` keep their
  signatures, so existing code compiles unchanged.
- `Image::<u8>::into_rgb8` drops alpha in place.
- `irot 0` no longer copies the image.

**Performance**

- Colour conversion runs in plain per-channel loops that the compiler vectorizes: 16% off a
  12 MP decode.

**Build**

- CI fetches the conformance and Nokia fixtures, so the tests that need them no longer skip.

## 0.3.13 — 2026-11-01

**Robustness.** Two malformed files that used to panic are now rejected with an error.

- A `grid` whose tile `ispe` declares a zero or impossible size is rejected, and tile placement
  no longer overflows on 32-bit targets, instead of panicking.
- An alpha plane with the right width but a smaller height than the image is rejected, instead of
  being read out of bounds.
- Regression tests build both files with the writer and check the errors.

## 0.3.12 — 2026-11-01

- `heifer::image_crate::register_image_decoder_hooks_with_options` registers the `image` crate
  hooks with decoding options, for example `max_threads: 1` in applications that already decode
  several images in parallel; `HeifDecoder::with_options` does the same for one decoder.

## 0.3.11 — 2026-11-01

**Faster encoding.** About 10% faster than 0.3.10 (12 MP: 0.77 s on 10 cores, 4.0 s on one),
with the same compression: heifer stays ahead of still265's Slow preset on the benchmark images.

- Whole-block trials estimate their luma distortion from the quantization errors of the levels,
  in the transform domain, and only the chosen mode is reconstructed.
- SAO is chosen on the encoder's own deblocked reconstruction, instead of decoding the picture a
  second time (the decoder gains `Reconstructor::deblocked` and block edge save and restore).
- 64×64 blocks are tried whole only when quite flat (elsewhere the split wins 96% of the time),
  and their mode search is shared with their first 32×32 block.
- Faster SAO statistics; the encoder's bin log reuses its buffers.

## 0.3.10 — 2026-11-01

**Faster encoding, same files.** Encoding is about 10% faster (12 MP: 0.8 s on 10 cores); the files are
byte-identical to 0.3.9.

- RDOQ, sign data hiding and residual coding read the scan positions and the significance
  contexts of each sub-block from tables built once, instead of recomputing them for every
  block; the cost of the last-position prefixes is computed once per block.
- Blocks whose coefficients all round to zero skip RDOQ (contributed with GitHub Copilot).
- Intra prediction skips the tile check in single-tile pictures (decoder and encoder).
- The seed files of the scheduled fuzzing are fetched from the new location of Nokia's samples.

## 0.3.9 — 2026-10-31

**Smaller files.** At equal quality, encoded files are 6 to 7% smaller than with 0.3.8, slightly
smaller than still265's Slow preset (0.3 to 0.6% on the benchmark images), 5% smaller than x265
`medium` and 3% smaller than x265 `slow`. Encoding takes about 1.5 times as long (12 MP: 0.9 s on 10 cores).

- SAO (sample adaptive offset), decided on the deblocked picture.
- 64×64 coding blocks; transform trees split up to twice (32 → 16 → 8, 16 → 8 → 4).
- More prediction modes coded for real, 4×4 blocks included; chroma mode chosen by
  rate-distortion cost.
- Transform skip for 4×4 blocks, tried where it pays (graphics, text: 4% smaller).
- RDOQ also weighs coding nothing at all; chroma distortion weighs more in decisions, as it does
  in RGB.
- Thumbnails are coded at quality 60 at most.

## 0.3.8 — 2026-10-31

- **Thumbnails**: images larger than 512 pixels get a thumbnail (`thmb` item, longest side 320
  pixels, with its alpha plane), as in iPhone photos, which file browsers can show without
  decoding the whole image. `heifer::decode_thumbnail` reads it; `heifer info` lists it.
- **Benchmarks**: [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) compares heifer with heic-rs,
  heic, heic-decoder, zune-heic, still265, ffmpeg, x265 and Apple's ImageIO on decoding speed,
  fidelity and coverage, and on encoding size, quality and speed. `benchmarks/run.py` reproduces
  it on any Mac.

## 0.3.7 — 2026-10-31

**Colours as Apple shows them.** Decoded images now match the output of Apple's decoder
(ImageIO) within about 50 dB PSNR on Apple photos, instead of 35 to 48 dB, closer than any other
decoder measured (ffmpeg, heic-decoder, heic, heic-rs, zune-heic).

- 4:2:0 and 4:2:2 chroma is upsampled bilinearly with the chroma samples on the even luma rows
  and columns, as Apple's decoder does, instead of by nearest neighbour. About 4% slower on one
  core.
- The encoder computes its chroma on the same positions (a [1 2 1] filter in both directions):
  files are decoded more faithfully by Apple and heifer, 22 to 31% smaller than those of Apple's
  encoder at equal quality as Apple decodes them (15 to 30% before).

## 0.3.6 — 2026-10-31

**Faster encoding on each core.** A 12 MP image takes 3.5 s on one core (4.6 s in 0.3.4) and
0.7 s on a 10-core Mac (0.9 s); same files.

- The encoder gathers the reference samples of a block once for all the prediction modes it
  compares, and reconstructs coded blocks from the prediction it already computed.
- `heifer-hevc-dec`: `Reconstructor::intra_references`, `predict_from` and `reconstruct`, for
  encoders; decoding is unchanged (same output, same speed).
- RDOQ precomputes the scan positions and magnitudes of each block.

## 0.3.5 — 2026-10-31

- Encoder: trials estimate their rate instead of coding it. Their bins go to a log that adds
  up their cost from the context probabilities and updates the contexts like the arithmetic
  coder; the log of the chosen coding is replayed into the arithmetic coder once each 32×32
  block is decided. About 10% faster, same compression.

## 0.3.4 — 2026-10-31

**Faster encoding.** About 1.8× faster on one core and 30% faster on a 10-core Mac (a 12 MP
image in 0.9 s instead of 1.25 s), for files 0.5% larger on average (BD-rate vs x265 `medium`:
−0.1% on a photo, +0.5% on an image with text).

- Trials that almost never win are skipped, from statistics of the decisions: splitting blocks
  that already cost little per pixel, coding very detailed 32×32 blocks whole, coding the 2nd
  and 3rd prediction modes when their estimate is more than 10% worse than the best.
- The chroma mode is chosen once per coding unit.
- Mode estimation uses the new `Reconstructor::predict_block` of `heifer-hevc-dec`, which
  predicts without writing the picture. RDOQ reuses its buffers.

## 0.3.3 — 2026-10-31

**10-bit HEIC.** Images of more than 8 bits per sample (16-bit PNGs, 10-bit photos decoded by
heifer) are now encoded in 10 bits, HEVC Main 10, instead of being rounded to 8 bits: smooth
gradients keep their levels instead of turning into steps. Apple's ImageIO and ffmpeg read them
as 10-bit images.

- `heifer::encode`, `encode_with_metadata` and the `image` crate `HeifEncoder` (16-bit images)
  encode in 10 bits; 8-bit images are encoded as before.
- `heifer-hevc-enc`: `encoder::encode_picture16` with `Planes16` (8 to 10 bits),
  `transform::forward_transform` and `quantize_levels` take a bit depth. The `encode_yuv`
  example reads `yuv420p10le` files.
- The `encode` fuzz target covers 10-bit pictures.

## 0.3.2 — 2026-10-31

**Smaller HEIC files.** At equal quality, the encoder now makes files slightly smaller than x265's
`medium` preset (BD-rate −0.6% on a photo, −0.5% on an image with text), against +11% and +13%
before. Encoding is about 4× slower (a 12 MP image in 1.6 s on 10 cores).

- 4×4 prediction blocks in 8×8 coding units, and transform blocks split in four (with the 4×4
  DST for luma); each coding unit tries these against one transform and keeps the cheapest.
- The three most promising luma modes are coded for real and compared by rate-distortion cost;
  the chroma mode is chosen too, instead of always following luma.
- RDOQ: levels, sub-blocks and the last position chosen with bit costs estimated from the CABAC
  contexts. Sign data hiding.
- Coding units that need no residual are not split further.
- `heifer-hevc-dec`: `Reconstructor::set_reconstructed` (for encoders that try several codings of
  an area) and the `DST4` matrix are public. `heifer-hevc-enc`: `transform::forward_dst4` and
  `quantize_exact`, and an `encode_yuv` example for rate-distortion measurements.

## heifer-cli 0.3.1 — 2026-10-31

- New `heifer-cli` crate: the `heifer` command converts HEIC/HEIF to and from PNG, JPEG, WebP,
  TIFF, BMP and GIF, one file or many (`--to jpg -o dir`), keeps EXIF and ICC profiles, writes
  photos upright, and never overwrites files without `--force`. `heifer info` shows the size,
  tiles, camera, date and metadata of a file.
- Web demo: PNG, JPEG and WebP images are converted to HEIC in the browser (in a worker), with a
  quality slider and the result shown as decoded from the HEIC file.

## 0.3.1 — 2026-10-31

- `image` crate: `HeifEncoder` implements `image::ImageEncoder`, so that
  `img.write_with_encoder(HeifEncoder::new(file))` writes HEIC files. 8- and 16-bit grey and
  RGB, with or without alpha; the ICC profile and EXIF set by `image` are stored.
- `encode_with_metadata`: encoding with EXIF, XMP and the ICC profile, e.g. those of the
  original file. The EXIF orientation is reset to 1, since decoded pixels are already upright.
- `heifer-isobmff`: `HeifWriter::add_xmp` and `add_icc_profile`.

## 0.3.0 — 2026-10-31

**Encoding.** heifer now writes HEIC files, in pure Rust.

- `heifer::encode`, `encode_rgba8` and `encode_rgb8`, with `EncodeOptions` (quality 0–100,
  threads): 8-bit RGB(A) to YCbCr 4:2:0 (sRGB, BT.601 full range), 512×512 grid tiles encoded in
  parallel, alpha as a monochrome auxiliary image, any size (odd sizes cropped with `clap`). A 12 MP
  image takes about 0.4 s on 10 cores. The files are decoded by Apple's ImageIO and by ffmpeg.
- `heifer-hevc-enc`: HEVC Main intra encoder (`encoder::encode_picture`). Coding blocks of 8×8 to
  32×32 and the luma mode are chosen by rate-distortion cost; the picture is reconstructed with the
  decoder's code, so that the decoder reproduces it sample for sample (checked by the tests).
  `ArithmeticEncoder` gains `bit_count` and checkpoints.
- `heifer-isobmff`: `writer::HeifWriter` writes HEIF files: HEVC image items, grids, alpha planes,
  EXIF, `colr`, `pixi`, `clap`, shared properties.
- Decoding: alpha planes of grid images (an alpha grid referencing the image grid).
- **Container**: `iloc` construction method 2: item data built from ranges of other items
  (`iloc` references), nested or not, with limits against reference cycles and data blow-up.
- New fuzz target `encode` (encode then decode random images), and a `to_heic` example.
- `heifer_hevc_dec::recon::DCT32` is public.
- **Breaking:** `heifer_isobmff::boxes::Extent` has a new `index` field (`extent_index`);
  `heifer::Error` has a new `Encode` variant.

## 0.2.1 — 2026-10-31

- **HDR**: ISO 21496-1 gain maps stored as HEIF `tmap` items (per-channel gain, gamma, offset
  and headroom). `decode_hdr` uses them in preference to Apple gain maps when both are present.
- crates.io metadata: documentation links, a README for each crate, and keywords and
  categories per crate (the decoders are no longer listed under encoding).

## 0.2.0 — 2026-10-08

About 2× faster than 0.1.3 for `decode` + `to_rgba8`, and faster again on large photos.

- CABAC: `ivlOffset` kept scaled with bytes read ahead (one shift per renormalization) and
  branchless bypass bins.
- Grid tiles without transforms or alpha are colour-converted straight into the final image,
  each tile into its own row segments (no intermediate image, no shared lock).
- Fixed-point YCbCr → RGB: 116 of 195 million samples of the test files change by 1.
- `to_rgba8`/`to_rgb8`: plain copy for 8-bit images, lookup table otherwise.
- Intra reference samples gathered per 4×4 block; angular prediction transposed in place;
  scaling list factors from a lookup table instead of a search.
- **Breaking:** `heifer_hevc_dec::syntax::TransformBlock` has a new `coeff_bounds` field;
  `heifer::color::frame_to_rgba_into` is new.

## 0.1.3 — 2026-10-08

- **HDR photos**: Apple HDR gain maps (iPhone). `decode_hdr` returns linear-light RGB with the
  gain map applied, matching Apple's decoder (ImageIO) within 0.3% on average; `read_gain_map`
  gives the gain map and headroom to apply them yourself. `Exif::apple_hdr_headroom` reads the
  headroom from the Apple MakerNote.

## 0.1.2 — 2026-10-08

Faster again, still without any `unsafe` code and with bit-identical output.

- The inverse transform works on fixed-size, contiguous rows and the intra prediction writes whole
  rows, so the compiler vectorizes them (NEON, SSE/AVX, wasm simd128).
- About 10% faster single-threaded (12 MP iPhone photo: 208 → 187 ms on one thread, 42 → 36 ms on
  ten). In the browser, a 1280×854 photo decodes in ~80 ms instead of ~115 ms.

## 0.1.1 — 2026-10-08

Faster decoding, with bit-identical output.

- About 2.2× faster single-threaded and 2.4× faster on 10 cores (12 MP iPhone photo: 454 → 208 ms
  on one thread, 102 → 42 ms on ten). In the browser, a 1280×854 photo decodes in ~115 ms instead
  of ~260 ms.
- Inverse transform without allocations, using only the non-zero coefficients and an even/odd
  decomposition of the DCT.
- Intra prediction without allocations; neighbour availability computed once per 4×4 block.
- Table-based YCbCr → RGB conversion, cache-friendly rotation, fewer image copies.
- Fuzzing now runs almost continuously on CI.

## 0.1.0 — 2026-10-08

First release: a pure-Rust HEIF/HEIC decoder.

- **HEIF container** (`heifer-isobmff`): items, properties, references, grids, overlays, alpha,
  `clap`/`irot`/`imir`, EXIF, XMP and ICC access.
- **HEVC decoder** (`heifer-hevc-dec`): Main, Main 10, Main Still Picture and intra Range
  Extensions (8–12 bit, 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4), tiles, WPP, PCM, scaling lists, deblocking
  and SAO, decoded picture hash verification. Identical to ffmpeg on 62/62 comparable conformance
  bitstreams; 41 verified against the reference decoder's hashes.
- **`heifer::decode`**: RGB(A) output with grid assembly (tiles decoded in parallel), overlays,
  alpha, transforms and YCbCr→RGB conversion; `read_metadata` for EXIF/XMP/ICC.
- **`image` integration** (feature `image`): `HeifDecoder` and hooks so that `image::open` reads
  `.heic` files.
- **Safety**: `#![forbid(unsafe_code)]`, configurable size limits, fuzzed nightly (5 targets).
- **WebAssembly**: builds for `wasm32-unknown-unknown` (≈ 250 KB demo).

Not yet supported: HDR gain maps, applying ICC profiles, extended precision (16-bit) HEVC
tools, inter-coded image sequences, encoding (`heifer-hevc-enc` only contains the CABAC encoder).
