#!/usr/bin/env bash
# Builds the WebAssembly demo into demo/web/ (open it through a local web server).
# Requires: rustup target add wasm32-unknown-unknown; cargo install wasm-bindgen-cli --version 0.2.129
# Optional: wasm-opt (binaryen) to shrink the binary further.
set -euo pipefail
cd "$(dirname "$0")/../demo"
cargo build --release --target wasm32-unknown-unknown
wasm-bindgen --target web --no-typescript --out-dir web/pkg target/wasm32-unknown-unknown/release/heifer_demo.wasm
if command -v wasm-opt >/dev/null; then
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int -o web/pkg/heifer_demo_bg.wasm web/pkg/heifer_demo_bg.wasm
fi
ls -l web/pkg/heifer_demo_bg.wasm | awk '{printf "wasm size: %.0f KB\n", $5/1024}'
echo "Serve it with: python3 -m http.server -d demo/web 8000   then open http://localhost:8000"
