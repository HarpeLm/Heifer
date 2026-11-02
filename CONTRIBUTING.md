# Contributing

Thanks for helping heifer. This guide covers what a good change looks like here, and how to check
it before opening a pull request.

## Scope

heifer decodes and encodes HEIC/HEVC still images. It is deliberately narrow:

- **Pure Rust.** No C or C++ dependency, no system library, no build script that needs one.
  `#![forbid(unsafe_code)]` is a workspace lint, and the workspace must build for
  `wasm32-unknown-unknown`.
- **HEIC/HEVC only.** AVIF, VVC, AVC, JPEG-in-HEIF and JPEG 2000 are other codecs and out of scope.
- **Additive API.** Existing public signatures do not change in a patch release. New behaviour
  goes behind a new function, an option field with a default, or a new type.
- **No panic on untrusted input.** A malformed file returns an `Error`, as described under
  "Robustness" in the README.

If a change is larger than a bug fix, open an issue first so that the design can be discussed.

## Before opening a pull request

Run the same gates as CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
./scripts/fetch-fixtures.sh          # tests that need real files skip when they are missing
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo build --workspace --target wasm32-unknown-unknown
```

The last command needs `rustup target add wasm32-unknown-unknown`. For a change to the HEVC
decoder, also run the conformance suite (see "Conformance" in the README); for a change to a
parser, fuzz the affected target with `./scripts/fuzz.sh 30m <target>` (nightly toolchain and
`cargo install cargo-fuzz`).

## Tests

- Put unit tests next to the code, in a `#[cfg(test)] mod tests`.
- Put end-to-end tests in `crates/heifer/tests/`. Build synthetic files with the encoder when you
  can; do not commit sample photos.
- Test the failure path as well: truncated input, impossible sizes, unsupported colour spaces.
- A decoder change should say what it was compared against (a conformance hash, ffmpeg, ImageIO,
  littleCMS) and the observed difference.
- A test that needs a file from `scripts/fetch-fixtures.sh` must skip, not fail, when it is
  missing, like the existing tests do.

## Style

- `rustfmt` and `clippy` decide formatting and lints. Do not add `#[allow]` without a comment
  saying why.
- Public items need a doc comment: `missing_docs` warns and `cargo doc` runs with `-D warnings` in
  CI.
- Comments explain why a value or an order is what it is (a specification clause, a decoder
  quirk), not what the next line does.
- The decode path is performance-sensitive: keep allocations out of per-pixel loops and mention
  any measured change in the pull request.

## Commits and changelog

- Keep commits focused. Use an imperative subject line ("Fix ...", "Add ...") and a body that
  explains the behaviour change and how it was validated.
- User-visible changes go under `## Unreleased` in `CHANGELOG.md`, in the style of the existing
  entries.
- Update `README.md` or `docs/MIGRATION.md` when a limitation or an API changes.

## Security

Do not open a public issue for a vulnerability. Follow [SECURITY.md](SECURITY.md).

## License

Contributions are accepted under the project's dual MIT OR Apache-2.0 license, without additional
terms.
