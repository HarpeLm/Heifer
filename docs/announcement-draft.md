<!-- Draft announcement for r/rust. Adapt it in your own words before posting. -->

# heifer 0.1: a pure-Rust HEIC decoder (no C, no unsafe)

HEIC is the default photo format on iPhones, but decoding it in Rust has meant binding to
libheif and libde265 (C/C++). heifer is a decoder written entirely in safe Rust.

- **Correct**: identical to ffmpeg on 62/62 comparable HEVC conformance bitstreams, 41 also
  verified against the reference decoder's MD5 hashes; real iPhone, Samsung (200 MP) and Xiaomi
  photos decode correctly.
- **Safe**: `#![forbid(unsafe_code)]`, size limits against malicious files, fuzzed every night.
- **Practical**: grids decoded in parallel (~100 ms for a 12 MP iPhone photo), alpha, rotation,
  EXIF/XMP/ICC, 10/12-bit, and `image::open("photo.heic")` with the `image` feature.
- **Portable**: compiles to WebAssembly — try the browser demo: <demo link>

Not there yet: HDR gain maps, ICC colour management, and an encoder (next step).

Feedback and bug reports with sample files are very welcome: <repo link>

<!-- Disclosure (your choice, recommended): "Developed with the help of an AI assistant;
     every change was validated against conformance streams and real files." -->
