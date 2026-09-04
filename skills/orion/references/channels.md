# Channels and connectors

Read this reference when defining ingress, guards, response behavior, or
external dependencies.

## Channel identity and routing

A channel names one workflow and one ingress shape:

```json
{
  "channel_id": "orders",
  "name": "orders",
  "channel_type": "sync",
  "protocol": "rest",
  "methods": ["POST"],
  "route_pattern": "/orders/{id}",
  "workflow_id": "order-processing",
  "config": {}
}
```

`channel_type` is `sync` or `async`. Protocol is `rest`, `http`, or
`kafka`; it is immutable across versions. HTTP channels require methods and a
route pattern. Kafka channels use a topic and optional consumer group. Route
parameters arrive in `metadata.params`.

The workflow must be active before its channel can activate. Use channel
activation `--dry-run` because it verifies route collisions, workflow
availability, connector dependencies, and whether the stored config can build.

## Admission configuration

Unknown channel config keys are rejected. Important blocks are:

| Block | Purpose |
|---|---|
| `auth` | API key, HMAC, JWT, or inbound OAuth2 sign-in |
| `rate_limit` | Per-key token bucket |
| `backpressure` | Per-node concurrent execution cap |
| `deduplication` | Idempotency replay protection |
| `cache` | Sync HTTP response cache |
| `request` | Body mode and cookie-to-metadata opt-in |
| `response` | Standard or workflow-shaped response |
| `validation_logic` | Pre-workflow predicate |
| `timeout_ms` | Workflow deadline |
| `origin_allow_list` | Server-side HTTP Origin guard |
| `tracing` | Per-channel trace-storage override |

Guard applicability differs by ingress:

| Guard | Sync HTTP | Async HTTP | Kafka | `channel_call` |
|---|:---:|:---:|:---:|:---:|
| rate limit | yes | yes | yes | yes |
| auth / origin | yes | yes | no | no |
| validation | yes | yes | yes | yes |
| deduplication | yes | yes | yes | no |
| response cache | yes | no | no | no |
| backpressure / timeout | yes | yes | yes | yes |

The order is rate limit, authentication, origin, validation, deduplication,
cache lookup, then backpressure. This affects accounting and failure behavior.

Admin authentication protects admin routes only. A data channel without its own
`auth` block is unauthenticated to anyone who can reach the data-plane port.
JWT claims are exposed at `metadata.auth.claims` after verification.

A rate limit without `key_logic` is per inferred caller, not a global channel
throughput cap. A backpressure cap is per process, so aggregate concurrency
scales with replicas. For shared deduplication/rate-limit/cache state, configure
an appropriate cache backend and decide deliberately whether backend errors
fail open or closed.

Cache identity includes channel, method, route params, query, and all or selected
payload fields. Headers are not part of the cache key. Do not cache a response
that varies by identity held only in headers. Orion suppresses caching when a
shaped response sets cookies.

## Request body and metadata

`config.request.body_mode` defaults to `auto`:

- `auto` recognizes an Orion envelope containing top-level `data` or
  `metadata`; otherwise the parsed body is the payload.
- `payload` treats the whole parsed body as data and accepts no caller
  metadata. Use `orion-cli send --raw`.

Only cookies named by `cookies_to_metadata` are copied into metadata. Sensitive
headers are masked. Treat all request-derived values as untrusted even when
they live under `metadata`.

## Response shaping and cookies

A normal sync response uses Orion's envelope. With
`config.response.mode = "shaped"`, the workflow writes controls beneath
`data._orion.response`: status, headers, body selection, and raw mode.

Only configured/allowed response headers are emitted; hop-by-hop headers,
content length, and Orion-owned request IDs remain forbidden. The allowed list
replaces the default rather than extending it.

For multiple safe cookies, set `config.response.cookies` to `true` and
write declarative cookie entries under `data._orion.response.cookies`. Cookie
parts include name, value, path, domain, max age/expires, same-site, HTTP-only,
and secure settings. Orion rejects delimiter/control characters that could
inject attributes or headers. Prefer this over constructing `Set-Cookie`
strings manually.

Shaping is sync-only. Async ingress always returns an acknowledgement and the
final result belongs to its trace.

## Stored configuration values

Use one mechanism for each kind of value:

- `var://name` substitutes a non-secret value declared in server `[vars]`
  into a stored connector or channel config at load time and preserves its
  JSON type.
- `env://NAME` or `vault://...` supplies credentials to connector config and
  the explicitly secret-bearing channel auth fields.
- `{"var":"metadata.vars.name"}` reads a declared var inside runtime logic.
- `{"secret":"name"}` reads a declared secret in permitted expressions.

A channel's `*_logic` fields are not traversed for `var://`; use the JSONLogic
variable form there. An undeclared config var prevents the row from loading.
Do not use legacy dollar-brace environment placeholders in new connector
definitions; text substitution is deprecated and has weaker safety/masking
behavior.

## Connectors

Connectors update in place; follow the mutation command's current reload
behavior and verify engine status afterward. Common types are HTTP, Kafka,
database, cache, Elasticsearch, SMTP, and object storage.

```json
{
  "name": "payments",
  "connector_type": "http",
  "enabled": true,
  "config": {
    "type": "http",
    "url": "var://payments_base_url",
    "auth": { "type": "bearer", "token": "env://PAYMENTS_TOKEN" },
    "operations": { "methods": ["POST"] }
  }
}
```

Let the connector own endpoint/authentication policy and let task inputs carry
request-specific data. HTTP connectors can manage OAuth2 token acquisition,
caching, refresh, and rotation; workflows should not implement that lifecycle.

Operation gates restrict what a workflow may do:

- database/Elasticsearch: read, insert, update, delete, upsert, raw write;
- cache: read and write;
- Kafka: publish;
- HTTP: method allowlist;
- storage: presign-get, presign-put, and head.

For delete-proof database access, disable both structured delete and raw write.
An HTTP method list becomes exhaustive once non-empty.

Connector reads mask secret literals while preserving reference names. An
export containing masked placeholders is not a deployable credential backup.
Inventory and supply all referenced secrets at the destination before package
apply.

Use `orion-cli connectors test <id>` for connectivity and the circuit-breaker
commands for runtime breaker state. Use `orion-server test-connectivity` only
when the intended offline config and command help confirm it is safe to contact
those dependencies.
