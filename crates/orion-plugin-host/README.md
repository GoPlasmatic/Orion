# orion-plugin-host

The WebAssembly plugin host of the [Orion](https://github.com/GoPlasmatic/Orion)
runtime: the Wasmtime sandbox and its ceilings, the `orion:plugin` WIT world
(`wit/orion-plugin.wit`, which `orion-plugin-sdk` carries a byte-identical
copy of), the `plugin.toml` manifest, plugin functions as `dataflow-rs`
functions over the engine's field tables, and the Ed25519 trust check.

Wasmtime is linked here and nowhere below, so a change to the engine does not
rebuild the sandbox.

This crate is an implementation detail of `orion-server`. It has no release
cycle of its own and nothing in it is covered by semver.
