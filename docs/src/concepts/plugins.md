<!-- description: A plugin is a custom task function compiled to WebAssembly and sandboxed: what it can and cannot do, its lifecycle, and when to write one. -->
# Plugins

A plugin is a custom task function compiled to WebAssembly and run inside
Orion, in a sandbox that can read nothing but its input and write nothing but
its result. It is a versioned entity like a workflow or a channel: uploaded
and activated through the admin API, promoted in a package, synced across a
cluster by the config epoch, and served from the same runtime generation as
everything else.

## What a plugin is for

Compiled codecs on the hot path: ISO 8583, SWIFT MT/MX, X12/EDIFACT, ASN.1,
fixed-width and copybook records. They are pure, they already exist in Rust or
C, they are per-customer, and they run on every message where a network hop
would dominate the cost. Anything with I/O stays an `http_call` to a service
the team owns; a simple field rewrite is a JSONLogic expression.

## The model

A plugin function is a pure JSON-to-JSON transformation. A task calls it like
any other function:

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

The function receives the task's evaluated `input` as one JSON object — the
fields its manifest marks `template_at` evaluated as JSONLogic by the engine,
`{"var": …}` folded in the ones it marks `resolvable`, the rest as written —
and returns one JSON value, which Orion writes at the `output` path the
workflow author chose. It has no other way in and no other way out: the WebAssembly world it
implements imports nothing — no filesystem, clock, randomness, sockets,
logging, connectors or secrets.

Every plugin function enters the same registry the built-in functions live in,
so `GET /admin/functions` lists it with `source: "plugin"`, workflow validation
checks its input against the manifest's field table, `orion-server lint` and
`clippy` reason about its reads and writes, and a retry is always safe — a
function with no side channel is pure by construction.

## The entity

| Table | Key | Holds |
|---|---|---|
| `plugins` | `(plugin_id, version)` | manifest, digest, status, tags |
| `plugin_artifacts` | `digest` | the component bytes, stored once per digest |

A plugin follows the [entity lifecycle](./lifecycle.md) exactly: integer
versions, one draft per id, active rows immutable, `draft → active →
archived`. Two rules are its own:

- Exactly one version of a plugin is active at a time. Activating a draft
  archives the previously active version in the same transaction, so a
  function name resolves to one digest per generation.
- A plugin cannot be archived or deleted while an active workflow calls one
  of its functions; the refusal is a `409` naming the workflows. The reverse
  holds too: a workflow activates only if every function it names is one the
  node currently dispatches.

An upload validates the manifest, hashes the component, compiles it in the
sandbox and probes every declared function before the draft row exists, so a
draft is already known to load. The identity of a component is its SHA-256
digest, computed by the server; it is what a generation, a trace, a package
and the catalogue all name.

## On a node

`plugins.enabled = false` (the default) preserves the pre-plugin behaviour
exactly. With it on, every active plugin is compiled once per digest at
startup and on reload, and every invocation runs in a fresh instance under the
operator's ceilings — memory, wall-clock deadline, request and response size,
concurrency per function, and an instruction budget as a backstop. The limits
belong to the operator: a manifest requests nothing, and a per-plugin override
may only lower a ceiling.

A plugin that fails to load on a node — no artifact under its digest, a
component that will not compile, a failed self-test, or the sandbox being off
while an active row exists — does not stop the node. The workflows naming its
functions are quarantined with the reason, `/health` reports the plugin as
degraded, `GET /plugins/{id}` says what happened, and everything else keeps
serving. In a cluster every node reads the same rows and compiles locally, so
a rolling deploy stays consistent.

## What it costs

Wasmtime and Cranelift join the trusted computing base. The sandbox bounds a
plugin's blast radius — memory, time, sizes, concurrency, no ambient
authority — but does not make a malicious plugin harmless and cannot tell a
wrong answer from a right one. The trust root is admin auth, the credential
that already reads and writes connector secrets; installing a plugin adds no
new principal.

See the [Plugins reference](../reference/plugins.md) for the manifest, the
ABI, the limits and the error classes, and [Configuration](../reference/configuration.md#plugins)
for the ceilings.
