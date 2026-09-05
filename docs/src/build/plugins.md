<!-- description: Build an Orion plugin: write a pure JSON → JSON function in Rust with the SDK, describe it in a manifest, test it offline, upload it, and promote it in a package. -->
# Build a Plugin

**Page type:** How-to · **Audience:** Service authors with a codec, parser or calculation that already exists as code

A plugin adds a task function to Orion without a release of Orion: a
WebAssembly component that receives a task's evaluated `input` as one JSON
value and returns one JSON value, which the workflow writes wherever its
`output` says. The sandbox imports nothing — no filesystem, clock, randomness,
sockets, connectors or secrets — so a plugin can only compute, and that is the
whole reason it can be uploaded to a running instance by the same credential
that edits workflows. [Plugins](../concepts/plugins.md) is the concept page;
this one is the build.

## 1. Decide it is a plugin

A plugin is for the hot-path transformation that is awkward in JSONLogic and
already exists in Rust or C: ISO 8583, SWIFT, X12, ASN.1, fixed-width and
copybook records, a domain calculation. Two tests, in order:

- **Does it need I/O?** Then it is not a plugin. Reach it with `http_call`
  from a service you own, or through a [connector](./connectors.md).
- **Is it a field rewrite?** Then it is a `map` with a
  [JSONLogic expression](../reference/expressions.md), which needs no build.

## 2. Write it

A plugin is a Rust `cdylib` on [`orion-plugin-sdk`](https://crates.io/crates/orion-plugin-sdk),
implementing one trait and calling one macro:

```toml
# Cargo.toml
[package]
name = "acme-fixedwidth"
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

struct FixedWidth;

impl Plugin for FixedWidth {
    fn invoke(function: &str, input: Value) -> Result<Value, PluginError> {
        match function {
            "acme.fixedwidth.parse" => parse(&input),
            other => Err(PluginError::caller_input(
                "UNKNOWN_FUNCTION",
                format!("this component exports no '{other}'"),
            )),
        }
    }
}

fn parse(input: &Value) -> Result<Value, PluginError> {
    let record = input["record"].as_str().ok_or_else(|| {
        PluginError::caller_input("BAD_RECORD", "'record' must be a string")
    })?;
    // …split by the spec…
    Ok(json!({ "account": record[..10].trim() }))
}

export_plugin!(FixedWidth);
```

`function` is the registered name — `<plugin>.<label>` as the manifest will
spell it — so one component exports as many functions as it dispatches on.
`input` is every declared field the task set, with the host's evaluation
already done, and never `output`. A refusal is a `PluginError` with a stable
`code` (`^[A-Z][A-Z0-9_]{0,63}$`) and a class: `caller_input` when the input
was wrong for this function and the same input cannot succeed, `internal`
when the plugin itself failed. Neither is retried; the code is what a workflow
can branch on.

The complete version of this codec is
[`examples/plugins/fixed-width/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/plugins/fixed-width),
about a hundred lines.

## 3. Build it

Target `wasm32-unknown-unknown` — not a WASI target, whose standard library
imports WASI and would be refused by the world — and turn the core module into
a component:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-tools
cargo build --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/acme_fixedwidth.wasm \
  -o fixed-width.wasm
```

Keep the component beside the manifest that names it; that is how every tool
finds it.

## 4. Describe it

The manifest declares what the component exports and what each function
accepts, in the same vocabulary every built-in function's field table uses —
so `lint`, the admin API and `GET /admin/functions` all read one thing:

```toml
# plugin.toml
abi = "orion:plugin@1.0.0"
name = "acme.fixedwidth"
version = "0.1.0"
component = "fixed-width.wasm"

[[functions]]
name = "acme.fixedwidth.parse"
description = "Split a fixed-width record into typed fields, by a column spec"

[[functions.input_fields]]
name = "record"
kind = "string"
required = true
template_at = true     # an expression: the engine evaluates it per message

[[functions.input_fields]]
name = "spec"
kind = "array"
required = true         # a literal
```

`template_at = true` is what lets a task write `"record": {"var":
"data.input.line"}` — or any JSONLogic — and have the engine evaluate it
before the guest sees it. `resolvable = true` is the narrower `{"var": …}`
fold. A field is one or the other or literal; `output` is implicit on every
function and never declared. The full key table is in the
[Plugins reference](../reference/plugins.md#manifest).

## 5. Test it offline

A plugin function runs for real in an offline run — it is capability-free, so
there is nothing to stub. Point `dry-run` and `test` at the directory holding
the manifest and component:

```bash
orion-server dry-run -w workflow.json -i input.json --plugin-dir ./plugin
orion-server test ./cases --plugin-dir ./plugin
```

`lint` needs only the manifest, and checks a task's input against it exactly as
the admin API will: a missing required field, a wrong kind, an undeclared
field. A case that names a function whose component is not beside its
manifest fails as `PLUGIN_ARTIFACT_UNAVAILABLE`, never as a silently passing
stub. [Test Workflows Offline](./testing.md) covers the case format.

## 6. Upload and activate it

```bash
orion-cli plugins create -f plugin.toml --tag codecs
orion-cli plugins activate acme.fixedwidth
orion-cli functions list --output json | jq '.data[] | select(.source == "plugin")'
```

The upload validates the manifest, hashes the component, compiles it and
probes every declared function before the draft exists, so a draft is already
known to load. Activation rebuilds the engine, and the functions join the
catalogue; a workflow may name them from then on. The server must run with
`plugins.enabled = true`, and a node whose `[plugins.trust]` names keys needs
`--signature` — see [Trust](../reference/plugins.md#trust).

A new version is a new upload against a new draft (`orion-cli plugins
new-version`, then `update`), and activating it supersedes the previous
version in one transaction. Activation is refused while an active workflow
calls one of the version's functions with an input its schema no longer
accepts, so a renamed field cannot quarantine a running workflow; archiving is
refused while any active workflow calls the plugin at all.

## 7. Promote it

A plugin is the fourth member of a [package](../concepts/packages.md). Export
with the component inlined and the target installs it before the workflow
that calls it:

```bash
orion-server package export -s https://dev.orion.internal --tag codecs \
  --name statements --version 1.0.0 --include-artifacts -o statements.json
orion-server package apply -s https://prod.orion.internal -f statements.json
```

Or compile from source form — a `plugin.toml` in a definition set is compiled
into the artifact the same way. [Promote Between
Environments](../operate/promotion.md) has the order `apply` runs in.

## What the host promises, and does not

Every invocation runs in a fresh instance under the node's ceilings — memory,
wall clock, input and output size, concurrency, and a fuel backstop — and a
failure of any kind writes nothing to the message. Guest strings never reach a
metric label, and a trap's internals go to the operator log, not the client.
What the sandbox does **not** promise is that a wrong answer is caught: it
bounds blast radius, not truth. The [Plugins reference](../reference/plugins.md)
lists the limits and the error table; [Secure an
Instance](../operate/security.md#bound-what-a-plugin-can-do) states the
security model.

## Related

- [Plugins](../concepts/plugins.md): what a plugin is and why it is shaped this way.
- [Plugins reference](../reference/plugins.md): manifest, ABI, limits, errors, trust.
- [`orion-plugin-sdk`](https://crates.io/crates/orion-plugin-sdk): the guest crate.
- [Run the Examples](../getting-started/examples.md): `fixed-width-statement`, the plugin-backed package.
