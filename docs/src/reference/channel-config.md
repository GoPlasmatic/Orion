<!-- description: Every key in an Orion channel's config object: auth, rate limiting, backpressure, deduplication, response caching, validation, response shaping and timeouts. -->
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
| [`principal_rate_limit`](#per-principal-quotas-principal_rate_limit) | A second limit, applied after authentication and keyed on the verified principal. |
| [`backpressure`](#backpressure) | Per-node concurrency cap; excess is shed with `503`. |
| [`deduplication`](#deduplication) | Idempotency-key replay protection. |
| [`cache`](#response-caching) | Serve repeated identical requests from a response cache. |
| [`request`](#request-body) | How the HTTP request body becomes `data` and `metadata`. |
| [`response`](#response-shaping) | Standard envelope, or workflow-controlled status, headers, and body — and [per-status guard-rejection bodies](#error-bodies). |
| [`validation_logic`](#validation) | JSONLogic predicate; a falsy result rejects the request with `400`. |
| [`timeout_ms`](#timeouts) | Deadline on workflow execution. |
| [`origin_allow_list`](#cors--origins) | Server-side `Origin` header check. |
| [`tracing`](#tracing-override) | Per-channel override of the global trace-storage policy. |
| [`oauth2_login`](#inbound-oauth2-sign-in) | Complete a browser OAuth2 authorization-code grant on this channel. |

**Field-table legend.** In the Required column, `yes`/`no` are absolute; a protocol or mode name (`rest`, `hmac`, …) means the field is required exactly when that protocol or mode applies. `—` in Default means the field has no value until you set one. The Guards-by-ingress matrix above answers a different question and answers it in prose — `Yes`/`No`, not a field value.

## Guards by ingress

A channel is reachable on up to five ingresses. Each guard runs on the ingresses marked Yes:

| Guard | HTTP sync | HTTP `/async` | Kafka | `channel_call` | Cron |
|---|---|---|---|---|---|
| `rate_limit` | Yes | Yes | Yes | Yes | No |
| `principal_rate_limit` | Yes | Yes | No | No | No |
| `auth` | Yes | Yes | No | No | No |
| `origin_allow_list` | Yes | Yes | No | No | No |
| `validation_logic` | Yes | Yes | Yes | Yes | Yes |
| `deduplication` | Yes | Yes | Yes | No | No |
| `cache` | Yes | No | No | No | No |
| `backpressure` | Yes | Yes | Yes | Yes | Yes |
| `oauth2_login` | Yes | No | No | No | No |
| `timeout_ms` | Yes | Yes | Yes¹ | Yes | Yes |

¹ Clamped to a transport ceiling. See [Timeouts](#timeouts). Every No cell is deliberate; the owning section below states why.

The Cron column is mostly No because that ingress has no caller at all, which is a stronger statement than Kafka's "the caller authenticated elsewhere". A cron channel is refused these keys at authoring time rather than storing them and ignoring them — see [Cron transport](#cron-transport).

<details><summary>Order of application</summary>

Guards run in a fixed order: rate limit → auth → origin allow-list → validation → deduplication → cache lookup → backpressure → `oauth2_login`. Four consequences:

- A rejected request (bad origin, failed validation) still consumes a rate-limit token.
- A replayed idempotency key answers `409` before the cache is consulted.
- A cache hit never consumes a backpressure permit, and a request shed by backpressure releases its idempotency claim.
- `oauth2_login` runs last, *after* the backpressure permit, because its callback leg makes a round trip to the identity provider and that call must be bounded by the channel's concurrency cap. The consequence is that `validation_logic` sees the request and not the grant — the grant is what the workflow is for.

</details>

## Routing & protocol

These fields sit on the channel object itself, beside `config`. They decide how requests reach the channel; [Route Resolution](./data-api.md#route-resolution) in the Data API reference specifies how a request path resolves to a channel.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `channel_type` | string | yes | — | `sync` (the caller waits for the result) or `async` (queued; answers `202` with a trace id). Case-insensitive. A `cron` channel must be `async`. |
| `protocol` | string | yes | — | `rest`, `http`, `kafka`, or `cron`. Case-insensitive. Immutable across versions. |
| `methods` | array of strings | `rest`, `http` | — | HTTP methods the route answers. Valid values: `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`. An unknown or duplicated method is refused. |
| `route_pattern` | string | `rest`, `http` | — | Path pattern, for example `/orders/{id}`. Grammar below. At most 255 characters. |
| `topic` | string | `kafka` | — | Kafka topic the channel consumes. At most 255 characters. Refused on a `cron` channel. |
| `consumer_group` | string | no | — | Kafka consumer group name. At most 255 characters. |
| `priority` | number | no | `0` | Route-match precedence. Routes match by priority descending, then segment count descending, then channel name — deterministic on every node. |

`rest` and `http` route identically: both must declare `methods` and `route_pattern`, both register in the route table, and both stay reachable by name at `/api/v1/data/{name}`. An async channel's pattern serves at `/{pattern}/async`, whatever its `channel_type`. A `kafka` channel registers its `topic` as a consumer at startup and on engine reload; config-file topic mappings take precedence over channel-declared ones (see [Kafka Consumer Configuration](./configuration.md#kafka)).

A `cron` channel declares none of those four fields and each is refused: it registers no HTTP route and no Kafka subscription, and it is **not** reachable by name at `/api/v1/data/{name}` either. Its schedule is the only thing that starts it. See [Cron transport](#cron-transport).

**`route_pattern` grammar.** The pattern must start with `/`. It must not contain whitespace, `?`, `#`, or `%`. No segment may be empty (no `//`, no trailing `/`). A parameter is a whole segment written `{name}`; the name must match `[A-Za-z_][A-Za-z0-9_]*` and be unique within the pattern. Captured parameters reach the workflow as `metadata.params`. See the [Workflow Schema](./workflows.md).

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

## Cron transport

A `cron` channel is started by a clock instead of a caller. It declares its schedule in `transport_config`, which is ordinary definition content — versioned with the channel, covered by its content hash, and promoted inside a package like everything else. There is no new top-level field and no fourth entity.

```json
{
  "channel_id": "nightly-order-rollup",
  "name": "Nightly order rollup",
  "channel_type": "async",
  "protocol": "cron",
  "workflow_id": "order-rollup",
  "transport_config": {
    "schedule": "0 15 2 * * *",
    "timezone": "Asia/Kolkata",
    "payload": { "window": "previous_day" },
    "misfire_policy": "latest",
    "concurrency": { "policy": "forbid" }
  },
  "config": { "timeout_ms": 1800000 }
}
```

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `schedule` | string | yes | — | Six-field cron expression: second, minute, hour, day-of-month, month, day-of-week. Grammar below. |
| `timezone` | string | no | `UTC` | IANA time-zone name the expression's calendar times are read in, for example `Europe/London`. Abbreviations (`IST`, `EST`) are ambiguous and refused. |
| `payload` | object | no | `{}` | The run's input. Must be an object; at most 1 MB serialized. Secrets are refused — see below. |
| `misfire_policy` | string | no | `latest` | `skip`, `latest`, or `catch_up`. What happens to occurrences whose time passed while nothing was running. |
| `max_catch_up` | number | `catch_up` | — | Bound on a replay, 1–1000. Required when `misfire_policy` is `catch_up`. |
| `concurrency.policy` | string | no | `allow` | `allow` (occurrences may overlap) or `forbid` (at most one per key at a time). |
| `concurrency.key` | string | no | the channel's `channel_id` | Literal lock name, `[A-Za-z0-9][A-Za-z0-9_.-]{0,127}`. Two channels naming the same key serialise with each other. |

Unknown keys are refused, as everywhere else in a channel definition. A misspelled `misfire_polcy` would otherwise leave the default in place forever with nothing to see.

**The payload arrives where a request body does.** A workflow reads it with `parse_json` from `payload`, exactly as it would behind a REST channel:

```json
{ "id": "parse", "function": { "name": "parse_json", "input": { "source": "payload", "target": "input" } } }
```

That is deliberate, and it is what makes a workflow portable between a route and a schedule with no change. What the schedule adds is `metadata.trigger` — see below.

### What the workflow receives

Beyond the payload, a scheduled run carries a reserved `metadata.trigger` object. It is platform-stamped, never authored:

| Field | Meaning |
|---|---|
| `type` | `cron` for a scheduled run, `manual` for one started by [the trigger endpoint](./admin-api.md#cron-occurrences) |
| `occurrence_id` | The ledger row this run belongs to |
| `scheduled_for` | The UTC instant the work was **due**. Immutable across retries |
| `started_at` | When this attempt actually began |
| `timezone` | The channel's IANA zone, so a workflow formatting a local date need not hard-code it |
| `attempt` | `1` for a first run |
| `singleton_key` | The lock this run holds, when its channel takes one |

`scheduled_for` and `started_at` are different questions and both are answered: the first is what the work is *for*, the second is when it happened. Use `scheduled_for` as an idempotency key — two attempts at one occurrence agree on it, and no two occurrences of a channel share it.

**The expression always has six fields.** `0 15 2 * * *` is 02:15 every day. The same text read as a five-field expression would mean *every minute* between 02:00 and 02:59 on the 15th of the month — a difference no author could see in the stored document, which is why five- and seven-field (trailing year) expressions are both refused rather than guessed at.

An expression with no occurrence in the next five years is refused too. `0 0 0 30 2 *` is syntactically perfect and means February 30th.

### Time zones and DST

Calendar times are read in `timezone`; Orion stores the resulting UTC instants. Each occurrence therefore has an immutable `scheduled_for` in UTC, and two rules cover the transitions:

- **A local time that does not exist does not fire.** On a spring-forward day, `0 30 1 * * *` in `Europe/London` fires on the day before and the day after and not on the transition day: 01:30 never happens.
- **A local time that happens twice fires twice.** On a fall-back day the same schedule fires at 01:30 BST and again at 01:30 GMT, an hour apart. They are different instants, so they are different occurrences with different identities.

Both follow from calendar scheduling meaning what a wall clock says. If you want exactly one run regardless, schedule outside 01:00–03:00 local, or use `UTC`.

### Misfire policies

A *misfire* is an occurrence whose scheduled time passed while no healthy scheduler could start it — a node down, a database unreachable. Ordinary polling delay is not a misfire: anything inside `cron.misfire_grace_secs` is merely late and still runs.

| Policy | What runs | Use when |
|---|---|---|
| `skip` | Nothing. The misses are recorded. | The work only makes sense at its own time. |
| `latest` (default) | The newest missed occurrence. | One run brings the world up to date — a rebuild, a summary, a sync. |
| `catch_up` | The missed occurrences oldest-first, up to `max_catch_up`. | Each occurrence does distinct work that still needs doing. |

Whatever the policy, the misses are recorded as **one** occurrence row with status `skipped_misfire` carrying the count and the range, not one row per missed instant — a per-second schedule down for a day missed 86 400 of them.

### Concurrency

`policy: "forbid"` means at most one occurrence for a `key` is admitted at a time, across the whole cluster. A contending occurrence is recorded `skipped_singleton` — visible, not dropped. `policy: "allow"` lets occurrences overlap and takes no lock at all.

The key defaults to the channel's `channel_id`, so `forbid` on its own means "one at a time, of this channel". Naming the same key on several channels deliberately serialises them with each other.

> **Non-overlap is not exactly-once.** A worker that loses its lease cancels, but it cannot prove that a connector call it already made did not land. Scheduled work that must not be applied twice needs an idempotent destination or an idempotency key, exactly as Kafka ingest does.

### What a cron channel may not declare

Everything about a caller, because there is not one:

| Refused | Instead |
|---|---|
| `methods`, `route_pattern`, `topic`, `consumer_group` | Nothing — a cron channel registers no route and no subscription. |
| `config.auth` | There is no caller to authenticate. |
| `config.origin_allow_list` | The check reads an HTTP header a scheduled run does not send. |
| `config.rate_limit` | The schedule already decides how often this runs. |
| `config.deduplication` | Occurrences are unique by `(channel, scheduled_for)` in the ledger, permanently rather than for a window. |
| `config.cache`, `config.request`, `config.response` | There is no request to shape and no reply to cache. |
| `config.oauth2_login` | Both legs are browser redirects. |

Each is refused at create, update and import time rather than stored and ignored. What still applies: `timeout_ms`, `validation_logic`, `backpressure` and `tracing`.

**Secrets are refused in `payload`.** The payload is definition content and is recorded verbatim as every occurrence's trace input, so a credential there is a credential at rest in the `traces` table. Read secrets inside the workflow, where the engine resolves them without recording them. `env://`, `vault://`, `secret://` and `var://` strings are refused for the related reason that nothing resolves them here — they would reach the workflow as literal text.

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

**`jwt`** verifies a bearer token at ingress and exposes the **verified claims**: never the token — at `metadata.auth.claims.*`, where `validation_logic`, `authorization_logic`, and every workflow task can read them. That is the difference from fronting Orion with a gateway: a gateway can accept or reject, but it cannot give the workflow the identity (`sub`, roles) that per-user logic needs, except by forwarding spoofable headers.

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

- **A failure is always `401` with one message**, whatever the cause. The response never reveals whether the header was missing, the key wrong, the signature malformed, or the timestamp stale. A template header missing from the request refuses — never empty-string substitution, and the replay window is checked before any MAC work.
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
> A header is caller-supplied and therefore spoofable, so a key derived from one bounds an **honest** client. That is the right trade for a burst control, and the wrong one for a quota: forging a token-bucket key gets you a different bucket, not a bigger one, but forging a quota key is the whole attack. For per-user quotas, count in the workflow — `db_write`/`mongo_write` can increment and read back atomically, and keep this guard on top as the per-caller burst control.

The key is part of the control, not a hint:

- A `key_logic` that does not compile quarantines the channel at load.
- A request whose key cannot be evaluated is rejected with `429`. Nothing falls back to `client_ip` — that would silently re-dimension a per-tenant limit into a per-IP one.
- A request whose key evaluates to `null` or an empty string is rejected the same way. A missing path resolves to `null`, so a `key_logic` naming a header outside the set above would otherwise make the bucket key the literal string `"null"` for **every** caller — one shared bucket, and a limit that reads as enforced while enforcing nothing. Orion warns at channel load when an expression statically reads a header the context will not carry, so a typo surfaces at boot rather than as unexplained throttling.

**Cross-ingress semantics.** A Kafka record refused by the limit is not dead-lettered: its offset stays uncommitted and the consumer's capped retry backoff becomes the throttle. The exception is a `key_logic` that cannot be evaluated against the record — that fails identically on every redelivery, so the record is dead-lettered instead of blocking its partition.

**Cluster mode.** Per-channel limits enforce as a shared fixed window on the cluster Redis, so the configured rate holds across all replicas combined. Platform-level limits stay per node. See [Cluster Mode](../operate/cluster.md).

Limiter state survives engine reloads: a channel whose `requests_per_second`, `burst`, `key_logic`, and `key_headers` are unchanged keeps its limiter, and consumed burst is not refilled. Editing any of them re-dimensions the buckets, so the limiter is rebuilt. Behind a proxy, set [`rate_limit.trusted_proxies`](./configuration.md#rate-limiting) in the server config — without it, every client behind the proxy keys on the proxy's address and collapses into one bucket.

### Per-principal quotas (`principal_rate_limit`)

`rate_limit` runs **before** authentication — deliberately, so a refusal costs
the least work and credential-stuffing is metered like any other traffic. The
consequence is that it cannot know who the caller is: the only identities it can
key on are the address and a request header, and a header is caller-supplied, so
a key derived from one bounds an honest client. That is a burst control, not a
quota.

`principal_rate_limit` is the quota half. It runs straight after authentication,
keyed on the **verified** claims, and both limits apply — the address limit stays
the cheap outer guard.

```json
{
  "auth": { "mode": "jwt", "jwt_keys": [ ... ], "algorithms": ["RS256"] },
  "rate_limit": { "requests_per_second": 100 },
  "principal_rate_limit": {
    "requests_per_second": 10,
    "burst": 20,
    "key_logic": { "var": "auth.sub" }
  }
}
```

The block takes the same fields as `rate_limit` — `requests_per_second`,
`burst`, `key_logic`, `key_headers`, `on_backend_error` — with two differences,
both refused at create rather than at run time:

- **`key_logic` is required.** The address limiter falls back to the caller
  identity when none is given; a principal has no such fallback, and inventing
  one would silently turn a per-user quota into a per-address one.
- **`auth.mode` must be `jwt`.** It is the only mode that exposes claims. On any
  other the key could never be computed and every request would be refused, so
  the config is refused instead.

Its `key_logic` context is the one `rate_limit.key_logic` reads plus `auth`, the
verified claims — so `{"var": "auth.sub"}` is the usual key, and
`{"cat": [{"var": "auth.tenant"}, "|", {"var": "auth.sub"}]}` meters a tenant and
a user together. Every rule above applies unchanged: a key that will not
evaluate, or resolves to `null` or an empty string, is refused rather than
bucketed somewhere wrong.

The two limiters keep separate buckets and separate state across reloads. A
refusal from either answers `429` and counts in
`orion_rate_limit_rejections_total` under the channel's name — 429 accounting
stays whole; which of the two refused is in the log line.

## Backpressure

`backpressure` bounds a channel's in-flight work with a semaphore. When every permit is taken, more requests are refused with `503 Service Unavailable` immediately — load shedding, not queueing.

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
| `key_logic` | JSONLogic | no | — | Computes the cache key. Takes precedence over `cache_key_fields`. |
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

**The cache key** is derived from exactly: the channel name, the HTTP method, the route parameters, the query string (both order-independent), and the request payload — the whole payload, the subset named by `cache_key_fields`, or the result of `key_logic`.

`key_logic` is the general form, and the same vocabulary [`rate_limit.key_logic`](#rate-limiting) uses, so one channel does not key two of its guards two different ways. It reads `{"data": …, "metadata": …}` and **replaces** the payload-derived half of the key rather than adding to it — an expression that says what varies the response is a complete answer, and mixing it with a payload hash would put back the fields it was written to exclude:

```json
"cache": {
  "enabled": true,
  "ttl_secs": 60,
  "key_logic": { "cat": [{ "var": "metadata.auth.subject" }, "|", { "var": "data.report_id" }] }
}
```

An expression that does not compile quarantines the channel rather than falling back — a cache key that silently widens serves one caller's body to the next. One that resolves to `null` at request time bypasses the cache for that request, as an unresolvable `cache_key_fields` does. Each entry resolves as a literal payload key (`user_id`), a dotted path (`user.id`), or the same path with a leading `data.` prefix (`data.user_id`). A request that resolves **none** of the declared fields bypasses the cache entirely: the workflow runs, nothing is stored, and Orion logs a warning naming the channel and fields — it almost always means the names do not match the payload shape.

> [!WARNING]
> Request headers are never part of the cache key. A cached entry is shared by every caller whose method, route, query, and payload agree, whatever headers they sent. If a response varies by anything a header carries, that value must appear in the payload and in `cache_key_fields`, or the channel must not cache.

Behaviour:

- A hit is served without executing the workflow and without consuming a backpressure permit.
- A replayed idempotency key answers `409` before the cache is consulted.
- The response is stored on success and expires after `ttl_secs`.
- A cached [shaped](#response-shaping) response replays its status and headers, not just its body.
- A write-gated cache connector is refused for the response cache; see [operation gates](./connectors.md).

**Cluster mode.** With the shared cluster Redis, hits are shared across replicas. A channel whose cache connector is missing, broken, or explicitly in-memory refuses to load; on a single node it falls back to process memory with a warning.

## Request body

`request` controls how the HTTP request body becomes `data` and `metadata`. **HTTP ingresses only**: Kafka parses the whole payload as `data` and builds metadata separately, and `channel_call` inherits the parent's metadata with `data` from the task input, so neither is affected.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `body_mode` | string | no | `"auto"` | `auto` detects the Orion envelope; `payload` takes the parsed body verbatim. |
| `cookies_to_metadata` | array of strings | no | — | Named request cookies copied to `metadata.cookies.*`. Absent exposes nothing. |

Under `auto`, **an object carrying a top-level `data` or `metadata` key is the envelope**: that key becomes the payload and every sibling field is discarded. Anything else (an array, a scalar, an object without those keys) is the payload as it stands, and an empty body is `{}`.

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
- **`orion-cli send` needs `--raw` for a payload-mode channel.** The CLI wraps its argument in `{"data": …}` by default, which a payload-mode channel then delivers as `data = {"data": …}`. `orion-cli send my-channel --raw -d '…'` sends the payload verbatim. Because such a channel accepts no caller metadata, `--raw` and `--metadata` are refused together rather than one being dropped.

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

**Scope it to opaque identifiers a workflow matches against its own stored state**: a browser-pinning id, a first-party visitor id, a bucket cookie. For a session token, JWT or CSRF token use [`auth.mode: "jwt"`](#authentication) with `source: {"cookie": …}` instead, where the token is consumed at verification rather than copied into the context.

> [!WARNING]
> **Allowlisted values land in `traces.result_json` and `trace_dlq.metadata_json` unmasked.** The read side is covered — `GET /admin/traces/{id}` strips all of `context.metadata`, but the row on disk is not. Note also that `tracing.mode = "off"` suppresses only *sync* persistence: on an `/async` channel the row is still written before the `202`, so turning tracing off is **not** a complete mitigation there. `trace_queue.retention_hours` is the ageing-out control.

Two further limits worth knowing:

- **A cookie-varying channel must not enable `cache`.** `compute_cache_key` hashes method, params, query and payload — never headers, so a cached response would replay one caller's `Set-Cookie` to the next.
- **`rate_limit.key_logic` still cannot see cookies.** Its context is `{client_ip, channel, headers}`, and `cookie` is not among the readable headers. Per-cookie rate limiting stays out of reach. It cannot see the authenticated principal either, because it runs before authentication — [`principal_rate_limit`](#per-principal-quotas-principal_rate_limit) is the block that can.

`channel_call` propagates metadata verbatim, so an allowlisted cookie reaches sub-channels — the same way verified claims do.

## Error bodies

Every ingress guard rejection answers with the platform envelope `{"error": {"code", "message", "request_id"}}`. `response.error_bodies` lets a channel replace those **bytes**: for a migrated API whose deployed clients parse a different shape. **The platform still decides the status.**

```json
{
  "config": {
    "response": {
      "error_bodies": {
        "default": { "body": "{\"errorCode\":\"{status}\",\"message\":\"{message}\"}" },
        "401": { "body": "{\"status\":401,\"error\":\"SESSION_EXPIRED\",\"message\":\"{message}\"}" },
        "429": { "body": "…", "content_type": "application/json" }
      }
    }
  }
}
```

Keys are HTTP statuses (`400`–`599`) plus an optional `"default"`. `error_bodies` is **independent of `mode`**: an `envelope` channel can use it, since the two settings answer different questions and `mode` covers only the success path.

| Placeholder | Value |
|---|---|
| `{status}` / `{code}` | The status and the stable error code |
| `{message}` | The platform's message — already redacted, since it comes from the same chokepoint the envelope uses |
| `{request_id}` | The correlation id the envelope carries |
| `{channel}` | The resolved channel name |
| `{timestamp}` | RFC 3339, milliseconds, UTC |

A placeholder is `{` + a lowercase identifier + `}` and nothing else, so ordinary JSON braces need no escaping; write `{{` and `}}` for a literal brace pair. An **unknown** placeholder is refused at authoring time rather than shipped as a literal — a misspelled `{mesage}` is a body that would be wrong forever.

Applies to the fourteen ingress guard rejections — rate limit (`429`, `503`), auth (`401`, `403`), origin (`403`), validation (`400`), dedup (`409`, `503`) and backpressure (`503`) — on both the sync and `/async` paths, which run the same guards.

**Not shapeable:** `413` (produced by the body extractor before any channel is known), the global rate-limit `429`, CORS preflights, and pre-resolution rejections (`404`, `415`, malformed JSON). Post-guard errors (`504`, `500`) are out of scope for now.

> [!IMPORTANT]
> **No cause-selectable bodies.** Keying is by status, never by *why* a request was refused. A uniform `401` is an anti-oracle: the response never reveals whether the header was missing, the key was wrong, the signature was malformed or the timestamp was stale. Keying by cause would rebuild exactly that credential oracle. Status is also the honest key — two rejections already share `RATE_LIMITED` and three share `SERVICE_UNAVAILABLE`.

Three further guarantees:

- **Error-owned headers survive.** `retry-after` on a `429` and `WWW-Authenticate` on a refused token are attached by the error itself and are preserved when the body is replaced.
- **Metrics and traces are unaffected.** Rejection counters fire before the response is built, so an operator loses no visibility when a channel changes its bytes.
- **Soft failure.** A template that no longer renders falls back to the platform envelope rather than 500ing — a cosmetic authoring slip must not become an outage. Templates are capped at 4 KiB so a refusal cannot become an amplification primitive, and there is no JSONLogic: there is no engine at guard time, and evaluating expressions over attacker-influenced input on the cheapest-must-be path would be new attack surface for no gain.

## Response shaping

By default every sync channel answers `200` with the fixed envelope `{id, status, data, errors}`, whatever happened. See [Errors & Response Envelopes](./errors.md). That is a workable contract between workflows and an awkward one for a REST API: no `201` with a `Location`, no `404`, no content type but JSON.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `response.mode` | string | no | `"envelope"` | `envelope` or `shaped`. |
| `response.allowed_headers` | array of strings | no | default allowlist | Headers the workflow may set. **Replaces** the default list, so a channel can narrow it as well as widen it. Case-insensitive. |
| `response.cookies` | boolean | no | `false` | Whether the workflow may set cookies through `data._orion.response.cookies`. Independent of `allowed_headers` — see [Cookies](#cookies) below. |

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
| `headers` | object | no | `{}` | Response headers, subject to the allowlist below. A value may be a string, or an **array of strings** to send the header once per element. |
| `cookies` | array of objects | no | `[]` | Cookies to set. Requires `response.cookies` on the channel — see [Cookies](#cookies). |
| `body_path` | string | no | whole document | Field to send instead of the entire data document. A leading `data.` is optional. |
| `raw` | boolean | no | `false` | Send a string field verbatim rather than as a JSON string — how a channel returns CSV, XML, or plain text. |

`Content-Type` is `application/json` unless the workflow sets it.

**Header allowlist.** With no `allowed_headers`, a workflow may set `content-type`, `location`, `cache-control`, `etag`, `last-modified`, `retry-after`, `content-language`, and `link`. The hop-by-hop headers (`connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`), `content-length`, and `x-request-id` are refused even when listed — response framing belongs to the server, and `x-request-id` correlates a response with its stored trace. A dropped header does not fail the request.

**Repeated headers.** The first value for a name replaces whatever the platform set, so a workflow's `content-type` still wins, and every later value is appended beside it. That is what makes an array meaningful.

**Failures are soft.** A shaped channel whose workflow sets no control block, or an unusable one, falls back to the standard envelope rather than erroring.

**Interactions.** A cached shaped response replays its status and headers, not just its body — *unless* it sets a cookie, which is never cached. Profiling (`?profile=1`) appends `_orion.profile` to the envelope only — a shaped body is the workflow's own — though timings still reach the trace and metrics. Shaping applies to the synchronous path only; [`/async`](./data-api.md#asynchronous-processing) answers `202` with a trace id as always.

### Cookies

Reading cookies is configured per channel with `request.cookies_to_metadata`. Writing them is the mirror: a shaped channel sets `response.cookies` to `true`, and its workflow declares them rather than assembling the attribute string by hand.

```json
{ "path": "data._orion.response.cookies", "logic": [
  { "name": "session", "value": { "var": "temp_data.jwt" },
    "path": "/", "http_only": true, "secure": true,
    "same_site": "Lax", "max_age": 2592000 },
  { "name": "oauth_state", "value": "", "path": "/", "max_age": 0 }
] }
```

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Cookie name. Must be an RFC 6265 token — no spaces, `=`, `;` or separators. |
| `value` | string | yes | Cookie value. May be empty, which with `max_age: 0` is how a cookie is cleared. |
| `path` | string | no | `Path` attribute. |
| `domain` | string | no | `Domain` attribute. |
| `max_age` | number | no | `Max-Age` in seconds. `0` expires the cookie immediately. |
| `expires` | string | no | `Expires` attribute, as an HTTP date. |
| `same_site` | string | no | `Strict`, `Lax` or `None`. Case-insensitive, emitted canonically. |
| `http_only` | boolean | no | Adds `HttpOnly`. A `false` emits nothing — the attribute has no negative form. |
| `secure` | boolean | no | Adds `Secure`. |

**Why its own switch, not `allowed_headers`.** That list *replaces* the default one, so gating cookies on it would mean a channel setting a session cookie also has to re-list `content-type` to keep serving JSON. The raw escape hatch still works — list `set-cookie` in `allowed_headers` and write the header directly, with an array for more than one, but the declared form is what validates the value and spells the attributes for you.

**A response that sets a cookie is never cached.** The response cache keys on the method, path parameters, query and payload — never on who is calling, so a stored `Set-Cookie` would be replayed to every caller repeating that request for the TTL. Orion suppresses the cache write instead. This applies however the cookie was set, including through `allowed_headers`.

**Values are validated, and a refusal is reported.** A `value` carrying `;`, a comma, a quote, a backslash, CR or LF is refused, because a workflow interpolating user input into a cookie could otherwise inject further attributes or split the response. `path`, `domain` and `expires` refuse `;`, CR and LF for the same reason, and `secure`/`http_only` must be real booleans — coercing the string `"false"` to `true` would be worse than refusing it.

As everywhere on this path the failure is **soft**: the cookie is dropped and the rest of the response still ships with its declared status. It is not **silent**. Every dropped declaration — an invalid cookie, a header the channel does not allow, a header value that is not a string, cookies declared with the switch off — appends a `{code, message, path}` entry to the response envelope's `errors` and increments [`orion_response_drops_total`](./metrics.md). A shaped channel's body belongs to its workflow, so the entries do not reach the caller there; they reach the **trace**, which is where a `302` that quietly did not set a session cookie is otherwise indistinguishable from a browser having refused it. `orion-server clippy` catches the statically decidable half ([`correctness.response_cookie_type`](./clippy.md)).

## Inbound OAuth2 Sign-In

`oauth2_login` makes a channel the relying party in a browser OAuth2 authorization-code grant (RFC 6749 §4.1) — "Sign in with GitHub", "Continue with Google". Orion owns the redirect, the state cookie, the CSRF binding, PKCE, the code exchange and, for OIDC, `id_token` verification. The workflow keeps the application half and receives the grant at `metadata.oauth`.

This is *establishment*, not verification, which is why it is a `config` block rather than a fourth [`auth.mode`](#authentication). The two compose: `oauth2_login` mints a session, and `auth.mode = "jwt"` with `source: {"cookie": …}` guards every route the session then reaches.

**The channel serves two routes.** Its `route_pattern` is the authorize leg, where you send a user to begin, and `callback_path` is where the identity provider sends the browser back. Both are gated for collisions at activation, like any other route.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `authorize_url` | string | yes | — | The provider's authorization endpoint. `https` only. Literal, `var://name`, or `env://NAME` / `vault://…` resolved at load. |
| `token_url` | string | yes | — | The provider's token endpoint. `https` only, and address-checked on every exchange unless [`oauth2_login.allow_private_token_urls`](./configuration.md#inbound-oauth2-sign-in) is set. Literal, `var://name`, or `env://NAME` / `vault://…` resolved at load. |
| `client_id` | string | yes | — | The OAuth2 client identifier. Literal, `var://name` for a per-environment value, or `env://NAME`. |
| `client_secret` | string | yes | — | The client secret. `env://NAME` or `vault://…`; a literal works but puts the secret in the stored definition. |
| `client_auth` | string | no | `basic` | How credentials are presented at the token endpoint: `basic` (RFC 6749 §2.3.1) or `body`. |
| `redirect_uri` | string | yes | — | The absolute redirect URI registered with the provider, `https` only. Sent on both legs, because RFC 6749 §4.1.3 requires them to match. It differs on every environment, so `var://name` (or `env://NAME` / `vault://…`, resolved at load) is the usual spelling. |
| `callback_path` | string | yes | — | The callback route, as a second path on this channel. Static — no `{param}` segments — and must differ from `route_pattern`. |
| `scopes` | array of strings | no | `[]` | Requested scopes, space-joined. Empty sends no `scope` parameter. |
| `extra_authorize_params` | object | no | `{}` | Extra query parameters on the authorize URL (`prompt`, `hd`, `allow_signup`). Naming a reserved parameter is a create-time error — see below. |
| `pkce` | boolean | no | `true` | PKCE (RFC 7636), S256 only. `plain` is not representable. |
| `state_secret` | string | yes | — | HS256 key for the state cookie. `env://NAME` or `vault://…`, at least 32 bytes. Must be identical on every node. |
| `state_cookie` | object | no | see below | The state cookie's attributes. |
| `run_workflow_on_authorize` | boolean | no | `false` | Run the workflow on the authorize leg before the redirect is built. |
| `return_to` | object | no | — | `{param, allow_list}` — carry a pre-login destination through the flow. |
| `id_token` | object | no | — | OIDC `id_token` verification. Absent is plain OAuth2. |

`state_cookie` fields: `name` (default `orion_oauth_state`), `secure` (default `true`), `same_site` (default `lax`), `path` (default `/`), and `max_age` in seconds (default `600`), which is also the state token's expiry — the window a user has to finish the consent screen. `max_age` must be between `1` and `86400` (24 hours); it sizes one consent screen, not a session, and a long one keeps a replayable state token valid for as long as it lasts.

`id_token` fields: `issuer` (required, accepted `iss` values), `jwks_url` (required, `https`), `audience` (defaults to `[client_id]`, per OIDC Core §3.1.3.7), `algorithms` (default `["RS256"]`), `required` (default `true`), and `nonce` (default `true`).

**Per-environment values.** Any value in the block may be `var://name`, substituted from the instance's `[vars]` when the channel loads. `env://NAME` and the vault schemes are resolved in `client_id`, `client_secret`, `state_secret`, `authorize_url`, `token_url` and `redirect_uri` only; a secret reference anywhere else is refused at create, because nothing would resolve it and its text would reach the provider. Create-time validation checks what it can see and defers a reference it cannot; the `https` rule and the rest of the shape are applied to the resolved value at load, and a value that fails them quarantines the channel rather than serving it.

### A complete channel

```json
{
  "channel_id": "github-signin",
  "protocol": "rest",
  "methods": ["GET"],
  "route_pattern": "/v1/auth/github",
  "workflow_id": "github-signin",
  "config": {
    "response": { "mode": "shaped", "cookies": true },
    "oauth2_login": {
      "authorize_url": "https://github.com/login/oauth/authorize",
      "token_url": "https://github.com/login/oauth/access_token",
      "client_id": "var://github_client_id",
      "client_secret": "env://GITHUB_CLIENT_SECRET",
      "redirect_uri": "var://github_redirect_uri",
      "callback_path": "/v1/auth/github/callback",
      "scopes": ["read:user"],
      "state_secret": "env://ORION_SECRET_OAUTH_STATE"
    }
  }
}
```

`GET /api/v1/data/v1/auth/github` answers `302` to GitHub with a signed state cookie. GitHub redirects back to the callback, Orion verifies the state and exchanges the code, and the workflow runs with the grant in hand:

```json
[
  { "id": "identify", "function": { "name": "http_call", "input": {
      "connector": "github-api", "method": "GET", "path": "/user",
      "headers": { "authorization": { "cat": ["Bearer ", { "var": "metadata.oauth.access_token" }] } },
      "output": "temp_data.gh" } } },
  { "id": "upsert",  "function": { "name": "db_write",  "input": { "…": "upsert the user" } } },
  { "id": "session", "function": { "name": "jwt_sign",  "input": { "…": "mint the app's own token" } } },
  { "id": "respond", "function": { "name": "map", "input": { "mappings": [
      { "path": "data._orion.response", "logic": { "status": 302, "headers": { "location": "/" } } },
      { "path": "data._orion.response.cookies", "logic": [
        { "name": "session", "value": { "var": "temp_data.token" }, "path": "/",
          "http_only": true, "secure": true, "same_site": "Lax", "max_age": 2592000 } ] } ] } } }
]
```

### What the workflow receives

| Path | Present when |
|---|---|
| `metadata.oauth.access_token` | always |
| `metadata.oauth.token_type` | always (`Bearer`) |
| `metadata.oauth.expires_in` | the provider returned one |
| `metadata.oauth.scope` | the provider returned one |
| `metadata.oauth.refresh_token` | the provider returned one |
| `metadata.oauth.id_token` | the provider returned one |
| `metadata.oauth.claims` | `id_token` verification is configured and a token was verified |
| `metadata.oauth.return_to` | `return_to` is configured and the caller supplied a permitted value |

`metadata.oauth` is platform-reserved: it is stripped from every caller-supplied envelope and written only by Orion, so a workflow reading it is reading a verified grant. It is also excluded from persisted task-detail snapshots, so the tokens in it are not written to disk.

### Reserved authorize parameters

`extra_authorize_params` may not set `client_id`, `redirect_uri`, `response_type`, `scope`, `state`, `nonce`, `code_challenge` or `code_challenge_method`. Naming one is a create-time `400`; a workflow contributing one under `run_workflow_on_authorize` has it ignored with a warning. Overriding `state` would disable the CSRF binding the block exists to provide.

### `run_workflow_on_authorize`

Off by default, the channel answers the redirect itself and the workflow is never entered, which is what makes the CSRF binding and the nonce unskippable.

Turn it on and the workflow runs first. It can refuse the sign-in by shaping its own `data._orion.response` (an unknown tenant, a maintenance window), or contribute to the redirect:

```json
{ "path": "data._orion.oauth2.authorize", "logic": {
    "extra_params": { "login_hint": { "var": "metadata.query.email" } },
    "scopes": ["read:user", "user:email"] } }
```

Orion still mints the state, the nonce and the PKCE challenge. The workflow cannot reach them and cannot replace them.

### `return_to`

```json
"return_to": { "param": "next", "allow_list": ["https://app.example.com/"] }
```

The value is read from that query parameter on the authorize leg, checked against the allow-list **there**, sealed into the signed state, and handed back at `metadata.oauth.return_to`. Checking on the way in is what makes it safe to redirect to: a value that reaches the workflow has already passed. A value that has not is dropped silently. This is the one part of the flow a workflow cannot do for itself, because it never sees the authorize request.

An entry admits a candidate when the two have the **same origin**: scheme, host and port all equal, and the candidate's path is the entry's path or lies beneath it at a `/` boundary:

| Allow-list entry | Admits | Refuses |
|---|---|---|
| `https://app.example.com` | anything on that host | `https://app.example.com.evil.test/steal` |
| `https://app.example.com/app` | `/app`, `/app/home` | `/application`, `/other` |

The match is on origin and path segments rather than on the text, so the trailing slash is not load-bearing and a host that merely *starts with* a permitted one is a different origin. Relative values (`/dashboard`) are not accepted; entries and candidates are both absolute URLs.

### What is refused, and why

- **`cache` alongside `oauth2_login`.** The response cache keys on the request and never on the caller, so a stored authorize `302` would replay one browser's state cookie to the next visitor and a stored callback would replay one user's session.
- **`same_site: "strict"` on the state cookie.** The callback is a top-level cross-site `GET` from the provider, so a `Strict` cookie is withheld on exactly that request and every sign-in fails the state check.
- **A non-`rest` protocol, or no `route_pattern`.** Both legs are routes; a channel reachable only by name has nowhere for the provider to send the browser back to.
- **`callback_path` equal to `route_pattern`.** They are two different requests and must be two different paths.
- **A `.../callback/async` submission.** `202` with a trace id is not a response a browser redirect can follow, and admitting it would run the workflow with no grant — a sign-in that appears to succeed and established nothing.

### Failures

Every callback refusal — a missing, expired, forged or mismatched state, a failed nonce check, a rejected `id_token`, a spent code, a user who pressed Cancel — answers the same `401` with the same body. Naming the failing half would tell a prober which one to work on. The distinction lives in the log and in [`orion_oauth_login_total{outcome}`](./metrics.md), and the body is replaceable per channel with [`response.error_bodies`](#error-bodies). An unreachable identity provider answers `503` with `Retry-After`.

### Limits

Single use is enforced by clearing the state cookie on the callback, not by a stored row, so two concurrent replays of one callback inside the window would both pass Orion's check. The authorization code itself is single-use at the provider, which is where that defence lives. Implicit and hybrid flows, the device-code grant, RP-initiated logout and end-user refresh-token rotation are all out of scope.

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

**Context.** The expression evaluates against exactly `{ "data": …, "metadata": … }`. `data` is the request payload as submitted — the guard runs before any workflow task, so `data.order_id` resolves here even though the workflow itself reads the payload only after `parse_json`. `metadata` has the same shape the workflow's data context carries (headers, query, path params, channel name; transport-dependent). See the [Workflow Schema](./workflows.md). On Kafka, `metadata` carries the record coordinates and no headers.

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

The value governs every ingress. Where the channel declares none, each ingress falls back to its own server-level default, and on two ingresses that server value is a **ceiling** the channel value is clamped to, never a mere default:

| Ingress | Channel declares none | Channel declares more than the transport allows |
|---|---|---|
| synchronous HTTP | runs to completion | honored — nothing else waits on it |
| `/async` | `trace_queue.processing_timeout_ms` | clamped to `trace_queue.processing_timeout_ms` |
| Kafka | `kafka.processing_timeout_ms` | clamped to `kafka.processing_timeout_ms` |
| `channel_call` | `engine.default_channel_call_timeout_ms` | honored — the calling task's own `timeout_ms` outranks it anyway |

<details><summary>Why the two clamps</summary>

On those paths the deadline protects something shared. A Kafka dispatch blocks the consumer's poll loop; a channel asking for ten minutes would push the consumer past librdkafka's `max.poll.interval.ms` and get it evicted from its group mid-record. An `/async` dispatch occupies one of a fixed number of queue workers, so an over-long deadline starves every other channel's queued work. A channel can shorten its deadline everywhere; it can lengthen it only where nothing else depends on it. Raise the transport setting if a channel genuinely needs longer there.

</details>

A `channel_call` task may set its own `timeout_ms`, which outranks the target channel's. See [Task Functions](./functions.md). The server-level settings live in the [Configuration Reference](./configuration.md).

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

- [Data API](./data-api.md): how requests resolve to channels, and what traces carry.
- [Workflow Schema](./workflows.md): the data context these guards feed, and workflow selection.
- [Connector Types](./connectors.md): the cache connectors named by `deduplication.connector` and `cache.connector`.
- [Configuration Reference](./configuration.md): every server-level setting named on this page.
