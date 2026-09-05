# orion-plugin-sdk

The guest SDK for [Orion](https://github.com/GoPlasmatic/Orion) WebAssembly
plugins: the `orion:plugin` world bindings, the JSON boundary, and the
`export_plugin!` macro that wires a type to the component's export.

A plugin function is a pure JSON → JSON transformation. Orion evaluates the
task's `function.input`, hands it to the component as one JSON value, and
writes the value the component returns at the `output` path the workflow
author chose. The world imports nothing — no filesystem, clock, randomness,
sockets, logging, connectors or secrets — so a plugin is exactly as capable as
its input.

## Write one

```toml
# Cargo.toml
[package]
name = "acme-codec"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
orion-plugin-sdk = "1"

[profile.release]
opt-level = "s"
lto = true
panic = "abort"
strip = true
```

```rust
// src/lib.rs
use orion_plugin_sdk::{Plugin, PluginError, Value, export_plugin, json};

struct Codec;

impl Plugin for Codec {
    fn invoke(function: &str, input: Value) -> Result<Value, PluginError> {
        match function {
            "acme.codec.upper" => {
                let text = input["text"].as_str().ok_or_else(|| {
                    PluginError::caller_input("MISSING_TEXT", "'text' must be a string")
                })?;
                Ok(json!({ "text": text.to_uppercase() }))
            }
            other => Err(PluginError::caller_input(
                "UNKNOWN_FUNCTION",
                format!("this component exports no '{other}'"),
            )),
        }
    }
}

export_plugin!(Codec);
```

## Build it

Target `wasm32-unknown-unknown` — not a WASI target, whose standard library
would import WASI and be refused by the world — then turn the core module into
a component with [`wasm-tools`](https://github.com/bytecodealliance/wasm-tools):

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-tools
cargo build --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/acme_codec.wasm -o plugin.wasm
```

## Describe and upload it

The manifest beside the component names the functions and the fields each
accepts, in the same vocabulary Orion's built-in functions declare:

```toml
# plugin.toml
abi = "orion:plugin@1.0.0"
name = "acme.codec"
version = "0.1.0"
component = "plugin.wasm"

[[functions]]
name = "acme.codec.upper"
description = "Upper-case a string"
output_default_root = "data"

[[functions.input_fields]]
name = "text"
kind = "string"
required = true
template_at = true
```

```bash
orion-cli plugins create -f plugin.toml
orion-cli plugins activate acme.codec
```

The [Plugins reference](https://docs.goplasmatic.io/reference/plugins.html)
covers the manifest, the ABI, the host's limits and how a failure is
reported; [Build a plugin](https://docs.goplasmatic.io/build/plugins.html) is
the author's walkthrough, including offline testing with `orion-server test
--plugin-dir`.

## Versioning

This crate shares the Orion workspace version. The ABI it speaks is the WIT
package version, `orion:plugin@1.0.0`, exposed as `orion_plugin_sdk::ABI` and
required as `abi` in a manifest; a component built against a later world is
refused at upload rather than failing at its first call.
