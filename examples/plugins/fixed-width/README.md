# acme.fixedwidth — a plugin, built from source

The source of the codec the [`fixed-width-statement`](../../packages/fixed-width-statement/)
package deploys: two functions, `acme.fixedwidth.parse` and
`acme.fixedwidth.format`, driven by a column spec. Written against
[`orion-plugin-sdk`](../../../crates/orion-plugin-sdk/) in about a hundred
lines, which is the shape a plugin is meant to have — a pure transformation
that already exists as code, is per-customer, and runs on every message.

```
guest/            the Rust crate: a `cdylib` implementing `Plugin` and calling `export_plugin!`
build.sh          builds it for wasm32-unknown-unknown and writes the component beside the manifest
```

The manifest (`plugin.toml`) and the built component live in the package
directory, not here: they are what deploys, and a manifest names its
component relative to itself.

## Rebuild

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-tools
./examples/plugins/fixed-width/build.sh
```

The `plugin-sdk` CI job rebuilds this component and the test fixture from
source on every relevant change and runs the example's offline cases against
the fresh bytes, so the committed component and the source cannot drift.

## Try it without a server

```bash
orion-server test examples/workflow-tests --plugin-dir examples/packages/fixed-width-statement
orion-server dry-run -w examples/packages/fixed-width-statement/workflow.json \
  -i examples/packages/fixed-width-statement/request.json \
  --plugin-dir examples/packages/fixed-width-statement
```

Both run the codec for real in the same sandbox the server uses. See
[Build a plugin](https://docs.goplasmatic.io/build/plugins.html) for the
walkthrough and the [Plugins reference](https://docs.goplasmatic.io/reference/plugins.html)
for the manifest, the ABI and the host's limits.
