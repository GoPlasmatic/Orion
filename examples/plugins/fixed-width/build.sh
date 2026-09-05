#!/bin/sh
# Rebuild the acme.fixedwidth component from guest/ and place it beside the
# manifest that names it, in the example package. Needs the
# wasm32-unknown-unknown target (`rustup target add wasm32-unknown-unknown`)
# and `wasm-tools` (`cargo install wasm-tools`). The output is committed; run
# this after changing guest/ or the SDK, and commit the result with the change.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out="$here/../../packages/fixed-width-statement/fixed-width.wasm"
cd "$here/guest"
cargo build --release --target wasm32-unknown-unknown
wasm-tools component new \
  target/wasm32-unknown-unknown/release/acme_fixedwidth.wasm \
  -o "$out"
wasm-tools validate "$out" --features component-model
ls -l "$out"
