# Plugins Reference

The manifest a plugin is uploaded with, the ABI its component implements, how
an invocation is bounded, and what a failure looks like. The concept page is
[Plugins](../concepts/plugins.md); the ceilings are under
[Configuration › Plugins](./configuration.md#plugins); the endpoints are under
[Admin API › Plugins](./admin-api.md#plugins).

## Manifest

Host-owned metadata submitted with the component, as TOML. Its vocabulary is
the same field table every built-in function declares, and nothing more.

```toml
abi = "orion:plugin@1.0.0"
name = "acme.iso8583"
version = "1.2.0"            # informational; Orion assigns the entity version
component = "component.wasm" # relative to this file; read by tooling and the CLI only

[[functions]]
name = "acme.iso8583.parse"
description = "Parse an ISO 8583 message into field-numbered JSON"
category = "transform"
output_default_root = "data"

[[functions.input_fields]]
name = "message"
kind = "string"
required = true
resolvable = true

[[functions.input_fields]]
name = "spec"
kind = "string"
required = true
```

| Key | Rule |
|---|---|
| `abi` | Must be `orion:plugin@1.0.0`, the WIT package version this server speaks. |
| `name` | The plugin id: lowercase reverse-domain with at least two labels (`[a-z][a-z0-9-]*`). `orion.*` and unqualified names are reserved. |
| `version` | The author's own version string. Informational: Orion assigns the entity version. |
| `component` | Path of the component relative to the manifest, with no way out of its directory. Read only by offline tooling and the CLI upload; the server receives bytes and identifies them by digest. |
| `functions[].name` | Must be `<name>.<label>` — a plugin's functions live in its own namespace, which is what keeps them from colliding with a built-in or another plugin. |
| `functions[].category` | Free text, defaulting to `transform`. |
| `functions[].output_default_root` | `data`, `temp_data` or `metadata`: where the result lands when a task names no `output`. Absent means a task must name one. |
| `functions[].input_fields[].kind` | `string`, `number`, `bool`, `object`, `array` or `any`. |
| `functions[].input_fields[].required` | Refused at create time when absent. |
| `functions[].input_fields[].template_at` | `true` makes the field's value a JSONLogic expression: compiled once when the workflow loads, evaluated per message, and the guest receives the result — the same `template_at: [""]` a built-in's field declares in the catalogue. |
| `functions[].input_fields[].resolvable` | `{"var": …}` nodes in the field are folded against the message before the guest sees it. Not combinable with `template_at`, which already evaluates `var`. |

Unknown keys, an unsupported `abi`, an invalid kind, a reserved name and a
declared `output` all reject the upload, with a path into the document.
`output` is implicit on every function: a task may always name where its
result goes. A field is evaluated (`template_at`), folded (`resolvable`) or
literal; `secret_at` does not exist, and a `{"secret": …}` node is refused at
create time anywhere in a plugin task's input — a plugin never sees key
material.

Activating a version checks every active workflow that calls its functions
against the schema it declares: a field renamed or newly required between
versions is refused with a `409` naming the workflow, while the previous
version keeps serving. (A workflow that reaches the engine with an input its
plugin's table refuses — through an import, say — is quarantined when it
loads, with the mismatch in the reason.)

## ABI

```wit
package orion:plugin@1.0.0;

interface functions {
  enum error-class { caller-input, internal }
  record plugin-error { code: string, class: error-class, message: string }

  /// `function` is the registered name; `input` is the evaluated
  /// `function.input` as JSON. Returns the JSON value written at `output`.
  invoke: func(function: string, input: string) -> result<string, plugin-error>;
}

world plugin {
  export functions;
}
```

The world imports nothing. The guest receives the whole evaluated input
object — every declared field the task set, `output` excluded — and returns
one JSON value. One component may export many functions and dispatch on the
name. A guest `code` must match `^[A-Z][A-Z0-9_]{0,63}$`; its `message` is
capped and prefixed with the function name before a client sees it.

## Limits

Every invocation runs in a fresh instance under the node's ceilings, each
narrowed by the plugin's `[[plugins.overrides]]` block if one exists:

| Ceiling | What it bounds |
|---|---|
| `max_memory_bytes` | Linear memory. A growth past it is refused; a guest that then aborts fails as a limit. |
| `max_timeout_ms` | Wall clock. The epoch deadline traps the guest; a wall-clock timeout around the call is the belt to that brace. The task's own deadline applies too and the shorter wins. |
| `max_request_bytes` | The serialised input, checked before the guest runs. |
| `max_response_bytes` | The returned JSON, checked before it is parsed. |
| `max_concurrency_per_function` | Invocations of one function at once; beyond it a task waits for a permit until its deadline. |
| `fuel_backstop` | An instruction budget, sized well above what the clock admits; it catches a runaway the clock somehow missed. |

Compiling happens once per digest per process, on a blocking thread, never on
a request. Instantiation is microseconds through a pooling allocator whose
size is `max_live_instances`.

## Errors

| Source | Condition | Class | Retried |
|---|---|---|---|
| guest | `caller-input` | `CallerInput` | no |
| guest | `internal` | `Backend` | no |
| host | fuel, memory, request or response size, no permit, instance pool full | `Limit` | no |
| host | epoch or wall-clock deadline | `Timeout` | yes — a pure function retries for free |
| host | trap, panic, result that is not JSON | `Backend` | no |

A failure writes nothing. Wasmtime's internals and trap text go to the
operator log with the plugin id, version, digest, function and trace id; a
client sees the category and, for a guest error, the code and capped message.
`orion_plugin_failures_total{category}` counts them by the same categories —
see [Metrics › Plugins](./metrics.md#plugins).

## Health

`GET /plugins/{id}` reports this node's load state under `health`:
`loaded` (with the compile time), `failed` (with the stage and reason),
`disabled` (the sandbox is off on this node) or `inactive` (the version is not
the one this node's generation carries). `/health` carries a `plugins`
component that is `ok`, `degraded` when an active plugin did not load, or
`disabled`; the admin-only detail lists every loaded version and every
failure. The stages a failure names are `manifest`, `signature`, `artifact`,
`compile`, `link`, `size` and `self_test`.

## Trust

Installing a plugin needs the admin credential — the one that already reads
and writes connector secrets — so a plugin adds no new principal. The
optional hardening on top is a **detached Ed25519 signature over the
component digest**, configured under
[`[plugins.trust]`](./configuration.md#plugins):

```toml
[plugins.trust]
public_keys = ["MCowBQYDK2VwAyEA…"]   # raw 32-byte keys, base64
```

When any key is configured, an upload must carry `signature`: the base64
Ed25519 signature over the ASCII digest string (`sha256:<64 hex>`) by one of
those keys. The digest is what is signed, not the bytes, so a release
pipeline signs the identity every other surface already names and never needs
the component in memory. An upload with no signature is refused at
`signature` with `REQUIRED`; one that does not verify, with `INVALID`. The
signature is stored on the version and **verified again by every node that
loads it** — a row that arrived through an import on a node without keys, or
a peer's activation, is checked by the node that runs it, and one that fails
is a `signature` load issue that quarantines the workflows naming its
functions. A node with no keys configured checks nothing and stores whatever
the upload sent.

Signing with OpenSSL, given an Ed25519 private key in `signer.pem`:

```bash
digest=$(printf 'sha256:%s' "$(sha256sum plugin.wasm | cut -d' ' -f1)")
printf '%s' "$digest" | openssl pkeyutl -sign -rawin -inkey signer.pem | base64 -w0 > plugin.sig
openssl pkey -in signer.pem -pubout -outform DER | tail -c 32 | base64 -w0   # the value for public_keys
orion-cli plugins create -f plugin.toml --signature plugin.sig
```

`orion-cli plugins create --signature <file>` reads the base64 text from a
file; over the API, `signature` is a field of the JSON body or a part of the
multipart form.

## Packages and offline tooling

A plugin is the fourth member of a [package](../concepts/packages.md).
`package export` resolves every plugin function a selected workflow calls to
the active version and digest serving it — `GET /workflows/{id}/dependencies`
reports them under `plugins` — and carries each under `plugins[]`; with
`--include-artifacts` the component travels inline, so `apply` installs and
activates it on a target that has never seen it before any workflow that
calls it. Without the component the target must already hold the digest, and
`plan` says so. A plugin the source no longer serves at that digest goes to
`requires.plugins`, which `plan` checks the target has active.
[`compile`](./cli.md#compile) does the same for a `plugin.toml` in a
definition set, inlining the component beside it.

Offline, [`lint`](./cli.md#lint), `clippy` and `compile` read the manifests in
the set and in every `--plugin-dir`, and validate a plugin task's input
against the manifest's field table exactly as the admin API validates it
against the active plugin. A function of a plugin no manifest accounts for is
reported as unverifiable — a note, never an error — because only the serving
instance can answer for it. [`dry-run`](./cli.md#dry-run) and
[`test`](./cli.md#test) go further: given `--plugin-dir` with the components
beside the manifests, plugin functions **run for real** in the same sandbox
the server uses, under the host's default ceilings. They are never stubbed —
a plugin is capability-free, so the real answer is always available — and a
case naming a function whose component is absent fails as
`PLUGIN_ARTIFACT_UNAVAILABLE` rather than passing on a stand-in.

`fmt` reads no manifest: one style everywhere is its whole value, so a plugin
task's `input` keeps its author order. `clippy` has no plugin-specific rule;
a plugin's writes are proven structurally through `output`, as `crypto`'s are.

## Performance

What a plugin invocation costs per request is a fresh store, an
instantiation from the pre-linked component, one JSON serialisation in and
one parse out. Scenario H of the [benchmark
suite](https://github.com/GoPlasmatic/Orion/tree/main/crates/orion-server/tests/benchmark)
measures exactly that: the test fixture's `identity` function on the hot path
against the same rewrite as a JSONLogic `map`, both behind `parse_json`, so
the difference between the two rows is the sandboxed call. It runs in the
default set (`bench.sh plugin` runs it alone) and its rows are recorded with
every release's benchmark record under
`crates/orion-server/tests/benchmark/results/`, on the dedicated hardware
`RELEASING.md` requires for published numbers. On a development build the two
rows are within a few percent of each other; treat that as the shape of the
answer, not the answer — the release record is.

Two things move the number more than the sandbox does. A component compiles
**once per digest per process**, on a blocking thread, when its version
loads; that cost never sits on a request. And `max_live_instances` bounds
how many invocations can be in flight at once across every function, so a
pool sized below the concurrency the node actually sees surfaces as
instantiation failures under load rather than as latency.
