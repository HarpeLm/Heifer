# heifer-cli

Convert HEIC/HEIF photos to and from PNG, JPEG, WebP, TIFF, BMP and GIF, from the command line.
Pure Rust: no libheif, no ffmpeg, nothing to install besides the binary.

```sh
cargo install heifer-cli
```

```sh
heifer IMG_1234.heic photo.jpg               # one file; formats from the extensions
heifer *.HEIC --to jpg -o converted           # many files into a folder
heifer screenshot.png screenshot.heic -q 70   # to HEIC, quality 0-100 (default 80)
heifer info IMG_1234.heic                     # size, tiles, camera, date, metadata
```

- EXIF data and ICC colour profiles are kept, and photos are written upright.
- Existing files are never overwritten unless you pass `--force`.
- Grid photos (iPhone, Samsung…) are decoded and encoded on all cores.

Part of [Heifer](https://github.com/HarpeLm/Heifer), which also provides the
[`heifer`](https://crates.io/crates/heifer) library and an
[in-browser converter](https://harpelm.github.io/Heifer/).

HEVC is covered by patents; you are responsible for any licensing your use requires.

Licensed under MIT or Apache-2.0, at your option.
