<!-- description: A plugin is a custom task function compiled to WebAssembly and sandboxed: what it can and cannot do, how it is deployed, and when to write one. -->
<!-- type: concept -->
<!-- last_verified: 2026-10-25 -->

# Plugins

A *plugin* is a custom task function compiled to WebAssembly and run inside Orion, in a sandbox with no access beyond its input and result. It is a definition like a workflow or a channel. It lives in your definition set, deploys inside the same manifest, and serves from the same generation as everything else.

## What a plugin is for

Compiled codecs on the hot path: ISO 8583, SWIFT MT/MX, X12/EDIFACT, ASN.1, fixed-width and copybook records. They are pure, they already exist in Rust or C, and they are per-customer. They run on every message, where a network hop would dominate the cost.

Anything with I/O stays an `http_call` to a service the team owns. A field rewrite is a JSONLogic expression.

## The model

A plugin function is a pure JSON-to-JSON transformation. A task calls it like any other function:

```json
{
  "id": "parse",
  "name": "Parse the ISO 8583 message",
  "function": {
    "name": "acme.iso8583.parse",
    "input": { "message": { "var": "data.raw" }, "spec": "1987", "output": "data.parsed" }
  }
}
```

The function receives the task's evaluated `input` as one JSON object and returns one JSON value. Orion writes that value at the `output` path the workflow author chose. Fields the manifest marks `template_at` are evaluated as JSONLogic by the engine; fields it marks `resolvable` have `{"var": …}` folded in; the rest arrive as written.

It has no other way in and no other way out. The WebAssembly world it implements imports nothing: no filesystem, clock, randomness, sockets, logging, connectors or secrets.

Every plugin function enters the same registry the built-in functions live in. `GET /api/v2/admin/functions` lists it with `source: "plugin"`. Validation checks its input against the manifest's field table, and `orion-server lint` and `clippy` reason about its reads and writes. A retry is always safe: a function with no side channel is pure by construction.

## The definition

In a definition set, a plugin is its `plugin.toml` and the component file it names. `orion-server compile` turns the pair into one [definition](../reference/admin-api/deployments.md#definitions): the manifest as JSON and the component in base64, with an optional detached signature. The upload validates it the way a node loads it, and a deployment names it as a member like any workflow or channel.

The identity of a component is its SHA-256 digest, computed by the server. It is what a generation, a trace and the catalogue all name.

## On a node

`plugins.enabled = false`, the default, preserves the pre-plugin behaviour exactly. With it on, every deployed plugin is compiled once per digest when the node builds a generation. Every invocation runs in a fresh instance under the operator's ceilings. Those cap memory, wall-clock time, request and response size, and concurrency per function. An instruction budget is the backstop. The limits belong to the operator. A manifest requests nothing, and a per-plugin override may only lower a ceiling.

A plugin that fails to load on a node does not stop the node. It fails when its component does not compile, its self-test fails, or the node runs with the sandbox off. The workflows naming its functions are quarantined with the reason, and everything else keeps serving. `/health` reports the plugin as degraded, and `GET /api/v2/admin/engine/status` lists it under `load_issues.plugins`.

## What it costs

Wasmtime and Cranelift join the trusted computing base. The sandbox bounds a plugin's blast radius through memory, time, sizes, concurrency and the absence of ambient authority. It does not make a malicious plugin harmless, and it cannot tell a wrong answer from a right one. The trust root is admin auth, the credential that already deploys workflows and connections; installing a plugin adds no new principal.

## Next steps

- [Build a plugin](../guides/extend/plugins.md): the SDK, the manifest, the build and the upload, end to end.
- [Plugin manifest and ABI](../reference/plugin-manifest.md): the manifest, the ABI, the limits and the error classes.
- [Configuration › Plugins](../reference/configuration/plugins.md): the ceilings, and the trust settings that make signatures mandatory.
- [Deployments](./deployments.md): how a plugin joins the manifest beside the workflows that call it.
