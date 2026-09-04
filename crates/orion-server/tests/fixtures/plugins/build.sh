#!/bin/sh
# Rebuild fixture.wasm from guest/. Needs the wasm32-unknown-unknown target
# (`rustup target add wasm32-unknown-unknown`) and `wasm-tools`
# (`cargo install wasm-tools`). The output is committed; run this after
# changing guest/ or wit/, and commit the result with the change.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cd "$here/guest"
cargo build --release --target wasm32-unknown-unknown
wasm-tools component new \
  target/wasm32-unknown-unknown/release/orion_fixture_plugin.wasm \
  -o "$here/fixture.wasm"
wasm-tools validate "$here/fixture.wasm" --features component-model
ls -l "$here/fixture.wasm"
