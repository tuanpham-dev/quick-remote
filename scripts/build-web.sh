#!/bin/sh
# Rebuilds web/dist: the WASM core, its JS bindings, and the page bundle.
set -eu
cd "$(dirname "$0")/.."
cargo build --release -p qr-wasm --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir web/src/wasm --no-typescript target/wasm32-unknown-unknown/release/qr_wasm.wasm
cd web && npm run -s build
