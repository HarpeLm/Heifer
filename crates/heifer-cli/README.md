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
heifer screenshot.png screenshot.heic --format 444   # full-resolution chroma
heifer screenshot.png screenshot.heic --lossless     # pixel-exact
heifer IMG_1234.heic photo.png --depth        # with photo.depth.png next to it
heifer portrait.heic portrait.png --aux       # every auxiliary image as PNG
heifer info IMG_1234.heic                     # size, tiles, camera, auxiliary images, metadata
```

- `--format 420|444|lossless` picks the HEIC coding: 4:2:0 by default, 4:4:4 for graphics and
  text, or lossless (4:4:4 GBR, no quantization, so 8-bit input decodes unchanged). `--lossless`
  is a shortcut, and `-q` has no effect on lossless output.

- `--aux` writes every auxiliary image next to the output as PNG: `photo.depth.png`,
  `photo.gain-map.png`, `photo.alpha.png`, and `photo.aux-<id>.png` for other refinements.
  `--depth` and `--gain-map` select one type and fail when the file has none. Depth maps, alpha
  planes and gain maps are written in grayscale at their coded bit depth; other refinements keep
  RGBA. `info` lists auxiliary images with their URN, size and bit depth, the depth representation
  information, the HDR gain map headroom when known, and reports IPTC presence.

- EXIF data and ICC colour profiles are kept, and photos are written upright.
- Existing files are never overwritten unless you pass `--force`.
- Grid photos (iPhone, Samsung…) are decoded and encoded on all cores.

Part of [Heifer](https://github.com/HarpeLm/Heifer), which also provides the
[`heifer`](https://crates.io/crates/heifer) library and an
[in-browser converter](https://harpelm.github.io/Heifer/).

HEVC is covered by patents; you are responsible for any licensing your use requires.

Licensed under MIT or Apache-2.0, at your option.
