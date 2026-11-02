# Adoption plan: replacing `libheif-rs` across the ecosystem

`heifer` grows one reverse dependency at a time. Today `libheif-rs` (the C bindings) has **31**
direct reverse dependencies on crates.io and `heifer` has **1**. This document tracks them,
ranks them, and describes the standard migration so each conversion is a small, reviewable diff.

The guide for the actual code change is [`MIGRATION.md`](MIGRATION.md). This file is the
*targeting and outreach* plan around it.

## Why crates switch (the pitch per target)

- `libheif` is **LGPL-3**. `heifer` is **MIT OR Apache-2.0**.
- `libheif-rs` needs a **system `libheif`** (`pkg-config`, `vcpkg`, or a vendored C build).
  `heifer` is `cargo add`, and builds for `wasm32-unknown-unknown` and `musl`.
- `heifer` is `#![forbid(unsafe_code)]`; libheif has a long CVE history.
- For HEIC/HEVC still images, `heifer` already has the features these crates use.

## The standard migration (covers ~80% of them)

Almost every one of these crates does exactly one thing with `libheif-rs`:

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

`heifer` returns *packed* interleaved RGBA (no stride to untangle), at 8 bits via
`decode_rgba8`, or `decode()` + `to_rgb8()` for RGB. Metadata comes from
`heifer::read_metadata`.

## Target list (31 crates, by downloads)

`opt` = the `libheif-rs` dependency is behind an optional feature (users opt in). `shape` is
what the crate uses it for. `effort` is the estimated migration size.

| Crate | dl | opt | shape | effort | notes |
|---|---:|---|---|---|---|
| `oculante` | 150k | yes | decode → RGBA | S | `src/image_loader.rs`, single block; well-known viewer |
| `simp` | 63k | yes | decode → RGBA, HDR→8bit | S | `src/image_io/load.rs`; maps `set_convert_hdr_to_8bit` |
| `czkawka_core` | 58k | yes | — | ? | big workspace; inspect member usage |
| `image_sieve` | 48k | **no** | decode → RGBA | S | `src/misc/images.rs`; **hard dep** → biggest structural win |
| `rmg` | 34k | yes | — | ? | inspect |
| `docling-pdf` | 23k | yes | — | ? | inspect |
| `ferrite` | 18k | **no** | decode → RGB | S | `src/image/loaders.rs` |
| `respicta` | 15k | **no** | decode → re-encode (jpeg/webp/png) | S | 3 files under `src/core/heic/` |
| `timewall` | 13k | **no** | decode (multi-image) + metadata + encode | L | Apple dynamic HEIF: several top-level images |
| `kapy` | 12k | **no** | decode + **encode** (EXIF/XMP) | M | `src/processor/heif.rs`; showcase for `encode_with_metadata` |
| `aqiv` | 11k | **no** | decode → RGB | S | tiny loader file |
| `heic-to-dynamic-gnome-wallpaper` | 10k | **no** | decode (multi-image) | M | same shape as `timewall` |
| `photohash` | 8.4k | **no** | decode → RGB + EXIF | S | `src/hash/heic.rs`; uses `vcpkg` build → drops whole C build |
| `rawshift-image` | 6.9k | yes | — | ? | inspect |
| `zune-heic` | 2.3k | no | — | — | **a competing pure-Rust decoder**; dep is likely bench/dev only — skip |
| `imcon` | 1.9k | **no** | decode | S | inspect |
| `squish-core` | 632 | no | — | ? | inspect |
| `crustyimg` | 285 | yes | — | ? | inspect |
| `panimg-core` | 206 | yes | — | ? | inspect |
| `img4avif` | 175 | yes | decode (HEIC→AVIF) | S | only needs decode; strong fit |
| `spatial-maker` | 171 | yes | decode (HEIC→spatial) | ? | inspect |
| `viprs-codecs` | 160 | yes | codec plugin | M | libvips reimplementation |
| `avify` | 136 | yes | decode (HEIC→AVIF) | S | only needs decode |
| `byteshaver` | 92 | yes | decode (batch convert) | S | |
| `inkswatch_colorscan` | 89 | no | decode | S | inspect |
| `scryglass` | 78 | yes | decode | S | viewer |
| `stitchpane-core` | 67 | no | — | ? | inspect |
| `shrinky-rs` | 61 | no | decode (convert) | S | |
| `viprs` | 33 | yes | codec plugin | M | see `viprs-codecs` |
| `photofold` | 23 | yes | decode | S | duplicate merge |
| `tauri-plugin-deeprespond-im` | 12 | yes | — | ? | inspect |

`S` = one file, mechanical. `M` = a few files. `L` = multi-image / encode semantics to reconcile.

## Order of attack

1. **Pilot — `image_sieve`** (or `photohash` if we want a pure-CLI reference).
   `image_sieve` has `libheif-rs` as a **non-optional** dependency, so converting it removes a
   mandatory C build dependency from a 48k-download app: the clearest "before/after" story.
   Its HEIC code is one function. This becomes the **reference conversion**.
2. **The easy, visible ones**: `oculante`, `simp`, `ferrite`, `aqiv`, `photohash`, `respicta`.
   Same recipe; each is one file.
3. **The encode showcase**: `kapy` (decode + re-encode with EXIF/XMP) — proves
   `encode_with_metadata`.
4. **The multi-image cases**: `timewall`, `heic-to-dynamic-gnome-wallpaper` — may require
   exposing enumeration of top-level images (a small, additive API).
5. **The ecosystem multipliers**: `respicta`, `viprs-codecs`, `squish-core`, `panimg-core`,
   `rawshift-image` are libraries — converting them reaches everything built on them.
6. **Endgame — the `image` crate** (7 632 reverse deps): once a few concrete users plus the
   hooks already exist, propose HEIC support built on `heifer`. This is the move that makes the
   switch a non-decision for most of the ecosystem.

## Outreach playbook

For each target, open a focused **issue** first (not a surprise PR), with:

- one sentence on the benefit (licence, no C build, wasm);
- the exact dependency change;
- the migration diff for *their* file(s), based on [`MIGRATION.md`](MIGRATION.md);
- an offer to send the PR.

Then, once invited (or for opted-in maintainers), send the PR with:
- the dependency swap;
- the code change;
- proof: `cargo tree | grep libheif` is empty, tests pass.

Template issue title: *"Drop the libheif C dependency: pure-Rust HEIC via `heifer`"*.

## Success metrics

- `heifer` appears as a reverse dependency of `image` (the big one).
- ≥ 50% of `libheif-rs`'s reverse dependencies migrated.
- `heifer` downloads overtake `heic-rs`, then `libheif-rs`.

## Status tracker

| Crate | Contacted | Migrated | Notes |
|---|---|---|---|
| `image_sieve` | — | — | pilot candidate |
| `oculante` | — | — | |
| `simp` | — | — | |
| `photohash` | — | — | |
| `ferrite` | — | — | |
| `kapy` | — | — | encode showcase |

Update this table as issues and PRs land.
