# Channels and connectors

Read this reference when defining ingress, guards, response behavior, or
the connector files a definition set carries.

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

`channel_type` is `sync` or `async`. Protocol is `rest`, `http`, or `cron`.
HTTP channels require methods and a route pattern. Route parameters arrive in `metadata.params`. A `cron` channel declares none of those
routing fields and must be `async` — see [Cron channels](#cron-channels).

`compile` turns `workflow_id` into the channel member's `requires`, so the
workflow must be in the same definition set. `lint` on the directory and
`deployments plan` both check route collisions, the workflow's presence, its
connections, and whether the config can build. Every channel in a deployment
serves; to stop serving one, leave it out of the set.

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

| Guard | Sync HTTP | Async HTTP | `channel_call` | Cron |
|---|:---:|:---:|:---:|:---:|
| rate limit | yes | yes | yes | no |
| auth / origin | yes | yes | no | no |
| validation | yes | yes | yes | yes |
| deduplication | yes | yes | no | no |
| response cache | yes | no | no | no |
| backpressure / timeout | yes | yes | yes | yes |
| `oauth2_login` | yes | no | no | no |

The order is rate limit, authentication, origin, validation, deduplication,
cache lookup, backpressure, then `oauth2_login`. This affects accounting and
failure behavior. Cron is mostly No because that ingress has no caller at all;
what it refuses, it refuses at authoring time rather than storing and
ignoring.

Admin authentication protects admin routes only. A data channel without its own
`auth` block is unauthenticated to anyone who can reach the data-plane port.
JWT claims are exposed at `metadata.auth.claims` after verification.

A rate limit without `key_logic` is per inferred caller, not a global channel
throughput cap. A backpressure cap is per process, so aggregate concurrency
scales with replicas. For shared deduplication/rate-limit/cache state, configure
an appropriate cache backend and decide deliberately whether backend errors
fail open or closed.

Cache identity includes channel, method, route params, query, and all or selected
payload fields, or the result of `cache.key_logic` over `{data, metadata}` (which
wins over `cache_key_fields`). For a per-user cache on a `jwt` channel, key on
`metadata.auth.claims.sub`: those claims are verified, and a caller cannot supply
`metadata.auth`. Headers are not part of the key unless `key_logic` reads them.
Orion suppresses caching when a shaped response sets cookies.

To cache until the data changes rather than for a fixed TTL, declare
`cache.namespaces: ["ladder"]` and call `cache_invalidate` with
`{"namespaces": ["ladder"]}` from the workflow that writes the data (or
`POST /api/v2/admin/cache/namespaces/ladder/invalidate`, or
`orion-cli cache`). Keep `ttl_secs` as the
staleness ceiling. `cache.coalesce_misses: true` makes concurrent misses for one key
cost one workflow run per node.

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

## Cron channels

A `cron` channel is started by a clock. It must be `channel_type: "async"`, and
its schedule lives in `transport_config` beside `config`:

```json
{
  "channel_id": "nightly-rollup",
  "name": "nightly-rollup",
  "channel_type": "async",
  "protocol": "cron",
  "workflow_id": "order-rollup",
  "transport_config": {
    "schedule": "0 15 2 * * *",
    "timezone": "Asia/Kolkata",
    "payload": { "window": "previous_day" },
    "misfire_policy": "latest",
    "concurrency": { "policy": "forbid" }
  }
}
```

The expression has **six** fields (second first). Five- and seven-field forms
are refused rather than guessed at, as are time-zone abbreviations and an
expression with no occurrence in the next five years. Unknown
`transport_config` keys are refused, so a misspelled `misfire_polcy` is an
error rather than a default left in place forever.

The payload arrives where a request body does — read it with `parse_json` from
`payload` — so a workflow is portable between a route and a schedule. The
schedule adds a reserved `metadata.trigger`: `type` (`cron` or `manual`),
`occurrence_id`, `scheduled_for`, `started_at`, `timezone`, `attempt`,
`singleton_key`. `scheduled_for` is the idempotency key: immutable across
retries, unique per occurrence.

`misfire_policy` is `skip`, `latest` (default) or `catch_up` (which requires
`max_catch_up`, 1–1000); whatever the policy, misses are recorded as one
`skipped_misfire` row carrying the count and range, not one row per instant.
`concurrency.policy = "forbid"` admits one occurrence per `key` (default: the
channel id) at a time across the whole cluster, recording contenders as
`skipped_singleton`. Non-overlap is not exactly-once — a worker that loses its
lease cancels but cannot prove a connector call did not land, so work that must
not be applied twice still needs an idempotent destination.

Everything caller-shaped is refused at authoring: the routing fields,
`config.auth`, `origin_allow_list`, `rate_limit`, `deduplication`, `cache`,
`request`, `response` and `oauth2_login`. The channel is also unreachable over
HTTP and by `channel_call` — running it either way would execute the workflow
outside the ledger and outside its lock. Use `orion-cli cron trigger <channel>`
for a manual run; it takes the same claim and singleton. Read the ledger with
`orion-cli cron status|list|get`, and `cron retry` for another attempt at the
same occurrence.

The node must have `cron.enabled` (the default). A deployed cron channel on a
node with the scheduler off is quarantined and `components.cron` degrades. A
schedule that silently never fires is the failure an operator cannot see, so
Orion makes it loud.

## Configuration values

Use one mechanism for each kind of value:

- `var://name` substitutes a non-secret value declared in server `[vars]`
  into a connector or channel config at load time, and keeps its JSON type.
- `env://NAME` or `vault://...` supplies credentials to connector config,
  the explicitly secret-bearing channel auth fields, and an `oauth2_login`
  block's `client_id`, `client_secret`, `state_secret`, `authorize_url`,
  `token_url` and `redirect_uri`.
- `{"var":"metadata.vars.name"}` reads a declared var inside runtime logic.
- `{"secret":"name"}` reads a declared secret in permitted expressions.

A channel's `*_logic` fields are not traversed for `var://`; use the JSONLogic
variable form there. An undeclared config var keeps the channel or connection
from loading. Do not use dollar-brace environment placeholders in new
connector files; text substitution is deprecated and less safe.

## Connector files

A connector is a file in the definition set. `compile` turns it into a
connection revision over a built-in driver, named by the connector's `name`.
Tasks name it by that id. The types are `http`, `db` and `cache` (per-node
memory).

```json
{
  "id": "payments",
  "name": "payments",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "var://payments_base_url",
    "auth": { "type": "bearer", "token": "env://PAYMENTS_TOKEN" },
    "operations": { "methods": ["POST"] }
  }
}
```

| Connector | Driver it compiles to |
|---|---|
| `http` | `org.orion.http` |
| `db` with `"driver": "postgresql"`, `"mysql"` or `"sqlite"` | `org.orion.postgresql`, `org.orion.mysql`, `org.orion.sqlite` |
| `cache`, backend `memory` | `org.orion.memory-cache` |

- A `db` connector must name its `driver`. The connection string is usually a
  secret reference, so its scheme is not visible at compile time.
- Every `env://` or `vault://` string moves to the connection's
  `secret_locators` at its dotted path. The rest becomes `public_config`.
- A literal credential is refused: a secret-shaped key with a literal value, an
  `Authorization` value, or a password in a URL.
- `enabled` may only be `true` or absent. Tags are not carried.
- A connection revision is immutable. A changed connector file becomes the next
  revision on the next apply; an unchanged one is reused.

The target resolves each reference from its own environment or Vault when the
connection is used. Define every referenced variable on every node before
applying. The deployment and the state database never hold the value.

Let the connector own endpoint and authentication policy, and let task inputs
carry request-specific data. HTTP connectors manage OAuth2 token acquisition,
caching, refresh and rotation; workflows should not implement that lifecycle.

Operation gates restrict what a workflow may do:

- database: read, insert, update, delete, upsert, raw write;
- cache: read and write;
- HTTP: method allowlist.

For delete-proof database access, disable both structured delete and raw write.
An HTTP method list becomes exhaustive once non-empty.

Use `orion-cli connections probe <id>` for connectivity, and the circuit-breaker
endpoints for runtime breaker state. Use `orion-server test-connectivity` only
for the state database; it does not probe connectors.
