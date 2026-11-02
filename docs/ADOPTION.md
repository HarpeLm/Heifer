# Adoption plan: replacing `libheif-rs` across the ecosystem

`heifer` grows one reverse dependency at a time. Today `libheif-rs` (the C bindings) has **31**
direct reverse dependencies on crates.io and `heifer` has **1**. This document tracks them,
ranks them, and describes the standard migration so each conversion is a small, reviewable diff.

The guide for the code change itself is [`MIGRATION.md`](MIGRATION.md). This file is the
*targeting and outreach* plan around it.

## The pitch, per target

- `libheif` is **LGPL-3**. `heifer` is **MIT OR Apache-2.0**.
- `libheif-rs` needs a **system `libheif`** (`pkg-config`, `vcpkg`, or a vendored/`cmake` build).
  `heifer` is `cargo add`, and builds for `wasm32-unknown-unknown` and `musl`.
- `heifer` is `#![forbid(unsafe_code)]`; libheif has a long CVE history.
- For HEIC/HEVC still images, `heifer` already has the features these crates use.

## The standard migration (covers ~80% of them)

Almost every crate does exactly one thing with `libheif-rs`:

```rust
// before
let lib_heif = LibHeif::new();
let ctx = HeifContext::read_from_bytes(bytes)?;
let handle = ctx.primary_image_handle()?;
let image = lib_heif.decode(&handle, ColorSpace::Rgb(RgbChroma::Rgba), None)?;
let planes = image.planes();
let p = planes.interleaved.unwrap();
let (data, w, h, stride) = (p.data, p.width, p.height, p.stride);
// ... copy `data` row by row using `stride` ...
```

```rust
// after
let image = heifer::decode_rgba8(bytes, &heifer::Options::default())?;
let (data, w, h) = (&image.data, image.width, image.height);   // packed, stride == w * 4
```

`heifer` returns *packed* interleaved RGBA (no stride to untangle), 8-bit via `decode_rgba8`,
or `decode()` + `to_rgb8()` for RGB. Metadata comes from `heifer::read_metadata`.

## Highest-conviction targets: they already want out of libheif

These crates gate HEIC behind an optional feature and document, in their own comments, that
`libheif` is a reluctant C/LGPL dependency. The migration removes the caveat entirely — the
narrative writes itself.

| Crate | dl | Their own words (from `Cargo.toml`) |
|---|---:|---|
| `crustyimg` | 285 | *"Requires a system libheif (≥ 1.17) at build and run time — `brew install libheif` / `apt-get install libheif-dev`"*; explicitly avoids `embedded-libheif` to dodge *"the LGPLv3 §4 static-relink obligation"*; notes *"the mature pure-Rust HEIC decoders are AGPL"*. **wasm-focused, pure-Rust policy.** It gates HEIC for AGPL *and* HEVC patents — `heifer` (MIT/Apache, no C) removes the dependency/AGPL half; the patent gate is orthogonal. |
| `byteshaver` | 92 | *"`embedded-libheif` builds the vendored libheif 1.23.1 statically … codec decode plugins … via `pkg-config`"* — a whole vendored C build to delete. |
| `img4avif` | 175 | *"⚠️ EXPERIMENTAL: HEIC/HEIF support requires the libheif C library at link time."* / *"LICENSE NOTE: libheif is LGPL-licensed."* |
| `avify` | 136 | `heic = ["dep:libheif-rs"]` — HEIC is the only heavy native input. |
| `scryglass` | 78 | *"HEIC/HEIF needs the system libheif C library, so it's opt-in."* |
| `photofold` | 23 | *"Needs: `brew install libheif pkg-config`."* |
| `spatial-maker` | 171 | `heic = ["dep:libheif-rs"]`, everything else pure. |
| `timewall` | 13k | `libheif-rs` non-optional, and **every other dependency is pure Rust** — the purest real target, and literally "Apple dynamic HEIF on Linux". Needs enumerating multiple top-level images (see API gaps). |

## Full target list (31 crates, by downloads)

`opt` = `libheif-rs` is behind an optional feature. `purity` = other native deps present.

| Crate | dl | opt | shape | purity | effort |
|---|---:|---|---|---|---|
| `oculante` | 150k | yes | decode → RGBA | `lcms2`, `libavif` | S |
| `simp` | 63k | yes | decode → RGBA, HDR→8bit | `libwebp` | S |
| `czkawka_core` | 58k | yes | — | ? | ? |
| `image_sieve` | 48k | **no** | decode → RGBA | `ffmpeg-next` | S |
| `rmg` | 34k | yes | — | ? | ? |
| `docling-pdf` | 23k | yes | — | ? | ? |
| `ferrite` | 18k | **no** | decode → RGB | `image/avif-native`, `jpegxl-rs` | S |
| `respicta` | 15k | **no** | decode → re-encode | `libwebp-sys`, `magick_rust` | S |
| `timewall` | 13k | **no** | decode (multi-image) + metadata + encode | **pure Rust** | L |
| `kapy` | 12k | **no** | decode + **encode** (EXIF/XMP) | `magick_rust` | M |
| `aqiv` | 11k | **no** | decode → RGB | `magick_rust`, `jpegxl-rs` | S |
| `heic-to-dynamic-gnome-wallpaper` | 10k | **no** | decode (multi-image) | ? | M |
| `photohash` | 8.4k | **no** | decode → RGB + EXIF | `turbojpeg`, `rusqlite` | S |
| `rawshift-image` | 6.9k | yes | dedicated `rawshift-image-heic` crate | many | ? |
| `zune-heic` | 2.3k | no | — | — | **skip** (competing decoder; dep is likely bench/dev) |
| `imcon` | 1.9k | **no** | decode | ? | S |
| `squish-core` | 632 | no | — | ? | ? |
| `crustyimg` | 285 | yes | decode | **pure Rust** | S |
| `panimg-core` | 206 | yes | decode | `lcms2` | S |
| `img4avif` | 175 | yes | decode (HEIC→AVIF) | **pure Rust** | S |
| `spatial-maker` | 171 | yes | decode | **pure Rust** | S |
| `viprs-codecs` | 160 | yes | codec plugin | ? | M |
| `avify` | 136 | yes | decode (HEIC→AVIF) | **pure Rust** | S |
| `byteshaver` | 92 | yes | decode | `mozjpeg`, `jpegxl-src` | S |
| `inkswatch_colorscan` | 89 | no | decode | ? | S |
| `scryglass` | 78 | yes | decode | `ffmpeg-next` (opt) | S |
| `stitchpane-core` | 67 | no | — | ? | ? |
| `shrinky-rs` | 61 | no | decode (convert) | `image/avif-native`, `libheif-sys` | S |
| `viprs` | 33 | yes | codec plugin | ? | M |
| `photofold` | 23 | yes | decode | `rusqlite` | S |
| `tauri-plugin-deeprespond-im` | 12 | yes | — | ? | ? |

`S` = one file, mechanical. `M` = a few files. `L` = multi-image / encode semantics to reconcile.
`?` = not yet inspected.

## Order of attack

1. **Pilot — `crustyimg`** (a pure-Rust, wasm-focused CLI that already documents the LGPL/C
   problem with libheif). Converting it removes its one C dependency and the `brew install
   libheif` caveat. Its HEIC support is a gated feature and one decode block. This becomes the
   **reference conversion**. Runner-ups with the same "we wish it weren't C" motivation:
   `byteshaver`, `img4avif`, `avify`, `scryglass`, `photofold`.
2. **The easy, visible ones**: `oculante`, `simp`, `ferrite`, `aqiv`, `photohash`, `respicta`,
   `panimg-core`, `inkswatch_colorscan`. Same recipe; each is one file.
3. **The encode showcase**: `kapy` (decode + re-encode with EXIF/XMP) — proves
   `encode_with_metadata`.
4. **The multi-image cases**: `timewall`, `heic-to-dynamic-gnome-wallpaper` — need the API gap
   below.
5. **The ecosystem multipliers**: `respicta`, `viprs-codecs`, `squish-core`, `panimg-core`,
   `rawshift-image` are libraries — converting them reaches everything built on them.
6. **Endgame — the `image` crate** (7 632 reverse deps): once concrete users exist plus the
   hooks, propose HEIC support built on `heifer`. This makes the switch a non-decision for most
   of the ecosystem.

## `heifer` API gaps surfaced by these targets

- **Enumerate top-level images** (`HeifFile::top_level_image_ids()`, or `heifer::decode_all`)
  for `timewall` and `heic-to-dynamic-gnome-wallpaper`. Today possible but low-level
  (`file.items` + `decode_item`). Additive, small.
- **File-type sniffing** (`check_file_type` equivalent) for `timewall::validate_file`. Trivial.
- Everything else (decode to RGB/RGBA, EXIF/XMP/ICC, encode with metadata, HDR) is already
  covered.

## Outreach playbook

For each target, open a focused **issue** first (not a surprise PR):

- one sentence on the benefit (licence, no C build, wasm);
- the exact dependency change;
- the migration diff for *their* file(s), based on [`MIGRATION.md`](MIGRATION.md);
- an offer to send the PR.

Template issue title: *"Drop the libheif C dependency: pure-Rust HEIC via `heifer`"*.

Then, once invited, send the PR: dependency swap, code change, and proof
(`cargo tree | grep libheif` empty, tests pass).

## Success metrics

- `heifer` appears as a reverse dependency of `image` (the big one).
- ≥ 50% of `libheif-rs`'s reverse dependencies migrated.
- `heifer` downloads overtake `heic-rs`, then `libheif-rs`.

## Status tracker

| Crate | Contacted | Migrated | Notes |
|---|---|---|---|
| `crustyimg` | — | — | pilot candidate |
| `byteshaver` | — | — | |
| `img4avif` | — | — | |
| `avify` | — | — | |
| `scryglass` | — | — | |
| `photofold` | — | — | |
| `timewall` | — | — | needs multi-image API |
| `kapy` | — | — | encode showcase |
| `oculante` | — | — | reach |
| `image_sieve` | — | — | reach |

Update this table as issues and PRs land.
