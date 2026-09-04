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
| `functions[].input_fields[].resolvable` | `{"var": …}` nodes in the field are folded against the message before the guest sees it. |

Unknown keys, an unsupported `abi`, an invalid kind, a reserved name and a
declared `output` all reject the upload, with a path into the document.
`output` is implicit on every function: a task may always name where its
result goes. `template_at` and `secret_at` are not accepted — a plugin never
sees key material, and a plugin field is folded or literal.

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
failure.
