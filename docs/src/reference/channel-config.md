# Channel Configuration

A channel's `config` object (stored as `config_json`) declares every per-channel guard: authentication, rate limiting, backpressure, deduplication, caching, validation, response shaping, timeouts, origin checks, and tracing. Routing fields (`protocol`, `route_pattern`, `methods`) are siblings of `config` on the channel object; [Routing & protocol](#routing--protocol) covers them first.

> [!WARNING]
> Unknown keys are refused at every level of `config`, including inside guard blocks. A create or update carrying one answers `400` and names the key. A stored config that no longer parses is **quarantined** at load: the channel is refused at every ingress rather than served with a guard silently missing. `orion-server preflight` names affected channels before an upgrade. See the [Glossary](./glossary.md) for the quarantine term.

## Key index

All `config` keys are optional. An empty `{}` is valid: the channel then runs with no guards of its own.

| Key | Purpose |
|---|---|
| [`auth`](#authentication) | Authenticate HTTP callers of this channel. |
| [`rate_limit`](#rate-limiting) | Token-bucket admission rate per caller. |
| [`backpressure`](#backpressure) | Per-node concurrency cap; excess is shed with `503`. |
| [`deduplication`](#deduplication) | Idempotency-key replay protection. |
| [`cache`](#response-caching) | Serve repeated identical requests from a response cache. |
| [`request`](#request-body) | How the HTTP request body becomes `data` and `metadata`. |
| [`response`](#response-shaping) | Standard envelope, or workflow-controlled status, headers, and body. |
| [`validation_logic`](#validation) | JSONLogic predicate; a falsy result rejects the request with `400`. |
| [`timeout_ms`](#timeouts) | Deadline on workflow execution. |
| [`origin_allow_list`](#cors--origins) | Server-side `Origin` header check. |
| [`tracing`](#tracing-override) | Per-channel override of the global trace-storage policy. |

**Field-table legend.** In the Required column, `yes`/`no` are absolute; a protocol or mode name (`rest`, `hmac`, …) means the field is required exactly when that protocol or mode applies. `—` in Default means the field has no value until you set one. The Guards-by-ingress matrix above answers a different question and answers it in prose — `Yes`/`No`, not a field value.

## Guards by ingress

A channel is reachable on up to four ingresses. Each guard runs on the ingresses marked Yes:

| Guard | HTTP sync | HTTP `/async` | Kafka | `channel_call` |
|---|---|---|---|---|
| `rate_limit` | Yes | Yes | Yes | Yes |
| `auth` | Yes | Yes | No | No |
| `origin_allow_list` | Yes | Yes | No | No |
| `validation_logic` | Yes | Yes | Yes | Yes |
| `deduplication` | Yes | Yes | Yes | No |
| `cache` | Yes | No | No | No |
| `backpressure` | Yes | Yes | Yes | Yes |
| `timeout_ms` | Yes | Yes | Yes¹ | Yes |

¹ Clamped to a transport ceiling — see [Timeouts](#timeouts). Every No cell is deliberate; the owning section below states why.

<details><summary>Order of application</summary>

Guards run in a fixed order: rate limit → auth → origin allow-list → validation → deduplication → cache lookup → backpressure. Three consequences:

- A rejected request (bad origin, failed validation) still consumes a rate-limit token.
- A replayed idempotency key answers `409` before the cache is consulted.
- A cache hit never consumes a backpressure permit, and a request shed by backpressure releases its idempotency claim.

</details>

## Routing & protocol

These fields sit on the channel object itself, beside `config`. They decide how requests reach the channel; [Route Resolution](./data-api.md#route-resolution) in the Data API reference specifies how a request path resolves to a channel.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `channel_type` | string | yes | — | `sync` (the caller waits for the result) or `async` (queued; answers `202` with a trace id). Case-insensitive. |
| `protocol` | string | yes | — | `rest`, `http`, or `kafka`. Case-insensitive. Immutable across versions. |
| `methods` | array of strings | `rest`, `http` | — | HTTP methods the route answers. Valid values: `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`. An unknown or duplicated method is refused. |
| `route_pattern` | string | `rest`, `http` | — | Path pattern, for example `/orders/{id}`. Grammar below. At most 255 characters. |
| `topic` | string | `kafka` | — | Kafka topic the channel consumes. At most 255 characters. |
| `consumer_group` | string | no | — | Kafka consumer group name. At most 255 characters. |
| `priority` | number | no | `0` | Route-match precedence. Routes match by priority descending, then segment count descending, then channel name — deterministic on every node. |

`rest` and `http` route identically: both must declare `methods` and `route_pattern`, both register in the route table, and both stay reachable by name at `/api/v1/data/{name}`. An async channel's pattern serves at `/{pattern}/async`, whatever its `channel_type`. A `kafka` channel registers its `topic` as a consumer at startup and on engine reload; config-file topic mappings take precedence over channel-declared ones (see [Kafka Consumer Configuration](./configuration.md#kafka)).

**`route_pattern` grammar.** The pattern must start with `/`. It must not contain whitespace, `?`, `#`, or `%`. No segment may be empty (no `//`, no trailing `/`). A parameter is a whole segment written `{name}`; the name must match `[A-Za-z_][A-Za-z0-9_]*` and be unique within the pattern. Captured parameters reach the workflow as `metadata.params` — see the [Workflow Schema](./workflows.md).

A channel names its workflow with a top-level `workflow_id`; how conditions and rollout percentages select a workflow version is specified in the [Workflow Schema](./workflows.md). Activation requires that workflow to be active.

```json
{{#include ../../../examples/packages/webhook-transform/channel.json}}
```

Guard keys go in a `config` object beside these fields:

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

## Authentication

`auth` authenticates HTTP callers of a data channel. It covers `POST /api/v1/data/{channel}` and the `/async` submission identically — appending `/async` is not a bypass. Without an `auth` block, the channel is reachable by anyone who can reach the port; [`[admin_auth]`](./configuration.md#admin-authentication) protects `/api/v1/admin` only.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `mode` | string | yes | — | `api_key`, `hmac`, or `jwt`. |
| `keys` | array of strings | `api_key` | — | Accepted keys; any match authorizes. Each entry is a literal or an `env://VAR` reference. |
| `header` | string | no | `Authorization` (`api_key`) / `X-Signature` (`hmac`) | Header carrying the credential. |
| `scheme` | string | no | `Bearer ` when `header` is `Authorization`; none otherwise | `api_key` only: expected prefix on the header value. |
| `secret` | string | `hmac` | — | Shared secret; literal or `env://VAR`. |
| `secrets` | array of strings | no | — | `hmac` only: additional accepted secrets, each tried in constant time — zero-downtime rotation. Merged with `secret`; at least one of the two is required. |
| `signature_prefix` | string | no | none | `hmac` only: prefix stripped from the signature before decoding, for example `sha256=`. Mutually exclusive with `signature_key`. |
| `signature_key` | string | no | none | `hmac` only: extract the signature from a comma-separated `k=v` packed header instead — Stripe's `v1`. Every occurrence is tried. |
| `algorithm` | string | no | `sha256` | `hmac` only: `sha1`, `sha256`, or `sha512`. The provider chooses; refusing `sha1` would only leave those webhooks unauthenticated. |
| `message` | string | no | `{body}` | `hmac` only: the signing-string template — literals plus `{body}` (required), `{header:<name>}`, and `{header:<name>:<key>}` for packed headers. Strictly parsed at create time. |
| `encoding` | string | no | auto-detect | `hmac` only: pins the presented signature encoding (`hex`, `base64`, `base64url`). Absent keeps auto-detection: hex first, then base64. |
| `timestamp` | string | no | — | `hmac` only: where the unix-seconds timestamp lives — `<header>` or `<header>:<key>`. Paired with `tolerance_secs`; either alone is a create-time error. |
| `tolerance_secs` | integer | no | — | `hmac` only: replay window in seconds around `timestamp`; requests outside it are refused before the MAC is computed. |
| `preset` | string | no | — | `hmac` only: `zoom`, `slack`, `stripe`, `github`, `shopify`, or `webex` — expands to the fields above; an explicitly set field overrides its preset row. |
| `jwt_keys` | array | no | — | `jwt` only: static verification keys `[{algorithm, key, kid?, key_encoding?}]`. At least one of `jwt_keys`/`jwks_url`. |
| `jwks_url` | string | no | — | `jwt` only: HTTPS JWKS URL — cached process-wide, single-flight refresh, stale-serve, `kid`-rotation refetch. |
| `algorithms` | array | `jwt` | — | `jwt` only: the mandatory non-empty allowlist (`HS/RS/PS 256-512`, `ES256/384`, `EdDSA`). Checked before anything else about a token — `alg: none` is unrepresentable. |
| `issuer` / `audience` | string \| array | no | — | `jwt` only: accepted `iss`/`aud` values. Absent skips the check. |
| `leeway_secs` | integer | no | `30` | `jwt` only: clock-skew allowance for `exp`/`nbf`, capped at 300. |
| `require_exp` | boolean | no | `true` | `jwt` only: tokens must carry `exp` (RFC 8725); opting out is deliberate config. |
| `required` | boolean | no | `true` | `jwt` only: `false` admits token-less requests with no `metadata.auth` key; a present-but-invalid token is still rejected. |
| `source` | object | no | `Authorization: Bearer` | `jwt` only: `{"header": …, "scheme": …}` or `{"cookie": …}`. Query parameters are deliberately not offered (RFC 6750 §2.3). |
| `max_token_bytes` | integer | no | `8192` | `jwt` only: token size cap. |
| `claims_to_metadata` | array | no | all claims | `jwt` only: which verified claims reach `metadata.auth.claims`. |
| `authorization_logic` | JSONLogic | no | — | `jwt` only: evaluated over `{"claims": …}` after verification; falsy → **403** `insufficient_scope`. An evaluation error fails closed. |

**`api_key`** compares the presented key in constant time against the SHA-256 of each accepted key. Listing several keys enables rotation without a window of refusals:

```json
{
  "auth": {
    "mode": "api_key",
    "keys": ["env://ORDERS_API_KEY", "env://ORDERS_API_KEY_PREVIOUS"],
    "header": "X-API-Key"
  }
}
```

**`hmac`** verifies an HMAC over a configurable signing string — by default the raw request body with SHA-256, exactly the pre-1.1 behavior. Verification runs on the bytes exactly as received, before any parsing, in constant time, against every listed secret.

Most providers are one preset:

```json
{ "auth": { "mode": "hmac", "preset": "zoom",   "secret": "env://ZOOM_WEBHOOK_SECRET" } }
{ "auth": { "mode": "hmac", "preset": "stripe", "secret": "env://STRIPE_WEBHOOK_SECRET" } }
{ "auth": { "mode": "hmac", "preset": "slack",  "secret": "env://SLACK_SIGNING_SECRET", "tolerance_secs": 60 } }
```

| Preset | Scheme it expands to |
|---|---|
| `zoom` | SHA-256 over `v0:{header:x-zm-request-timestamp}:{body}`, `v0=` hex in `x-zm-signature`, 300 s window |
| `slack` | SHA-256 over `v0:{header:x-slack-request-timestamp}:{body}`, `v0=` hex in `x-slack-signature`, 300 s window |
| `stripe` | SHA-256 over `{header:stripe-signature:t}.{body}`, signature from the packed header's `v1` key(s), 300 s window |
| `github` | SHA-256 over `{body}`, `sha256=` hex in `x-hub-signature-256` |
| `shopify` | SHA-256 over `{body}`, base64 in `x-shopify-hmac-sha256` |
| `webex` | SHA-1 over `{body}`, hex in `x-spark-signature` |

An unlisted provider is the explicit form — configuration, never code:

```json
{
  "auth": {
    "mode": "hmac",
    "secret": "env://PARTNER_WEBHOOK_SECRET",
    "message": "v1:{header:x-request-timestamp}:{body}",
    "header": "x-partner-signature",
    "signature_prefix": "v1=",
    "timestamp": "x-request-timestamp",
    "tolerance_secs": 300
  }
}
```

One named non-goal: **Twilio**, whose base string needs the full public URL plus re-sorted form parameters — a per-provider algorithm, not a concatenation.

**`jwt`** verifies a bearer token at ingress and exposes the **verified claims** — never the token — at `metadata.auth.claims.*`, where `validation_logic`, `authorization_logic`, and every workflow task can read them. That is the difference from fronting Orion with a gateway: a gateway can accept or reject, but it cannot give the workflow the identity (`sub`, roles) that per-user logic needs, except by forwarding spoofable headers.

```json
{
  "auth": {
    "mode": "jwt",
    "algorithms": ["HS512"],
    "jwt_keys": [{ "algorithm": "HS512", "key": "env://JWT_ACCESS_SECRET" }],
    "issuer": "example-api",
    "authorization_logic": { "in": ["teacher", { "var": "claims.roles" }] }
  }
}
```

Verification is fail-fast (RFC 8725): extract → allowlist (`alg: none` and downgrades die here) → `kid` routing → signature → `exp`/`nbf`/`iss`/`aud` with leeway → `authorization_logic` (falsy → 403; everything before → 401 with `WWW-Authenticate: Bearer`). Only **expiry** is named on the wire (`error_description="token expired"` — the one failure a client answers with a refresh); every other reason is uniform, and typed only in metrics and traces. Verified claims propagate through `channel_call` — one request, one identity. Static-key rotation is old + new entries under distinct `kid`s; issuer-side JWKS rotation is absorbed by the cache's refetch. Login and refresh flows are the [`jwt_sign` / `jwt_verify`](./functions.md#jwt_sign) task functions over the same core.

Rules:

- **A failure is always `401` with one message**, whatever the cause. The response never reveals whether the header was missing, the key wrong, the signature malformed, or the timestamp stale. A template header missing from the request refuses — never empty-string substitution — and the replay window is checked before any MAC work.
- **Auth configs are validated structurally at create/update/validate/import**: a missing `secret`, an unknown preset, a malformed template, or half a replay guard is a `400` naming the problem — not a channel quarantined at the next reload.
- **`env://` references resolve at channel load.** An `auth` block that cannot be built — an unset `env://` secret, for example — quarantines the channel rather than serving it unauthenticated.
- **`auth.keys`, `auth.secret`/`auth.secrets`, and `auth.jwt_keys[].key` are masked** as `"******"` in every API read. A masked value sent back on update is restored from the stored config; a sentinel with nothing to restore from is refused.
- **Kafka and `channel_call` are exempt by design.** A Kafka record carries no header and no signature; its authentication is the broker connection's (SASL/mTLS). A `channel_call` is a step inside a request that already authenticated at its own ingress and holds no credential to present.
- OIDC flows (discovery, PKCE, userinfo) and mTLS stay out of scope — the `jwt` mode verifies tokens; it is not an IdP. See [Secure an Instance](../operate/security.md).

## Rate limiting

`rate_limit` meters admission with a token bucket: tokens refill at the configured rate, `burst` absorbs short spikes, and an empty bucket answers `429 Too Many Requests`.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `requests_per_second` | integer | yes | — | Steady admission rate per bucket. |
| `burst` | integer | no | `requests_per_second / 2 + 1` | Allowance above the steady rate. |
| `key_logic` | JSONLogic | no | caller identity | Expression computing the bucket key. See the context below. |
| `key_headers` | array of strings | no | — | Extra request headers `key_logic` may read, **merged with** the built-in set below. |
| `on_backend_error` | string | no | `"allow"` | `allow` fails open, `deny` refuses with `503`, when the shared cluster Redis cannot answer. Irrelevant on a single node — the in-process limiter cannot fail. |

The limit applies on every ingress, whether or not the platform limiter ([`[rate_limit]`](./configuration.md#rate-limiting)) is enabled.

> [!WARNING]
> Without `key_logic`, the bucket key is the caller identity each transport has: the client IP over HTTP, the topic for Kafka, the calling channel for `channel_call`. `requests_per_second: 100` therefore admits 100/s **per HTTP client**, plus 100/s from the channel's Kafka topic, plus 100/s per calling channel — a per-caller rate, not a throughput cap. For one shared bucket, give `key_logic` an expression that returns the same value on every ingress:

```json
{
  "rate_limit": {
    "requests_per_second": 100,
    "burst": 20,
    "key_logic": { "var": "channel" }
  }
}
```

**`key_logic` context.** The expression evaluates against exactly:

```json
{ "client_ip": "…", "channel": "…", "headers": { } }
```

`client_ip` is the transport's caller identity (it keeps that name on all four ingresses). `headers` contains these headers, when present: `authorization`, `x-api-key`, `x-forwarded-for`, `x-real-ip`, `user-agent`, `content-type`, `origin`, `x-tenant-id` — plus any name the channel lists in `key_headers`. No other header is visible to `key_logic`. A non-string result is serialized to its JSON text and used as the key.

**`key_headers`** is what makes a house header keyable — a `deviceId`, an `x-client-id`, an `x-partner`:

```json
{
  "rate_limit": {
    "requests_per_second": 5,
    "key_headers": ["deviceid"],
    "key_logic": { "var": "headers.deviceid" }
  }
}
```

Names are matched case-insensitively (they are lowercased at load) and the list **adds to** the built-in set rather than replacing it, so declaring a header can never take `x-tenant-id` away from an expression that already reads it. Listing a built-in again is a no-op. The set stays closed by default because the request path materializes exactly the names that might be read — a channel does not pay an allocation per header for a key that references one of them.

> [!WARNING]
> A header is caller-supplied and therefore spoofable, so a key derived from one bounds an **honest** client. That is the right trade for a burst control, and the wrong one for a quota: forging a token-bucket key gets you a different bucket, not a bigger one, but forging a quota key is the whole attack. For per-user quotas, count in the workflow — `db_write`/`mongo_write` can increment and read back atomically — and keep this guard on top as the per-caller burst control.

The key is part of the control, not a hint:

- A `key_logic` that does not compile quarantines the channel at load.
- A request whose key cannot be evaluated is rejected with `429`. Nothing falls back to `client_ip` — that would silently re-dimension a per-tenant limit into a per-IP one.
- A request whose key evaluates to `null` or an empty string is rejected the same way. A missing path resolves to `null`, so a `key_logic` naming a header outside the set above would otherwise make the bucket key the literal string `"null"` for **every** caller — one shared bucket, and a limit that reads as enforced while enforcing nothing. Orion warns at channel load when an expression statically reads a header the context will not carry, so a typo surfaces at boot rather than as unexplained throttling.

**Cross-ingress semantics.** A Kafka record refused by the limit is not dead-lettered: its offset stays uncommitted and the consumer's capped retry backoff becomes the throttle. The exception is a `key_logic` that cannot be evaluated against the record — that fails identically on every redelivery, so the record is dead-lettered instead of blocking its partition.

**Cluster mode.** Per-channel limits enforce as a shared fixed window on the cluster Redis, so the configured rate holds across all replicas combined. Platform-level limits stay per node. See [Cluster Mode](../operate/cluster.md).

Limiter state survives engine reloads: a channel whose `requests_per_second`, `burst`, `key_logic`, and `key_headers` are unchanged keeps its limiter, and consumed burst is not refilled. Editing any of them re-dimensions the buckets, so the limiter is rebuilt. Behind a proxy, set [`rate_limit.trusted_proxies`](./configuration.md#rate-limiting) in the server config — without it, every client behind the proxy keys on the proxy's address and collapses into one bucket.

## Backpressure

`backpressure` bounds a channel's in-flight work with a semaphore. When every permit is taken, additional requests are refused with `503 Service Unavailable` immediately — load shedding, not queueing.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `max_concurrent_per_node` | integer | yes | — | Maximum concurrent requests for this channel on this node. |

```json
{ "backpressure": { "max_concurrent_per_node": 200 } }
```

The permit is per channel, not per ingress: synchronous requests, queued `/async` work, Kafka records, and `channel_call`s all draw from the same semaphore. Each channel's semaphore is independent, so a spike on one channel does not shed another's traffic.

**Cross-ingress semantics.** A Kafka record that cannot get a permit is left uncommitted for redelivery rather than shed — the transport can wait; an HTTP caller cannot be told to.

The semaphore is per process, as the name states: N replicas admit up to N × `max_concurrent_per_node` in flight in total.

## Deduplication

`deduplication` extracts an idempotency key from a request header and refuses a repeat of the same key within the window with `409 Conflict`.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `header` | string | yes | — | Header carrying the idempotency key. |
| `window_secs` | integer | no | `300` | Seconds a key is remembered. |
| `connector` | string | no | in-memory store | Name of a [cache connector](./connectors.md) backing the dedup store. In cluster mode the default is the shared cluster Redis. |
| `on_backend_error` | string | no | `"allow"` | `allow` proceeds without the check when the store cannot answer; `deny` refuses with `503` — never `409`, because the key is unverifiable, not a known duplicate. |

```json
{
  "deduplication": {
    "header": "Idempotency-Key",
    "window_secs": 300,
    "on_backend_error": "deny"
  }
}
```

Rules:

- Keys are scoped per channel. A request that does not carry the header is not checked.
- The key is claimed before the workflow runs and settled once the outcome is durable. A delivery that fails without settling is re-processed on retry, not refused as a duplicate of itself. The full claim/settle argument is in [Availability](../concepts/lifecycle.md).
- `deny` on payment-style workloads trades availability for the guarantee that a duplicate can never slip through an outage.

**Kafka.** Kafka ingest deduplicates too, and needs it most: at-least-once delivery replays records the workflow already ran. The key is the record header named by `header` when the producer sets one, else the record key. A recognized duplicate is skipped and its offset committed — nothing is dead-lettered, because nothing failed. Set the key per logical event, not per entity. Deduplication narrows at-least-once; it does not make Kafka exactly-once.

**`channel_call` is exempt.** An in-process call is a step inside a request already deduplicated at its own ingress and carries no key of its own. It would inherit the parent's, so a workflow calling one channel once per line item would see its second call refused.

**Cluster mode.** A channel whose dedup connector is missing, broken, or explicitly in-memory refuses to load instead of silently degrading to per-node state. On a single node, an unusable connector falls back to process memory with a warning.

## Response caching

`cache` serves repeated identical requests from a stored response instead of executing the workflow. It applies to the synchronous HTTP ingress only.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `enabled` | boolean | yes | — | `false` disables the cache without removing the block. |
| `ttl_secs` | integer | no | `300` | Seconds an entry lives. |
| `cache_key_fields` | array of strings | no | whole payload | Payload fields that form the cache key. |
| `connector` | string | no | in-memory | Name of a [cache connector](./connectors.md) backing the cache. In cluster mode the default is the shared cluster Redis. |

```json
{
  "cache": {
    "enabled": true,
    "ttl_secs": 60,
    "cache_key_fields": ["data.user_id", "data.action"]
  }
}
```

**The cache key** is derived from exactly: the channel name, the HTTP method, the route parameters, the query string (both order-independent), and the request payload — the whole payload, or the subset named by `cache_key_fields`. Each entry resolves as a literal payload key (`user_id`), a dotted path (`user.id`), or the same path with a leading `data.` prefix (`data.user_id`). A request that resolves **none** of the declared fields bypasses the cache entirely: the workflow runs, nothing is stored, and Orion logs a warning naming the channel and fields — it almost always means the names do not match the payload shape.

> [!WARNING]
> Request headers are never part of the cache key. A cached entry is shared by every caller whose method, route, query, and payload agree, whatever headers they sent. If a response varies by anything a header carries, that value must appear in the payload and in `cache_key_fields` — or the channel must not cache.

Behaviour:

- A hit is served without executing the workflow and without consuming a backpressure permit.
- A replayed idempotency key answers `409` before the cache is consulted.
- The response is stored on success and expires after `ttl_secs`.
- A cached [shaped](#response-shaping) response replays its status and headers, not just its body.
- A write-gated cache connector is refused for the response cache; see [operation gates](./connectors.md).

**Cluster mode.** With the shared cluster Redis, hits are shared across replicas. A channel whose cache connector is missing, broken, or explicitly in-memory refuses to load; on a single node it falls back to process memory with a warning.

## Request body

`request` controls how the HTTP request body becomes `data` and `metadata`. **HTTP ingresses only** — Kafka parses the whole payload as `data` and builds metadata separately, and `channel_call` inherits the parent's metadata with `data` from the task input, so neither is affected.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `body_mode` | string | no | `"auto"` | `auto` detects the Orion envelope; `payload` takes the parsed body verbatim. |
| `cookies_to_metadata` | array of strings | no | — | Named request cookies copied to `metadata.cookies.*`. Absent exposes nothing. |

Under `auto`, **an object carrying a top-level `data` or `metadata` key is the envelope** — that key becomes the payload and every sibling field is discarded. Anything else (an array, a scalar, an object without those keys) is the payload as it stands, and an empty body is `{}`.

That rule keys on a field *name*, so a request model that owns the name `data` — the standard FCM/push payload shape, among others — is read as an envelope and loses its siblings silently, with a normal `200`. `payload` mode is the opt-out:

```json
{
  "config": {
    "request": { "body_mode": "payload" }
  }
}
```

The two modes differ for **exactly one input shape**: a top-level object carrying `data` or `metadata`. Everything else already took the payload path in both.

Three consequences worth knowing before switching a channel:

- **A caller cannot supply `metadata` at all** in `payload` mode — the metadata object is server-stamped keys only (`channel`, `http_method`, and `params`/`query`/`headers` where applicable). Under `auto`, a caller-supplied `metadata.params` or `metadata.query` survives when the server has none of its own to stamp, so this is a small security win as well as a trade-off.
- **Downstream, consistently:** `validation_logic` sees the whole body under `data`, and `cache.cache_key_fields` paths resolve against it. HMAC signing is unaffected — it always signed the raw bytes.
- **`orion-cli send` cannot reach a payload-mode channel.** Every CLI and MCP data path wraps its argument in `{"data": …}`, which a payload-mode channel then delivers as `data = {"data": …}`. Use `curl` for these channels.

> [!WARNING]
> Flipping a **live** channel from `auto` to `payload` changes its wire contract for any caller currently sending a legitimate `{"data": …}` envelope — that envelope becomes the payload, so the workflow starts reading `data.data.*`. It is a config change with the blast radius of a code change.

### Reading request cookies

The `Cookie` header is masked to `"******"` before request metadata is built, along with `authorization`, `proxy-authorization` and `x-api-key` — the metadata map is persisted verbatim into `traces.result_json` and `trace_dlq.metadata_json`, so a plaintext value there is a plaintext credential at rest.

Not every cookie is a credential, though. `cookies_to_metadata` names the ones a workflow may read:

```json
{
  "config": {
    "request": { "cookies_to_metadata": ["browser_uuid"] }
  }
}
```

and then, in any task or in `validation_logic`:

```json
{ "var": "metadata.cookies.browser_uuid" }
```

A listed-but-absent cookie is simply not present — never `null`, never an error. The raw `Cookie` header stays masked: this allowlist is additive and never unmasks it. `metadata.cookies` is platform-reserved, stamped from the allowlist and stripped otherwise, so a caller cannot supply it in an envelope.

**Scope it to opaque identifiers a workflow matches against its own stored state** — a browser-pinning id, a first-party visitor id, a bucket cookie. For a session token, JWT or CSRF token use [`auth.mode: "jwt"`](#authentication) with `source: {"cookie": …}` instead, where the token is consumed at verification rather than copied into the context.

> [!WARNING]
> **Allowlisted values land in `traces.result_json` and `trace_dlq.metadata_json` unmasked.** The read side is covered — `GET /admin/traces/{id}` strips all of `context.metadata` — but the row on disk is not. Note also that `tracing.mode = "off"` suppresses only *sync* persistence: on an `/async` channel the row is still written before the `202`, so turning tracing off is **not** a complete mitigation there. `trace_queue.retention_hours` is the ageing-out control.

Two further limits worth knowing:

- **A cookie-varying channel must not enable `cache`.** `compute_cache_key` hashes method, params, query and payload — never headers — so a cached response would replay one caller's `Set-Cookie` to the next.
- **`rate_limit.key_logic` still cannot see cookies.** Its context is `{client_ip, channel, headers}`, and `cookie` is not among the readable headers. Per-cookie rate limiting stays out of reach.

`channel_call` propagates metadata verbatim, so an allowlisted cookie reaches sub-channels — the same way verified claims do.

## Response shaping

By default every sync channel answers `200` with the fixed envelope `{id, status, data, errors}`, whatever happened — see [Errors & Response Envelopes](./errors.md). That is a workable contract between workflows and an awkward one for a REST API: no `201` with a `Location`, no `404`, no content type but JSON.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `response.mode` | string | no | `"envelope"` | `envelope` or `shaped`. |
| `response.allowed_headers` | array of strings | no | default allowlist | Headers the workflow may set. **Replaces** the default list, so a channel can narrow it as well as widen it. Case-insensitive. |

```json
{ "response": { "mode": "shaped", "allowed_headers": ["location"] } }
```

A shaped channel's workflow writes a control block to `data._orion.response`. Orion drains it before responding — it is control, not content, and never reaches the caller's body:

```json
{
  "id": "respond", "name": "Respond",
  "function": { "name": "map", "input": { "mappings": [
    { "path": "data._orion.response.status",  "logic": 201 },
    { "path": "data._orion.response.headers", "logic": {
        "Location": { "cat": ["/orders/", { "var": "data.order.id" }] } } },
    { "path": "data._orion.response.body_path", "logic": "data.order" }
  ]}}
}
```

The control block's fields:

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `status` | number | no | `200` | HTTP status. Out-of-range values fall back to `200`. |
| `headers` | object | no | `{}` | Response headers, subject to the allowlist below. |
| `body_path` | string | no | whole document | Field to send instead of the entire data document. A leading `data.` is optional. |
| `raw` | boolean | no | `false` | Send a string field verbatim rather than as a JSON string — how a channel returns CSV, XML, or plain text. |

`Content-Type` is `application/json` unless the workflow sets it.

**Header allowlist.** With no `allowed_headers`, a workflow may set `content-type`, `location`, `cache-control`, `etag`, `last-modified`, `retry-after`, `content-language`, and `link`. The hop-by-hop headers (`connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`), `content-length`, and `x-request-id` are refused even when listed — response framing belongs to the server, and `x-request-id` correlates a response with its stored trace. A dropped header does not fail the request.

**Failures are soft.** A shaped channel whose workflow sets no control block, or an unusable one, falls back to the standard envelope rather than erroring.

**Interactions.** A cached shaped response replays its status and headers, not just its body. Profiling (`?profile=1`) appends `_orion.profile` to the envelope only — a shaped body is the workflow's own — though timings still reach the trace and metrics. Shaping applies to the synchronous path only; [`/async`](./data-api.md#asynchronous-processing) answers `202` with a trace id as always.

## Validation

`validation_logic` is a JSONLogic predicate evaluated before workflow execution on every ingress. A truthy result admits the request; a falsy one rejects it with `400 Bad Request`. JSONLogic truthiness applies: `false`, `null`, `0`, `""`, and `[]` are falsy.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `validation_logic` | JSONLogic | no | none | Predicate over `{data, metadata}`. See the [Expression Language](./expressions.md). |

```json
{
  "validation_logic": {
    "and": [
      { "!!": [{ "var": "data.order_id" }] },
      { ">": [{ "var": "data.amount" }, 0] }
    ]
  }
}
```

**Context.** The expression evaluates against exactly `{ "data": …, "metadata": … }`. `data` is the request payload as submitted — the guard runs before any workflow task, so `data.order_id` resolves here even though the workflow itself reads the payload only after `parse_json`. `metadata` has the same shape the workflow's data context carries (headers, query, path params, channel name; transport-dependent) — see the [Workflow Schema](./workflows.md). On Kafka, `metadata` carries the record coordinates and no headers.

Rules:

- An expression that cannot be evaluated against a request rejects it with the same opaque `400` — the detail is logged, not returned, because the data plane is anonymous.
- A `validation_logic` that does not compile quarantines the channel at load.
- Payload size is bounded globally by [`ingest.max_payload_size`](./configuration.md#ingest), not per channel.

## Timeouts

`timeout_ms` bounds workflow execution for one message. A synchronous request that exceeds it answers `504 Gateway Timeout`.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `timeout_ms` | integer | no | per ingress, below | Maximum workflow execution time in milliseconds. |

```json
{ "timeout_ms": 5000 }
```

The value governs every ingress. Where the channel declares none, each ingress falls back to its own server-level default — and on two ingresses that server value is a **ceiling** the channel value is clamped to, never a mere default:

| Ingress | Channel declares none | Channel declares more than the transport allows |
|---|---|---|
| synchronous HTTP | runs to completion | honored — nothing else waits on it |
| `/async` | `trace_queue.processing_timeout_ms` | clamped to `trace_queue.processing_timeout_ms` |
| Kafka | `kafka.processing_timeout_ms` | clamped to `kafka.processing_timeout_ms` |
| `channel_call` | `engine.default_channel_call_timeout_ms` | honored — the calling task's own `timeout_ms` outranks it anyway |

<details><summary>Why the two clamps</summary>

On those paths the deadline protects something shared. A Kafka dispatch blocks the consumer's poll loop; a channel asking for ten minutes would push the consumer past librdkafka's `max.poll.interval.ms` and get it evicted from its group mid-record. An `/async` dispatch occupies one of a fixed number of queue workers, so an over-long deadline starves every other channel's queued work. A channel can shorten its deadline everywhere; it can lengthen it only where nothing else depends on it. Raise the transport setting if a channel genuinely needs longer there.

</details>

A `channel_call` task may set its own `timeout_ms`, which outranks the target channel's — see [Task Functions](./functions.md). The server-level settings live in the [Configuration Reference](./configuration.md).

## CORS & origins

`origin_allow_list` restricts which `Origin` values a channel accepts, server-side.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `origin_allow_list` | array of strings | no | no check | Accepted `Origin` values. `"*"` allows any origin. |

```json
{ "origin_allow_list": ["https://app.example.com", "https://admin.example.com"] }
```

Rules:

- A request whose `Origin` header is present and unlisted is refused `403` before the workflow runs.
- A request with no `Origin` header is not checked at all.
- Omitting the key checks nothing. `origin_allow_list` is the only accepted spelling.
- The check applies to the HTTP ingresses only — a Kafka record and a `channel_call` have no origin to check.

**This is not CORS.** It performs no handshake, sets no `Access-Control-Allow-Origin`, and takes no part in a preflight. The browser handshake is the platform [`[cors]`](./configuration.md#cors) layer's job. The division of labor:

- **`[cors]` governs the browser handshake.** It short-circuits a genuine preflight from an unlisted origin, but a non-preflighted cross-origin request still runs server-side — the layer merely omits the response header and the browser discards the answer. Non-browser clients are unaffected entirely.
- **`origin_allow_list` is the server-side check.** It runs on every request that reaches the handler, browser or not, and stops the workflow from executing.

Neither is authentication: `Origin` is client-supplied, and any non-browser caller can set or omit it. For access control that holds against a hostile client, use [`auth`](#authentication).

## Tracing override

`tracing` overrides the global [`[trace_storage]`](./configuration.md#trace-persistence) policy for one channel. Each field is independently optional; an unset field falls back to the global value.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `mode` | string | no | global `trace_storage.mode` | `sync`, `async`, `batch`, or `off`. |
| `sample_rate` | number | no | global `trace_storage.sample_rate` | Fraction of traces persisted, `0.0`–`1.0`. Applies to sync traces only; async traces always persist. |
| `errors_only` | boolean | no | global `trace_storage.errors_only` | Persist only traces that ended with errors. |
| `task_details` | boolean | no | `false` | Capture a per-task execution trace into `task_trace_json`. No global setting exists — this is per-channel only. |

```json
{
  "tracing": { "mode": "async", "errors_only": true, "task_details": true }
}
```

> [!NOTE]
> Each `task_details` trace grows with message size times task count. Enable it for debugging, not as a default. The recorded shape is specified under [the trace object](./data-api.md#the-trace-object).

## Related

- [Data API](./data-api.md) — how requests resolve to channels, and what traces carry.
- [Workflow Schema](./workflows.md) — the data context these guards feed, and workflow selection.
- [Connector Types](./connectors.md) — the cache connectors named by `deduplication.connector` and `cache.connector`.
- [Configuration Reference](./configuration.md) — every server-level setting named on this page.
