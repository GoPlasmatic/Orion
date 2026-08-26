# Channels and connectors

## The channel object

Routing fields sit on the channel itself; every guard goes in a `config`
object beside them.

| Field | Required | Default | Notes |
|---|:--:|---|---|
| `channel_id` | no | auto | Stable identifier |
| `name` | yes | — | Unique per `channel_id`; reachable at `/api/v1/data/{name}` |
| `channel_type` | yes | — | `sync` (caller waits) or `async` (queued; answers `202` with a trace id) |
| `protocol` | yes | — | `rest`, `http`, or `kafka`. **Immutable across versions** |
| `methods` | for `rest`/`http` | — | `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`. Unknown or duplicated is refused |
| `route_pattern` | for `rest`/`http` | — | e.g. `/orders/{id}`; ≤255 chars |
| `topic` | for `kafka` | — | Topic the channel consumes; ≤255 chars |
| `consumer_group` | no | — | Kafka consumer group |
| `workflow_id` | yes | — | The workflow this channel runs. Must be **active** before the channel can activate |
| `priority` | no | `0` | Route-match precedence: priority desc, then segment count desc, then name |
| `tags` | no | `[]` | Selection labels for `?tag=` and package export |
| `config` | no | `{}` | Guards — see below |

`rest` and `http` route identically. An async channel's pattern also serves at
`/{pattern}/async`.

**`route_pattern` grammar.** Must start with `/`; no whitespace, `?`, `#`, or
`%`; no empty segment (no `//`, no trailing `/`). A parameter is a whole
segment written `{name}` matching `[A-Za-z_][A-Za-z0-9_]*`, unique within the
pattern. Captured parameters arrive as `metadata.params`.

```json
{
  "name": "orders",
  "channel_type": "sync",
  "protocol": "rest",
  "methods": ["POST"],
  "route_pattern": "/orders",
  "workflow_id": "order-processing",
  "config": { "timeout_ms": 5000 }
}
```

## config — the guard blocks

**Unknown keys are refused at every level**, including inside a guard block. A
create or update carrying one answers `400` and names it. A *stored* config
that no longer parses is **quarantined** at load: the channel is refused at
every ingress rather than served with a guard silently missing.
`orion-server preflight` names affected channels before an upgrade.

All keys are optional; `{}` is valid.

| Key | Purpose |
|---|---|
| `auth` | Authenticate HTTP callers |
| `rate_limit` | Token-bucket admission rate per caller |
| `backpressure` | Per-node concurrency cap; excess shed with `503` |
| `deduplication` | Idempotency-key replay protection |
| `cache` | Serve repeated identical requests from a response cache |
| `request` | How the HTTP body becomes `data` and `metadata` |
| `response` | Standard envelope, or workflow-controlled status/headers/body |
| `validation_logic` | JSONLogic predicate; falsy rejects with `400` |
| `timeout_ms` | Deadline on workflow execution |
| `origin_allow_list` | Server-side `Origin` check |
| `tracing` | Per-channel override of the global trace-storage policy |

### Which guards run on which ingress

| Guard | HTTP sync | HTTP `/async` | Kafka | `channel_call` |
|---|---|---|---|---|
| `rate_limit` | Yes | Yes | Yes | Yes |
| `auth` | Yes | Yes | No | No |
| `origin_allow_list` | Yes | Yes | No | No |
| `validation_logic` | Yes | Yes | Yes | Yes |
| `deduplication` | Yes | Yes | Yes | No |
| `cache` | Yes | No | No | No |
| `backpressure` | Yes | Yes | Yes | Yes |
| `timeout_ms` | Yes | Yes | Yes (clamped) | Yes |

Order is fixed: rate limit → auth → origin → validation → dedup → cache lookup
→ backpressure. So a rejected request still consumes a rate-limit token, a
replayed idempotency key answers `409` before the cache is consulted, and a
cache hit never takes a backpressure permit.

### `auth`

Covers `POST /api/v1/data/{channel}` and `/async` identically — appending
`/async` is not a bypass. **Without an `auth` block the channel is reachable by
anyone who can reach the port**; `[admin_auth]` protects `/api/v1/admin` only.

`mode` is `api_key`, `hmac`, or `jwt`. For `api_key`, `keys` is an array of
literals or `env://VAR` references; any match authorizes. A `jwt` channel
exposes verified claims at `metadata.auth.claims`.

### `rate_limit`

| Field | Required | Default | Notes |
|---|:--:|---|---|
| `requests_per_second` | yes | — | Steady rate per bucket |
| `burst` | no | `rps / 2 + 1` | Allowance above the steady rate |
| `key_logic` | no | caller identity | JSONLogic computing the bucket key |
| `key_headers` | no | — | Extra headers `key_logic` may read, merged with the built-in set |
| `on_backend_error` | no | `"allow"` | `deny` refuses with `503` when the cluster Redis cannot answer |

**Without `key_logic` this is a per-caller rate, not a throughput cap.** The
key is the client IP over HTTP, the topic for Kafka, the calling channel for
`channel_call` — so `requests_per_second: 100` admits 100/s *per HTTP client*
plus 100/s from Kafka plus 100/s per calling channel. For one shared bucket,
give `key_logic` an expression returning the same value on every ingress.

### `backpressure`

`{ "max_concurrent_per_node": 200 }` — load shedding with `503`, not queueing.
The permit is per channel, drawn by every ingress alike. Per process: N
replicas admit up to N × the cap. A Kafka record that cannot get a permit is
left uncommitted for redelivery rather than shed.

### `deduplication`

| Field | Required | Default | Notes |
|---|:--:|---|---|
| `header` | yes | — | Header carrying the idempotency key |
| `window_secs` | no | `300` | Seconds a key is remembered |
| `connector` | no | in-memory | A cache connector; in cluster mode defaults to the shared Redis |
| `on_backend_error` | no | `"allow"` | `deny` refuses with `503` — never `409`, since the key is unverifiable, not known-duplicate |

### `cache`

Synchronous HTTP ingress only.

| Field | Required | Default | Notes |
|---|:--:|---|---|
| `enabled` | yes | — | `false` disables without removing the block |
| `ttl_secs` | no | `300` | Entry lifetime |
| `cache_key_fields` | no | whole payload | Payload fields forming the key |
| `connector` | no | in-memory | A cache connector |

The key is exactly: channel name, HTTP method, route params, query string (both
order-independent), and the payload (whole, or the named subset). **Request
headers are never part of the key** — a cached entry is shared by every caller
whose method, route, query and payload agree, whatever headers they sent. If a
response varies by anything a header carries, that value must be in the payload
*and* in `cache_key_fields`, or the channel must not cache.

A request resolving **none** of the declared fields bypasses the cache
entirely, logging a warning — it almost always means the names do not match the
payload shape.

### `request`

| Field | Default | Notes |
|---|---|---|
| `body_mode` | `"auto"` | `auto` detects the Orion envelope; `payload` takes the parsed body verbatim |
| `cookies_to_metadata` | — | Named cookies copied to `metadata.cookies.*` |

Under `auto`, an object carrying a top-level `data` or `metadata` key **is** the
envelope — that key becomes the payload and every sibling is discarded.
Anything else is the payload as it stands; an empty body is `{}`.

A `payload`-mode channel needs `orion-cli send --raw`, and accepts no caller
metadata at all.

### `response` — shaping

By default a sync channel answers `200` with `{id, status, data, errors}`
whatever happened. `{"response": {"mode": "shaped"}}` lets the workflow control
the reply by writing to `data._orion.response`:

```json
{ "id": "respond", "name": "Respond",
  "function": { "name": "map", "input": { "mappings": [
    { "path": "data._orion.response.status", "logic": 201 },
    { "path": "data._orion.response.headers",
      "logic": { "Location": { "cat": ["/orders/", { "var": "data.order.id" }] } } },
    { "path": "data._orion.response.body_path", "logic": "data.order" }
  ] } } }
```

| Field | Default | Notes |
|---|---|---|
| `status` | `200` | Out-of-range values fall back to `200` |
| `headers` | `{}` | Subject to the allowlist |
| `body_path` | whole document | Field to send instead; leading `data.` optional |
| `raw` | `false` | Send a string field verbatim — how a channel returns CSV, XML or plain text |

Default allowlist: `content-type`, `location`, `cache-control`, `etag`,
`last-modified`, `retry-after`, `content-language`, `link`.
`response.allowed_headers` **replaces** it. Hop-by-hop headers,
`content-length` and `x-request-id` are refused even when listed. A dropped
header does not fail the request.

**Failures are soft** — a shaped channel whose workflow sets no control block
falls back to the standard envelope rather than erroring. Shaping is sync-only;
`/async` always answers `202`.

---

# Connectors

Unversioned: no draft, no `activate`, no `versions`. `update` replaces the
stored config in place and the engine picks it up on reload.

```json
{
  "name": "payments-api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.stripe.com/v1",
    "auth": { "type": "bearer", "token": "env://STRIPE_API_KEY" }
  },
  "enabled": true,
  "tags": ["payments"]
}
```

Types: `http`, `kafka`, `db`, `cache`, `es`, `smtp`, `storage`.

Connector configs ignore unknown *top-level* fields so old rows keep loading —
but the `operations`, `retry` and `dialect` blocks refuse unknown keys, because
a misspelled control would read as protection while providing none.

## Secrets by reference

Any string in `config` may hold `env://VAR_NAME` instead of a literal. Orion
resolves it each time the connector loads; an unset variable fails the call, or
startup, naming the field. **Credentials never need to be sent to the API or
stored in the database.**

Orion refuses to start on an `ORION_*` variable that is not one of its own
settings, so a secret in that namespace needs the reserved prefix:
`env://ORION_SECRET_STRIPE_API_KEY`.

`vault://<api-path>#<field>` reads from HashiCorp Vault when `VAULT_ADDR` and
`VAULT_TOKEN` are set. `aws-sm://`, `gcp-sm://` and `azure-kv://` are reserved
— a reference using one without a live resolver is refused, never passed
through as a literal credential.

Secrets are **masked on read**, so an exported connector needs its credentials
supplied again on import.

## Authentication

`http` and `es` accept an `auth` object:

| Scheme | Fields |
|---|---|
| `bearer` | `token` |
| `basic` | `username`, `password` |
| `apikey` | `header`, `key` |
| `oauth2` | Managed — Orion acquires, caches, refreshes and persists the token itself. `http` only |

`db` and `cache` carry credentials in their connection URL. `kafka` has none —
broker auth is server configuration.

## Operation gates

Every type carries an `operations` block limiting what workflows may do through
it. Every gate defaults to allowed; a disabled one turns the call into a
validation error naming the operation and connector.

| Type | Gate | Blocks |
|---|---|---|
| `db`, `es` | `read` | `data_query`, `db_read`, `mongo_read`, `mongo_aggregate` |
| `db`, `es` | `insert`, `update`, `delete`, `upsert` | The matching `data_write` / `mongo_write` op |
| `db`, `es` | `raw_write` | `db_write` — raw SQL cannot be classified per operation |
| `cache` | `read` / `write` | `cache_read` / `cache_write` plus channel stores |
| `kafka` | `publish` | `publish_kafka` |
| `http` | `methods` | Any method not on the allow-list |
| `storage` | `presign_get`, `presign_put`, `head` | The matching storage function |

To make a `db` connector fully delete-proof, disable **both**:

```json
{ "type": "db", "connection_string": "env://ORDERS_DB_URL",
  "operations": { "delete": false, "raw_write": false } }
```

An HTTP connector's operation *is* its method, so `methods` is an allow-list:
empty allows everything, but naming even one makes the list exhaustive —
`{"methods": ["GET"]}` locks it to reads.

## `http` config

| Field | Required | Default | Notes |
|---|:--:|---|---|
| `url` | yes | — | Base URL for every request |
| `method` | no | `""` | Default method when the task sets none |
| `headers` | no | `{}` | Default headers |
| `query_params` | no | `{}` | Appended to every request; secret-resolvable and masked |
| `auth` | no | — | See above |
| `retry` | no | `{"max_retries": 3, "retry_delay_ms": 1000}` | GET/PUT/DELETE by default |
| `retry_non_idempotent` | no | `false` | Also retry POST and PATCH |
| `max_response_size` | no | `10485760` | 10 MB; a larger successful response fails the call |
| `allow_private_urls` | no | `false` | Allow private/internal IPs (SSRF protection) |
| `operations` | no | all methods | Method allow-list |

Connector calls run through circuit breakers when the global breaker is
enabled: `orion-cli connectors circuit-breakers` lists state,
`orion-cli connectors reset-breaker <connector>:<channel>` clears one.
