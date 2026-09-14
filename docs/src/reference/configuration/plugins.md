<!-- description: The [plugins] settings: enabling the WebAssembly sandbox, the memory, size, time and concurrency ceilings, trust keys and per-plugin overrides. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Plugin settings

Custom task functions as sandboxed WebAssembly Components. Off by default,
and turning it on changes nothing until a plugin is uploaded and activated. Every limit here is the
host's: a plugin requests nothing, and a per-plugin override may only lower a
ceiling, never raise one.

## Synopsis

```toml
[plugins]
enabled = false
cache_dir = ""
max_component_bytes = 16777216
max_memory_bytes = 67108864
max_request_bytes = 1048576
max_response_bytes = 1048576
max_timeout_ms = 5000
max_concurrency_per_function = 64
max_live_instances = 256
fuel_backstop = 100000000000
overrides = []

[plugins.trust]
public_keys = []
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `plugins.enabled` | `false` | `ORION_PLUGINS__ENABLED` | Turn on to load and run plugins on this node. A stored plugin on a node with plugins off quarantines the workflows naming its functions rather than aborting. |
| `plugins.cache_dir` | `""` | `ORION_PLUGINS__CACHE_DIR` | Reserved for an on-disk compile cache; a non-empty value is refused at startup until it exists. |
| `plugins.max_component_bytes` | `16777216` | `ORION_PLUGINS__MAX_COMPONENT_BYTES` | Largest component an upload may carry. |
| `plugins.max_memory_bytes` | `67108864` | `ORION_PLUGINS__MAX_MEMORY_BYTES` | Linear memory per invocation. Also the pooling allocator's per-instance reservation, so the process reserves `max_live_instances × max_memory_bytes` of virtual address space at startup. |
| `plugins.max_request_bytes` | `1048576` | `ORION_PLUGINS__MAX_REQUEST_BYTES` | Largest evaluated `function.input`, serialised, handed to a guest. |
| `plugins.max_response_bytes` | `1048576` | `ORION_PLUGINS__MAX_RESPONSE_BYTES` | Largest JSON a guest may return. |
| `plugins.max_timeout_ms` | `5000` | `ORION_PLUGINS__MAX_TIMEOUT_MS` | Wall-clock ceiling per invocation; the task's own deadline applies too and the shorter wins. |
| `plugins.max_concurrency_per_function` | `64` | `ORION_PLUGINS__MAX_CONCURRENCY_PER_FUNCTION` | Invocations of one function that may run at once; beyond it a task waits until its deadline and fails as a limit. |
| `plugins.max_live_instances` | `256` | `ORION_PLUGINS__MAX_LIVE_INSTANCES` | The instance pool across every function. Must be at least `max_concurrency_per_function`; under-sizing surfaces as instantiation failures under load. |
| `plugins.fuel_backstop` | `100000000000` | `ORION_PLUGINS__FUEL_BACKSTOP` | Instruction budget per invocation — a backstop against a guest that spins, not a contract, because fuel cost moves between Wasmtime versions. Reason in `max_timeout_ms`. |
| `plugins.trust.public_keys` | `[]` | `ORION_PLUGINS__TRUST__PUBLIC_KEYS` | When set, an upload must carry a signature over the component digest by one of these keys, verified again at every load. |
| `plugins.overrides` | `[]` | — | Per-plugin ceilings; see [Overrides](#overrides). |

```toml
[plugins]
enabled = true

[[plugins.overrides]]
id = "acme.iso8583"
timeout_ms = 100
max_memory_bytes = 16777216
```

### Overrides

Each `[[plugins.overrides]]` block names a plugin `id` and any of
`timeout_ms`, `max_memory_bytes`, `max_concurrency`, `max_request_bytes` and
`max_response_bytes`. A value above the host ceiling, a zero, or an `id`
repeated across blocks is refused at startup.

## Related

- [Plugins](../../concepts/plugins.md): what a plugin is.
- [Plugin manifest and ABI](../plugin-manifest.md): the ceilings as a guest sees them, and the trust signature.
- [Write a plugin](../../guides/extend/plugins.md): building and uploading one.
- [Server configuration](./index.md): every section, by what you are configuring.
