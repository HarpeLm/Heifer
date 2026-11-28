# Changelog

## Unreleased

**New APIs**

- AVIF files are described without decoding: `read_info` and `read_info_reader` give the size
  (after `clap`, `irot` and `imir`), bit depth and alpha of `av01` items and grids of them, and
  the new `FileKind::Avif` (`"avif"`, `"image/avif"`). Decoding AVIF is not supported yet and
  returns `Error::Unsupported`. Checked on all 172 files of the AOMediaCodec/av1-avif test set.
- `heifer-isobmff`: new `Av1Config` (the `av1C` box) and `HeifFile::av1_config`.
- New `heifer-av1-dec` crate (not published yet), the start of a safe-Rust AV1 decoder for AVIF:
  it splits AV1 bitstreams into OBUs and parses sequence headers (profile, maximum size, bit
  depth, colour configuration, enabled tools). Checked against the `av1C` box and image size of
  all 238 AV1 items in the AOMediaCodec/av1-avif test set, and fuzzed by a new `av1` target.
  It also reads intra frame headers (frame size, superres, tiles, quantizer, segmentation,
  deblocking, CDEF, loop restoration, film grain) and cuts tile groups into tiles: the 212
  single-layer items of the test set parse to their `ispe` size, and multi-layer items stop at
  their first inter frame. The symbol decoder and its CDF adaptation are in place, with the 96
  default CDF tables generated from the specification source (`scripts/gen-av1-tables.py`, which
  also generates the other lookup tables). `decode_frame` now reconstructs intra frames before
  loop filtering: every intra prediction mode (directional with edge filtering, smooth, Paeth,
  filter intra, chroma from luma, palette), all partitions, coefficients, quantizer and
  segmentation deltas, and the DCT, ADST, identity and Walsh-Hadamard inverse transforms, at 8, 10
  and 12 bits in 4:0:0, 4:2:0, 4:2:2 and 4:4:4. 18 small aomenc streams
  (`scripts/make-av1-streams.sh`) match aomdec sample for sample, and 204 of the 238 items of the
  av1-avif test set match dav1d run without loop filters; the other 34 use inter frames (spatial
  layers) or intra block copy, which are not supported yet.

## 0.9.0 — 2026-11-27

Bounded-memory tile-by-tile preview decoding (`decode_preview_rgba8` / `decode_reader_preview_rgba8`),
reader sRGB decoding with graceful ICC fallback (`decode_reader_srgb_rgba8`, `Options::fallback_to_coded_color`),
ISO 21496-1 `tmap` gain-map extraction in `read_gain_map`, still-image `EncodeSpeed::Fast` (~2.2× faster
end-to-end / ~3× faster pure HEIC encoding on 12 MP), and safe-Rust encoder/decoder hot-loop speedups.

**New APIs**

- Added bounded-memory preview decoding (`PreviewOptions`, `decode_preview_rgba8`,
  `decode_reader_preview_rgba8`) that decodes grid tiles directly into a downscaled RGBA buffer
  without allocating the full-resolution image (ideal for 48–200 MP photos, galleries, and WASM).
- Added `decode_reader_srgb_rgba8` and `Options::with_fallback_to_coded_color(bool)` so callers
  can decode from a `Read + Seek` stream to sRGB and gracefully fall back to coded color when an
  unsupported ICC profile is present.
- Extended `read_gain_map` and `read_gain_map_with_options` to extract ISO 21496-1 (`tmap`) gain
  maps in addition to Apple (`urn:com:apple:photo:2020:aux:hdrgainmap`) gain maps, and re-exported
  `APPLE_GAIN_MAP`, `decode_hdr_with_options`, and `read_gain_map_with_options` at the crate root.

**Performance (100% safe Rust)**

- `heifer-hevc-enc`: wired up `SpeedPreset::Fast` / `Slow` for still-image intra encoding (12 MP
  `photo12.png` CLI encode drops from `806 ms` → `371 ms` at q=75 and `1019 ms` → `456 ms` at q=85
  with `EncodeSpeed::Fast`, ~4.7–4.9% larger files; `Medium` remains bit-exact).
- `heifer-hevc-enc`: optimized `rdoq`, `hide_signs`, even/odd butterfly `forward_transform`,
  `residual_coding`, `quantize`, `reconstruct`, and `block_sse` (`-8%` to `-10%` faster `Medium`
  encoding with byte-for-byte identical output: 12 MP q=82 `903 ms` → `823 ms` on 10 threads).
- `heifer-hevc-dec`: tightened `residual_coding` hot loops, avoided redundant prediction buffer
  copies in `transform_block_impl`, and eliminated unnecessary stack zeroing in `predict_with`
  (`-1%` to `-2.2%` faster 1-thread decoding).

**Fixes**

- A forged HEVC stream could make the decoder panic (index out of bounds) instead of returning an
  error: a dependent slice segment naming another PPS had its address checked against that PPS's
  picture size, then used the previous segment's smaller picture. Slice segments of one picture
  must now use the same parameter sets, for still images as for sequences. Found by the `hevc`
  fuzz target.

## 0.8.0 — 2026-11-27

Header-only image probing (`read_info`), lazy limit-enforcing `image` crate decoding, pre-1.0
`#[non_exhaustive]` API hardening, fused grid transforms and loop-filter speedups (200 MP multi-core
`185 ms`, down from `401 ms` in `0.7.3`), full Nokia HEIF conformance fixes (edit lists, predicted
items, zero-length NALs), and fuzzing hardening.

**Breaking / Pre-1.0 API Hardening**

- Marked `Error`, `ReadError`, `Options`, `EncodeOptions`, `Metadata`, and `DepthRepresentation` as
  `#[non_exhaustive]` and added `new()` / `with_*` builders for forward compatibility.

**New APIs**

- Added header-only `read_info`, `item_info`, and `read_info_reader` (`FileKind`, `ImageInfo`) to
  query post-transform dimensions, bit depth, alpha presence, and container kind without decoding
  pixel payloads, plus `decode_reader_rgba8`.
- Made `HeifDecoder` and seekable `HookDecoder` (`image` crate integration) lazy: metadata and
  dimensions are parsed from container headers on construction, `image::Limits` are enforced before
  decoding pixels, and 8-bit images take the direct `decode_rgba8` fast path.

**Performance (100% safe Rust)**

- `heifer`: fused `clap` + `irot` + `imir` transforms directly into grid tile placement
  (`GridTransform`, `decode_grid_with_transform`), avoiding full-image post-transform copies and
  skipping out-of-crop tiles.
- `heifer-hevc-dec`: sped up `deblock` / `SAO` loops (zero-offset CTB skip, 32-entry `band_lut`,
  row-wise 8-bit `PlaneSample::sao_eo_row`) and added a single-tile intra reference fast path
  (`try_fast` in `reference_samples_into`).
- Measured results (Apple M5): 200 MP Galaxy `185 ms` multi-core (`2.17×` faster than `0.7.3`) /
  `885 ms` 1-thread; 48 MP `106 ms` / `622 ms`; 12 MP Apple photo `83 ms` `heifer8` 1-thread
  (`1.30×` faster than `heic-rs` at `108 ms`) / `14 ms` on 10 cores; 13-file suite `582 ms`
  1-thread / `127 ms` on 10 cores; 5 MP Xiaomi `13 ms` / `64 ms`.

**Fixes & Fuzzing** (including Nokia HEIF conformance files)

- Predictively coded image items (`pred` references, `mif2` brand, Nokia C044) are decoded: the
  items they reference first, then the image, as one HEVC sequence. Decoding from a reader now
  also loads those reference items.
- Image sequence samples with a composition offset of -2^31 are decoded as references but no
  longer shown (Nokia C041: 8 frames and 0.8 s instead of 9 frames and 2,147,484 s).
- Image sequence edit lists (`edts`/`elst`) are applied: frames an edit skips are not shown,
  frames it selects again are shown again (Nokia C029: 13 frames and 1.3 s instead of 8), and a
  media rate of 0 holds one picture. Leading empty edits become
  `SequenceInfo::start_delay` (Nokia C030) and a later one keeps the previous frame on screen.
  With the repeat flag, the frames play again until the track's duration is over (C036, C037);
  an indefinite duration sets `SequenceInfo::repeats_forever` (C038). New `Track::edits`,
  `repeat_edits`, `presented_duration`, `presentation`, `start_delay` and `repeats_forever` in
  `heifer-isobmff`.
- Zero-length NAL units in item and sample data, and empty `hvcC` entries, are skipped (Nokia
  C034, #12, by @argentist).
- `heifer`: fall back to sequential sequence decoding in `closed_gops` when a container flags a
  non-random-access sample as sync (fixes parallel vs sequential divergence found by fuzzing).
- `heifer-isobmff`: bounded cumulative sample counts across all tracks in `parse_tracks`
  (`MAX_SAMPLES = 1 << 18`) and eliminated redundant `vec![uniform; sample_count]` allocations for
  uniform `stsz` (fixes 214 MB OOM on a 5.6 KB input found by 8-hour overnight fuzzing).
- `heifer-hevc-dec`: skip `view_reuse` test cleanly when optional local fixtures and conformance
  streams are absent, so `cargo test --workspace` passes on a fresh clone.
- Added differential fuzz targets `info` and `image_decoder` plus regression inputs under
  `fuzz/regressions/{parallel,container}`.

## 0.7.3 — 2026-11-26

Per-thread buffer reuse across grid tiles, direct 8-bit `Frame8` planes with zero-copy
`CropWindow` decoding, WebAssembly `.heics` sequence demo support, and improved crates.io/docs.rs
documentation.

**Performance & Memory (100% safe Rust)**

- Per-thread buffer reuse across plain grid tiles:
  - `heifer-isobmff`: added `HeifFile::hevc_bitstream_into` and `hevc_annex_b_into` helpers.
  - `heifer-hevc-dec`: added `DecodeWorkspace`, `ReconstructorBuffers`, `PictureBuffers`,
    `FilterInfo::reset`, reused SAO scratch buffers, and `decode_picture_into`.
  - `heifer`: added `ColorScratch` and per-worker `GridWorkspace`.
- Direct 8-bit (`u8` planes) and zero-copy conformance crop path:
  - `heifer-hevc-dec`: added `Frame8`, `CropWindow`, `DecodedFrameView`,
    `decode_picture_view_into`, `deblock_8`/`sao_8`, and `PictureHash::compute_like_8`, eliminating
    `Frame::cropped_into` copies and halving 8-bit plane and SAO memory bandwidth.
  - `heifer`: added zero-allocation strided tile output (`frame_view_to_rgba_strided_with_scratch`)
    when `threads <= 1` on plain grids (`decode_grid`) and preallocated tile segment vectors on
    multi-core grids.
- Measured results (Apple M5): 200 MP Galaxy (`768` tiles) multi-core `534.1 ms` (down from
  `1013 ms` in `0.7.1`) and 1-thread `1080.1 ms` best; 48 MP 1-thread `638.9 ms`; 5 MP Xiaomi
  `68.5 ms`; 12 MP iPhone `109.4 ms` best / `110.1 ms` median vs `129.8 ms` for `heic-rs`
  (`ratio 0.848`).

**WebAssembly & Sequences**

- `heifer`: avoided spawning worker threads in `encode_sequence` when `max_threads <= 1`, fixing
  sequence encoding on single-thread WebAssembly builds.
- Browser demo: added `.heics` image sequence playback, GIF/APNG/PNG-ZIP export, and
  GIF/APNG-to-`.heics` conversion.

**CI, Docs & Packaging**

- Disabled the Homebrew formula target in `dist-workspace.toml` and `.github/workflows/release.yml`
  (no `HOMEBREW_TAP_TOKEN` secret configured) so tagged Release CI can complete cleanly alongside
  crates.io Trusted Publishing.
- Updated package `Cargo.toml` descriptions/keywords, `docs.rs` crate-level docs, and `README.md`
  with a `heifer` vs `libheif-rs` / `heic-rs` / `heic` comparison, inline `Migrating from libheif-rs`
  guide, and copy-paste 5-line `image` crate hook snippet.
- Validated with a 6-target nightly `cargo-fuzz` campaign across all targets (`0` crashes, `0`
  timeouts, `0` OOMs).

## 0.7.2 — 2026-11-26

Second pass of 100% safe-Rust single-thread decoding and colour-conversion optimizations (now
~1.20× faster than `heic-rs` on 12 MP iPhone photos and ~1.18× faster across the 13-file benchmark
suite), validated bit-exact across all 114 conformance bitstreams (7,115 pictures).

**Performance**

- `heifer-hevc-dec`:
  - `cabac`: added packed `u8` context state decoding (`decode_packed`), capped bypass Exp-Golomb
    decoding (`decode_bypass_unary_capped`), inline bypass bit refill fallback, and EOF bounds guard
    in `fetch_u32`.
  - `syntax`: replaced `[bool; 64]` coded-subblock grid with a `u64` bitmask, switched
    `sig_coeff_flag` and `greater1_flag` loops to register-cached `decode_packed`, used capped
    bypass decoding and whole-row `clear_coeffs`, and deferred `Picture::motion` allocation on
    intra-only pictures.
  - `recon`: eliminated per-block intra reference sample heap copies via scratch-buffer
    `reference_samples_into`, scoped angular buffers, and added a 4-column vectorizable 2-D IDCT
    (`inverse_2d_dct_x4`) for `N >= 8`.
  - `filters`: deferred `FilterInfo::motion` vector allocation on intra-only pictures.
- `heifer`: precomputed luma bias (`y_bias`) in the fixed-point YCbCr-to-RGBA loop (`color.rs`).
- Measured 1-thread results (`heifer8`, Apple M5): 12 MP iPhone `109 ms` (`decode_rgba8`; `114 ms`
  16-bit) vs `131 ms` for `heic-rs`, and `641 ms` vs `754 ms` across the 13-file suite (`89 ms`
  vs `107 ms` on Apple's 12 MP photo).
- Refreshed [docs/BENCHMARKS.md](docs/BENCHMARKS.md) and pure-Rust decoder numbers in
  [README.md](README.md).

## 0.7.1 — 2026-11-25

Single-thread HEVC decoding and YCbCr-to-RGBA speedups (13–21% faster in 100% safe Rust,
now faster than `heic-rs` on 1 thread), refreshed public benchmarks, and crates.io Trusted
Publishing in the release workflow.

**Performance & CI**

- **Single-thread decoding & colour conversion** (`#![forbid(unsafe_code)]`, bit-exact across all
  114 conformance sequences / 7115 pictures):
  - `heifer-hevc-dec`: 32-bit `u64` CABAC refill (`cabac.rs`), precomputed `sig_coeff_flag` scan
    context lookup, split non-DC loop, zero-escape fast path & skipped pre-initialized CU grid
    fills (`syntax.rs`), 7-multiply inverse 4×4 DST-VII, unrolled `idct4`, reused 2-D transform
    scratch buffer, `i32` flat dequantization path, zero-residual bypass, `single_tile` intra
    reference fast path and direct vertical/horizontal angular modes (`recon.rs`), row-major
    deblocking & clean SAO offset LUTs (`filters.rs`), and allocation-free `merge`/`amvp` + stack
    clamping buffer in inter interpolation (`inter.rs`).
  - `heifer`: removed unused `xcs` allocation on YCbCr paths and fused horizontal chroma widening
    directly into the 4:2:0/4:2:2 pixel output loop (`color.rs`).
  - Measured 1-thread speedups (`heifer8`): 12 MP iPhone `114.6 ms` vs `129.2 ms` median for
    `heic-rs` (13-file suite `646 ms` vs `729 ms`), 5 MP Xiaomi `74.7 ms`, 48 MP `700 ms`, 200 MP
    `1.21 s` on 1 thread (`0.43 s` multi-thread).
- Refreshed public benchmarks in `README.md`, `docs/BENCHMARKS.md`, `docs/benchmark.json`, and
  `docs/images/bench-*.svg`.
- Added crates.io Trusted Publishing (OIDC via `rust-lang/crates-io-auth-action@v1`) in
  `.github/workflows/release.yml` so tagged releases `vX.Y.Z` publish all workspace crates
  automatically alongside the GitHub release.

## 0.7.0 — 2026-11-25

Video sequence encoder upgrades (TMVP, per-slice QP offsets, hierarchical B-frames,
4:2:2/4:4:4/lossless & 10-bit formats, `EncodeSpeed`/`SpeedPreset`), `ctts` v0 + `edts`/`elst`
timing alignment, full `.heics` CLI support, sequence benchmarks, and sequence fuzzing.

**Breaking**

- `heifer::EncodeOptions` adds `speed: EncodeSpeed` (use `EncodeOptions::new()` and
  `with_speed`, or `..Default::default()`).

**New**

- `encode_sequence_with_format` and `encode_sequence_to_size_with_format` support `Ycbcr422`,
  `Ycbcr444` and `Lossless` (GBR + lossless inter `cu_transquant_bypass_flag`) image
  sequences.
- `EncodeSpeed` (`Fast`, `Medium`, `Slow`) / `heifer_hevc_enc::encoder::SpeedPreset` controls
  motion search and partition/mode trials across still and sequence encoding.
- Sequence encoding enables temporal motion vector prediction (`sps_temporal_mvp_enabled_flag` /
  `slice_temporal_mvp_enabled_flag`), hierarchical referenced B-frames (`TrailR`) when
  `b_frames >= 3`, and per-slice QP offsets (`+1` for P, `+2` for referenced B, `+3` for
  non-referenced `TrailN` B-frames).
- `benchmarks` gains a `sequence` subcommand to measure sequence encode/decode speed, size and
  PSNR across presets and B-frame configurations.
- CLI: `--speed <fast|medium|slow>`, `--keyframe-interval <n>`, `--b-frames <n>`, `--fps <n>`,
  `--format 422|444|lossless` on `.heics`, and combining multiple input image files into a single
  `.heics` sequence (`heifer frames/*.png out.heics`).
- `heifer_isobmff::writer::SequenceTrack` normalizes negative `ctts` offsets to non-negative `ctts`
  v0 values paired with an `edts`/`elst` edit list so `PTS >= DTS` for every sample.
- Fuzz targets (`container`, `hevc`, `decode`, `parallel`, `encode`) cover sequence parsing,
  `ctts`/B-frame reordering, TMVP, parallel sequence decoding and sequence round trips.

## 0.6.0 — 2026-11-22

Image sequences encode B-frames (`ctts`, GOP reordering, bi-prediction) and transparent alpha
tracks (`auxi`/`auxl`), gain configurable keyframe intervals and byte-size budgets, and clean
up deprecated internal APIs.

**Breaking**

- `heifer_hevc_dec::{bitreader, filters, refs, sei}` and `heifer_hevc_enc::transform` are now
  private (deprecated since 0.5.1).
- `heifer_isobmff::boxes::Property::ComponentDescriptions` parses `cmpd` directly in
  `HeifFile::properties` instead of leaving it as `Property::Unknown`.
- `heifer::EncodeOptions` adds `keyframe_interval` and `b_frames` (use `EncodeOptions::new()`
  and `with_keyframe_interval` / `with_b_frames`, or `..Default::default()`).

**New**

- Image sequences encode B-frames (`SliceType::B` with L0/L1/bi-prediction motion search,
  merge/skip and residuals) by default (`EncodeOptions::b_frames = 2`), with GOP reordering via
  `heifer_hevc_enc::encoder::SequenceEncoder::{with_b_frames, push, flush}` and `ctts`
  composition time offsets in `heifer_isobmff::writer::SequenceTrack::add_sample_with_offset`.
- `encode_sequence` supports transparent frames (writing an auxiliary monochrome alpha track
  referenced by `auxl` with `auxi`), custom keyframe intervals via
  `EncodeOptions::with_keyframe_interval`, and size targets via `encode_sequence_to_size`.
- `decode_sequence` and `decode_sequence_frame` decode auxiliary alpha sequence tracks (`auxl`)
  and attach the alpha plane to each decoded frame.
- `heifer_isobmff::writer::SequenceTrack::set_alpha` writes an auxiliary `auxv`/`auxl` alpha
  track alongside the main sequence track, and `heifer_isobmff::track::Track::auxiliary_type`
  parses `auxi`.
- CLI: `.heics` output preserves transparency instead of flattening onto white and accepts
  `-s` / `--max-size`.

**Changed**

- Encoder: `search_motion_rect` exits early when the partial SAD plus MVD penalty already
  exceeds the best motion vector cost, speeding up sequence encoding with bit-identical output.

## 0.5.3 — 2026-11-22

Image sequence encoding (`encode_sequence`, P pictures with motion search, merge residuals and
partitions, `.heics` CLI output) and parallel sequence decoding. No breaking change.

**New**

- Image sequences decode in parallel: when a sequence is made of closed groups of pictures,
  `decode_sequence` decodes several groups at once on up to `Options::max_threads` threads,
  with the same frames as one thread. Waiting frames are kept as decoded pictures and the
  number of groups decoded at once is limited to stay under 1 GiB. About 3 times faster on a
  1080p stream with a keyframe every 30 frames (130 instead of 43 frames per second).
- `examples/seqbench.rs` measures sequence decoding.
- Image sequences can be written: `heifer::encode_sequence` encodes frames, each with its
  duration, as a HEIF file with a sequence track (and the first frame as the primary image),
  read back by heifer, libheif and ffmpeg. Every 30th frame is a keyframe; the frames between
  are P pictures predicted from the previous one: skipped blocks, merge candidates, motion
  vectors searched to a quarter sample (AMVP), residuals, or intra blocks, whichever costs
  least. Groups of pictures are encoded in parallel. A 120-frame animation takes 42 KB instead
  of 157 KB with keyframes only.
- `heifer_hevc_enc::encoder::SequenceEncoder` encodes the pictures of a sequence in order (an
  IDR picture every N pictures, P pictures between), sharing one set of parameter sets; each
  picture decodes bit-exactly to the encoder's reconstruction, with heifer and ffmpeg. `heifer_isobmff::writer::SequenceTrack`
  and `HeifWriter::set_sequence` write the track (`moov`, `pict` handler, `ccst`, 32-bit
  chunk offsets unless the file passes 4 GiB).
- CLI: a `.heics` output writes an animated GIF, an APNG or a HEIF sequence as a HEIF image
  sequence, with each frame's duration (transparency is flattened onto white).

**Changed**

- `heifer_hevc_dec::inter` is no longer deprecated: the encoder now uses its `Motion` type. It
  stays internal and hidden from the documentation.
- The SAO filter skips its per-sample checks inside CTBs: 6 to 10% faster on sequences,
  identical output.
- Motion compensation copies reference blocks that cross the picture edge row by row instead
  of sample by sample: about 14% faster on a 1080p sequence with motion near the edges.
- Encoder: image sequences correct a merge prediction with a residual, not only skipped
  blocks, so a good merge candidate with a small correction is used instead of a new motion
  vector. The two 120-frame Nokia samples get 1 to 2% smaller, and every picture still
  decodes bit-exactly to the encoder's reconstruction.
- Encoder: image sequences also code partitioned prediction blocks (2N×N and N×2N), so a unit
  whose two halves move differently is predicted by two motion vectors with one set of headers
  instead of four smaller units. The two 120-frame Nokia samples get 1 to 7% smaller (the
  candle animation 6.9%, the sea 1.1%), and every picture still decodes bit-exactly to the
  encoder's reconstruction.
- Encoder: sequences enable asymmetric partitions (AMP): a unit whose quarter moves
  differently is coded as `2N×nU/D` or `nL/R×2N`, two blocks of different sizes under one set
  of headers. The candle animation gets 0.9% smaller on top of the symmetric partitions, the
  sea 0.2%, and every picture still decodes bit-exactly.

## 0.5.1 — 2026-11-14

Fixes found by fuzzing, uncompressed primary images, and a clearer line between the stable
API and the internals. No breaking change.

**Deprecated**

- Internal modules that nothing outside their crate uses will become private in 0.6:
  `bitreader`, `filters`, `inter`, `refs` and `sei` of `heifer-hevc-dec`, and `transform` of
  `heifer-hevc-enc`. They are now `#[deprecated]` and hidden from the documentation, as are the
  other internal modules of the two codec crates (see "What semver covers" in the README).

**New**

- `Options`, `EncodeOptions` and `DecodeOptions` gain `new()` and `with_*` construction methods,
  which keep compiling when options are added, unlike struct literals.
- `heifer-hevc-dec` re-exports its stable API at the crate root: `Frame`, `DecodeOptions`,
  `decode_picture`, `decode_picture_checked`, `HashCheck`, `DEFAULT_MAX_PIXELS`,
  `SequenceDecoder`, `access_units` and `split_annex_b`.

- Uncompressed images (`unci`, ISO/IEC 23001-17) are decoded as primary images, not only as
  auxiliary ones. One to four unsigned components of 1 to 16 bits are supported, with the
  component, pixel, row, mixed and tile-component interleave modes, tiles, 4:2:2 / 4:2:0 / 4:1:1
  subsampling, and the component, pixel, row and tile alignments. RGB and RGBA are copied straight
  into the RGBA image, mono is replicated, and YCbCr goes through the same colour conversion as
  HEVC. Blocks, floating-point or signed samples, little-endian samples and generic compression
  (`cmpC`) are refused with `Error::Unsupported`.
- `heifer_isobmff::boxes::ComponentDescriptions` parses the `cmpd` box and
  `HeifFile::component_descriptions` returns it (the box stays a `Property::Unknown` in
  `item_properties`, as in 0.5.0), so an uncompressed frame can say what each
  component holds (red, green, blue, alpha, Y, Cb, Cr, mono). Without `cmpd`, the usual component
  counts are assumed (1 mono, 2 mono and alpha, 3 RGB, 4 RGBA).

**Fixed**

- A file with a few junk bytes (fewer than a box header) after its last box was rejected by
  `decode`, `read_sequence` and the other slice-based functions, while `decode_reader` read it.
  Both now ignore such a tail; a box that runs past the end of the file is still an error.
- Coefficient levels are limited to the range the spec allows. A crafted lossless block with
  levels near 2^31 overflowed when added to the prediction: a panic in debug builds, wrong
  samples in release builds. Found by fuzzing.

**Changed**

- The crates' `homepage` is now the in-browser converter, <https://harpelm.github.io/Heifer/>.
- The fuzzing CI seeds the inter decoder with B frame, long-term and RDPCM streams.

## 0.5.0 — 2026-11-07

Image sequences decode completely: P and B frames, long-term references, in display order,
from HEIF files, MP4 tracks and raw HEVC streams, verified against 114 conformance bitstreams.

**Breaking**

- Structs that only describe a decoded file are now `#[non_exhaustive]`, so later fields are
  not breaking changes: `FrameInfo`, `SequenceInfo`, `Regions`, `AuxiliaryImage`,
  `DepthInfo` and `iptc::Dataset` (heifer), `Track` and `Sample` (heifer-isobmff),
  `SliceHeader`, `LongTermRef` and `PredWeight` (heifer-hevc-dec). They can no longer be
  built with a struct literal outside their crate; reading their fields is unchanged.
- Sequence frames are numbered in display order: `read_sequence` lists them in that order and
  `decode_sequence_frame` takes a display index. This only differs from 0.4 for files with B
  frames, which 0.4 did not decode.
- New public fields: `heifer_isobmff::track::Sample::composition_offset`,
  `heifer_hevc_dec::refs::RefPic::long_term` and
  `heifer_hevc_dec::syntax::TransformBlock::explicit_rdpcm`.
- Low-level `heifer-hevc-dec` API: `PocState::next` takes whether the picture can be
  `prevTid0Pic` (a `bool`, from `PocState::is_prev_tid0_candidate`) instead of its
  `TemporalId`; `inter::merge` takes the number of active references of both lists;
  `Reconstructor::set_references` takes both lists and their weights, with a long-term flag
  per picture; `SliceRefs` gains `long_term` and `ColMotion` gains `ref_lt`.

**New**

- P frames: image sequences, bursts and animations decode every frame, not only keyframes.
  `decode_sequence` iterates over the frames, and `decode_sequence_frame` decodes one frame
  from the keyframe before it. The HEVC decoder gains `sequence::SequenceDecoder` (picture
  order count, reference picture sets, decoded picture buffer) and `nal::access_units`.
  Supported: merge and skip modes, AMVP with motion vector scaling, temporal motion vector
  prediction, all partition modes including AMP, 8-tap luma and 4-tap chroma interpolation,
  explicit weighted prediction, constrained intra prediction, and deblocking of inter blocks,
  in 4:0:0 to 4:4:4 up to 12 bits.
- B frames: `RefPicList1`, bi-prediction (default and explicit weights), combined
  bi-predictive and zero merge candidates, and the collocated picture from either list.
- Long-term reference pictures: parsed from the slice header and the SPS, kept in the DPB,
  appended to both reference lists, and handled by AMVP and temporal prediction (no scaling,
  no mixing of short- and long-term vectors).
- Explicit RDPCM (range extension) for inter blocks coded without transform.
- Frames in display order: `decode_sequence` reorders frames by the composition offsets of
  the track (`ctts`), and `read_sequence` gives each frame's display duration.
  `heifer-isobmff`'s `Track` gains `composition_times` and `display_order`.
- Raw HEVC streams in display order: `SequenceDecoder::decode_in_display_order` and `flush`
  implement the output process (C.5.2): reordering, DPB fullness and latency limits,
  `no_output_of_prior_pics_flag` and `pic_output_flag`.
- `image` crate: `HeifSequenceDecoder` reads image sequences as an `image::AnimationDecoder`
  (RGBA 8-bit frames in display order, with their durations).
- CLI: `--frames` extracts every frame of an image sequence (`out-001.png`, ...), or writes
  one looping GIF or APNG when the output is `.gif` or `.apng`; converting a file that holds a
  sequence prints a note, and `heifer info` describes the sequence.
- `check_file_type`: HEVC image sequences (`hevc`, `hevx`) are `Supported`, and generic ones
  (`msf1`) `MayBe`.
- Verified: the 300 frames of the three Nokia sequences, 12 x265 streams (several
  references, AMP, slices, WPP, CTU sizes, open GOP, lossless, transform skip, 10-bit, 4:2:2,
  4:4:4, weighted prediction) and four x265 streams with B frames (pyramid, weighted
  bi-prediction, up to 8 B frames) decode identically to ffmpeg; 114 official conformance
  bitstreams (7,115 pictures) match their MD5 picture hashes, none rejected as unsupported;
  frames and frame counts in display order match ffmpeg on 17 of them.
  `scripts/fetch-conformance.sh` fetches the inter streams, and a test decodes every picture.
- Fuzzing covers the new code, with B frame, long-term and RDPCM seeds.

**Fixed**

- Inter blocks used the intra scaling matrices: P pictures with scaling lists were wrong in
  0.4 (matrixId is 3 to 5 for inter blocks).
- POC derivation took a RADL, RASL or sub-layer non-reference picture as `prevTid0Pic`, so
  some sequences got wrong POCs, hence a wrong display order or missing references.

**Changed**

- Inter prediction is about 14% faster (references read in place, row filters, reused
  buffers).
- A PPS may now arrive before its SPS: it is parsed when the SPS arrives, and again when the
  SPS changes.
- Documentation: image sequences, regions and reading from `Read + Seek` are listed as
  supported in the migration guide.

## 0.4.0 — 2026-11-06

**Breaking**

- `EncodeFormat` gains `Ycbcr422` and `heifer_hevc_enc::encoder::ChromaFormat` gains `Chroma422`;
  both enums are now `#[non_exhaustive]`, so a `match` on them needs a wildcard arm. This is the
  only incompatible change, and future formats will not break code again.

**New**

- Decode-only builds: the encoder is now behind the `encoder` feature, on by default. With
  `default-features = false`, heifer depends only on `heifer-isobmff`, `heifer-hevc-dec` and
  `thiserror`; `image` integration still decodes, and `HeifEncoder` needs `encoder`. CI lints
  and tests this build.
- Supply chain checks: `deny.toml` and CI jobs running `cargo deny` (RustSec advisories,
  licences, banned and duplicate crates, sources) and `cargo audit`.
- Image sequences (`.heics`, bursts, animations), first part: `read_sequence` describes the
  main track of a file (size, frame durations, keyframes) from its `moov` sample tables, and
  `decode_sequence_frame` decodes its keyframes with the intra decoder. Frames predicted from
  other frames need inter prediction, not decoded yet: they return `Error::Unsupported`.
  `heifer_isobmff::track` parses tracks (`tkhd`, `mdhd`, `hdlr`, `stsd` with `hvcC`, `stsz`,
  `stco`/`co64`, `stsc`, `stts`, `stss`, `tref`), with every count checked against its box.
  Tested on the Nokia animation and burst samples, and fuzzed.
- Size targets: `encode_to_size` encodes at a high quality whose file fits in a byte budget, and
  returns the quality used; `fit_to_size` does the same search around any encoder (the CLI uses
  it with the `image` crate encoder and metadata). Quality is searched by dichotomy over the 44
  qualities that give distinct quantization parameters, about six encodes, and every candidate
  is measured, so the result never exceeds the budget. The CLI gains `--max-size` (`-s`), e.g.
  `--max-size 500K`, and prints the quality chosen.
- 4:2:2 encoding: `EncodeFormat::Ycbcr422` (and `--format 422` in the CLI) codes chroma at half
  width and full height, with the Range Extensions profile. Each transform unit has two square
  chroma blocks per component, the bottom one predicted from the top one, with their own coded
  block flags, and chroma prediction modes are mapped by Table 8-3. Odd widths are rounded up
  and cropped with `clap`, as odd sizes already were in 4:2:0. Files decode in heifer, ffmpeg
  and Apple's ImageIO; on a photo at quality 80, 4:2:2 sits between 4:2:0 and 4:4:4 in size and
  quality. The encoder round-trip tests and the encode fuzzer cover it.

- Region annotations (`rgan`, ISO/IEC 23008-12): `read_regions` and `item_regions` return the
  region items of an image with their geometries (points, rectangles, ellipses, polygons,
  polylines, referenced and inline masks) in the coordinates of their reference size, and the
  names, descriptions and tags of their `udes` properties. Referenced masks are linked to their
  mask items through the `mask` references in order, and `Geometry::mask_bit` reads inline masks.
  `HeifWriter::add_region_item` writes them, with 32-bit fields only when a value needs them.
  The syntax follows libheif; every count and size is checked against the data, and the
  container fuzzer now parses them. `heifer info` lists the region items of the primary image.

## 0.3.29 — 2026-11-05

**New**

- IPTC metadata (#4): `read_iptc` and `item_iptc` return the IPTC-IIM block of an image, from its
  `iptc` item (or a `mime` item of type `application/x-iptc`) or, failing that, from the
  IPTC-NAA tag of its EXIF data (`Exif::iptc`). `Iptc::parse` reads it, Photoshop resource
  blocks included: every dataset in file order, repeated ones kept, with `caption`, `keywords`,
  `by_line`, `copyright`, `city`, `country`, `date_created` and `time_created`. Text is UTF-8
  when the block declares it, ISO 8859-1 otherwise. Every length is checked against the data.
  `heifer info` shows the caption, by-line, place and keywords.
- Decoding from a reader (#5): `decode_reader`, `decode_reader_with_options` and
  `read_metadata_reader` take any `Read + Seek`. Only the `ftyp` and `meta` boxes and the data of
  the items the call needs are read: metadata costs a few kilobytes of a large file, and a burst
  or collection reads about a tenth of it for its primary image. The output is identical to the
  slice API, which the decode fuzzer now checks on every input. Reader failures are reported by
  the new `ReadError`.
- The existing API is unchanged: `Metadata` keeps its fields, and the slice functions are as
  before.

**Performance**

- The inverse DCT runs as a partial butterfly: the even and odd coefficients are combined
  through the symmetry of the basis, with about half the multiplications of the matrix product.
  The integer sums are only regrouped, so the output is byte-identical; a test compares both on
  random and extreme inputs for every size, and the 42 hash-verified conformance streams still
  match. About 1% faster single-threaded decoding.
- Context-coded CABAC bins are decoded without a branch: both outcomes are computed and one is
  selected with a mask, the next context state comes from a single table lookup, and the range
  is renormalized by its leading zeros. Context bins are often close to random, so the branch
  was frequently mispredicted. Byte-identical output, checked against the branching form on
  three million bins; about 3% faster single-threaded decoding of 12 MP photos.

## 0.3.28 — 2026-11-05

**Correctness**

- `iref` and `ipma` parsing bound their entry counts by the size of the box, like `iloc`, so a
  crafted count can no longer reserve memory for entries the file does not contain.

**Performance**

- Decoding is about 4% faster on the 13-file single-thread benchmark (872 ms to 838 ms), and 7%
  faster on 12 MP photos (122 ms to 113 ms): the last significant coefficient is located through
  the inverse scan order instead of scanning the whole block, and `residual_coding` reads the 4×4
  raster positions from a table. Output is unchanged.

**Tests**

- The minimized inputs that once crashed the fuzzers are kept in `fuzz/regressions/` and replayed
  at the start of every fuzzing run.

## 0.3.27 — 2026-11-04

**Security**

- `iloc` parsing bounds item and extent counts by the size of the box, rejects extents that
  carry no field at all, and caps the total number of extents. A 42 KB crafted file could
  previously make parsing allocate gigabytes (found by fuzzing).
- An HEVC PPS tile layout that does not match the active SPS is now rejected with
  `Error::Invalid` instead of indexing past the CTB tables or decoding a wrong image (found by
  fuzzing).
- A stream that enables chroma QP offsets with an empty PPS offset list no longer panics; the
  offset is zero (found by fuzzing).

**New**

- The CLI extracts auxiliary images: `--aux` writes every depth map, gain map, alpha plane and
  other refinement next to the output as PNG, while `--depth` and `--gain-map` select one type.
  Files are named after the output (`photo.depth.png`, `photo.gain-map.png`, `photo.alpha.png`,
  `photo.aux-<id>.png`); grayscale planes keep the coded bit depth; a type that the file does not
  have is an error; existing files are never overwritten without `--force`.
- `heifer info` lists the auxiliary images of the primary image with their URN, size and bit
  depth, the depth representation information of depth maps (near and far planes, disparity
  range, representation and reference view), the HDR gain map headroom when the photo carries
  it, and reports IPTC presence.
- `heifer::DepthInfo` and `heifer::DepthRepresentation` read the depth representation
  information SEI message that HEIF depth maps store in the `auxC` subtype bytes.
- `HeifFile::auxiliary_subtypes` returns the `auxC` subtype bytes of an item; `auxC` subtypes are
  parsed when the property container is read.
- `AuxiliaryImage::is_gain_map` recognises Apple and Samsung HDR gain map URNs.

## 0.3.26 — 2026-11-03

**New**

- The CLI codes HEIC in 4:4:4 and lossless: `--format 420|444|lossless`, and `--lossless` as a
  shortcut for `--format lossless`. The default stays 4:2:0.
- `HeifEncoder::with_format` selects the chroma format and coding mode of the `image` crate
  encoder; `HeifEncoder::new` and `with_options` still code 4:2:0.

## 0.3.25 — 2026-11-03

**New**

- Uncompressed auxiliary images (`unci`, ISO/IEC 23001-17) are decoded by `decode_auxiliary`
  and `decode_auxiliary_urn`, including depth maps and alpha planes stored without HEVC
  compression. Single-component unsigned frames of 1 to 16 bits are supported, with component
  alignment, pixel size and row alignment. Uncompressed images remain unsupported as primary
  images: heifer stays an HEVC codec.
- `heifer_isobmff::boxes::UncompressedConfig` parses the `uncC` frame configuration, and
  `HeifFile::uncompressed_config` returns it. `AuxiliaryImage::bit_depth` now reads it when an
  item has no `pixi` property.
- An uncompressed alpha plane is attached to its primary HEVC image by `decode`, like an HEVC
  alpha plane.

## 0.3.24 — 2026-11-03

**New**

- `heifer::auxiliary_images` lists the auxiliary images of the primary image (depth maps, alpha
  planes, gain maps and other refinements): item ID, item type, `auxC` URN, size and bit depth.
  `item_auxiliary_images` does the same for any image item. `AuxiliaryImage::is_depth` and
  `is_alpha` recognise the HEVC and CICP URNs.
- `decode_auxiliary` decodes one by item ID and `decode_auxiliary_urn` by type, with the same
  `Image` and `Options` as `decode`; the latter returns `None` when the file has no such
  auxiliary image. Each auxiliary image gets its own `clap`/`irot`/`imir` transforms, not those
  of the primary image. Monochrome items such as depth maps have their sample replicated in the
  RGB channels.
- `heifer::DEPTH_URN` and `heifer::ALPHA_URN` are the `auxC` types of depth maps and alpha
  planes, also available as `heifer_isobmff::writer` constants.
- `HeifFile::auxiliary_items` returns the auxiliary items of an image, falling back to the first
  input of a derived image as the metadata accessors already do.
- `decode_all` still returns only top-level images: auxiliary planes stay skipped.

## 0.3.23 — 2026-11-03

**New**

- The encoder writes 4:4:4 HEVC (Range Extensions profile 4, `chroma_format_idc = 3`) with
  full-resolution chroma, for graphics, text and other images where 4:2:0 bleeds colours. It
  also writes lossless pictures: GBR 4:4:4 with `matrix_coeffs = 0`, transform and quantization
  bypassed in every coding unit, deblocking and SAO off. Decoding a lossless file reproduces
  every input sample exactly, alpha included. 4:4:4 and lossless files are decoded by Apple's
  ImageIO and by ffmpeg.
- `heifer::EncodeFormat` selects `Ycbcr420` (the default, unchanged), `Ycbcr444` or `Lossless`
  through the new `encode_rgba8_with_format`, `encode_rgb8_with_format`, `encode_with_format`
  and `encode_with_metadata_and_format`. The existing functions keep their signatures and write
  4:2:0.
- The low-level encoder exposes `ChromaFormat`, `CodingOptions`, `encode_picture_with` and
  `encode_picture16_with`, so callers that build planes themselves can choose the chroma format
  and lossless coding. `encode_picture` and `encode_picture16` are unchanged.
- Thumbnails and HDR gain maps stay 4:2:0 lossy. The alpha plane follows the main image and is
  lossless when the main image is.

**Correctness**

- 4:4:4 transform trees now parse each component's coded-block flag against its own parent
  flag, as H.265 §7.3.8.8 requires, instead of against either the luma or the other chroma
  parent. Streams where only one chroma component had a coded parent block now decode without
  desynchronising.

## 0.3.22 — 2026-11-02

**Correctness**

- Range Extensions decoding now honours `cabac_bypass_alignment_enabled_flag`: the CABAC engine is
  aligned before the bypass-coded signs and remaining coefficient levels of each sub-block, as the
  reference decoder does. 16-bit High Throughput 4:4:4 streams decode correctly, including
  `WAVETILES_RExt_Sony_2`. All 66 official conformance bitstreams now decode, 42 of them verified
  against the reference decoder's picture hashes.
- The `extended_precision_processing_flag` tool remains unsupported; no conformance stream enables
  it.

## 0.3.21 — 2026-11-02

**Correctness**

- The colour primaries and transfer characteristics now fall back to the HEVC VUI when an image
  has no `nclx` colour information, for coded items and for the tiles of a grid. HDR HEIC files
  that declare PQ or HLG only in their bitstream are therefore linearised correctly by
  `decode_hdr` instead of being decoded as SDR; `nclx` still takes precedence when present.

## 0.3.20 — 2026-11-02

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
- `decode_hdr` now linearises PQ (SMPTE ST 2084, `nclx` transfer 16) and HLG (BT.2100, transfer
  18) signals. The scale is the BT.2408 reference white of 203 cd/m², so PQ reaches about 49.3
  and HLG about 4.9 at peak; the full range is returned with no tone mapping. Gain maps apply in
  the base image's own transfer, and their headroom multiplies the transfer's. `decode_srgb`
  refuses PQ/HLG with an error that points to `decode_hdr`.

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
