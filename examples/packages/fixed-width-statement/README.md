# fixed-width-statement

Decode a fixed-width bank-statement line with a **plugin**: `acme.fixedwidth`,
a compiled codec that runs inside Orion's WebAssembly sandbox. The workflow
hands the plugin the line and a column spec, gets typed fields back, and
summarises them with an ordinary `map`. The plugin's source is in
[`examples/plugins/fixed-width/`](../../plugins/fixed-width/); the built
component `fixed-width.wasm` sits beside `plugin.toml` here, so the package
deploys without a wasm toolchain.

The one example that is **not** zero-configuration: the server must run with
`plugins.enabled = true` (or `ORION_PLUGINS__ENABLED=true`). From the
repository root, with such a server on `http://localhost:8080`:

```bash
./examples/deploy.sh fixed-width-statement
```

That uploads and activates the plugin, creates and activates the workflow and
channel, POSTs `request.json` to `POST /api/v1/data/statements`, and prints the
response — `data.statement.amount` is `12500.99`, decoded from the eleven
minor-unit digits in columns 11–21.

The same package is a promotion artifact: `orion-server compile
examples/packages/fixed-width-statement --name statements --version 1.0.0 -o
pkg.json` inlines the component, and `orion-server package apply` installs
the plugin on the target before the workflow that calls it. Offline,
`orion-server test examples/workflow-tests --plugin-dir
examples/packages/fixed-width-statement` runs the codec for real. See
[`examples/README.md`](../../README.md) for the file layout and the full example
list, [Run the Examples](https://docs.goplasmatic.io/getting-started/examples.html)
for the step-by-step walkthrough, and
[Build a plugin](https://docs.goplasmatic.io/build/plugins.html) for how the
codec was written.
