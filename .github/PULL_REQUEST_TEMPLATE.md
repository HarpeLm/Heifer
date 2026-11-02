## What changed

## Why

## How it was tested

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-features`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`
- [ ] `cargo build --workspace --target wasm32-unknown-unknown` (when the change can affect it)

## Notes for reviewers

<!-- Anything worth calling out: a trade-off, a file that needs a closer look, a follow-up. -->
