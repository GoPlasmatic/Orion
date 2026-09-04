<!-- description: Every Orion connector type and its config — http, kafka, db, cache, es, smtp and storage — with secret references, auth schemes and per-operation gates. -->
# Connector Types

Connector validation errors return `400 VALIDATION_ERROR` with field paths.
Workflow execution failures that cannot be exposed return `500 ENGINE_ERROR`,
while an open circuit returns `503 CIRCUIT_OPEN`. Correct the connector definition,
test it through `POST /connectors/{id}/test`, and use [Errors & Response
Envelopes](./errors.md) for the complete contract.

A **connector** is a named connection to an external system — an API, a database, a cache, a Kafka cluster, or a search cluster. Workflows reference connectors by name. Credentials stay in the connector, never in workflow JSON.

There are exactly seven connector types:

| Type | Backs | Task functions |
|------|-------|----------------|
| [`http`](#http) | REST APIs and webhooks | `http_call` |
| [`kafka`](#kafka) | Kafka topics (produce only) | `publish_kafka` |
| [`db`](#db) | PostgreSQL, MySQL, SQLite, MongoDB | `data_query`, `data_write`, `db_read`, `db_write`, `mongo_read`, `mongo_write`, `mongo_aggregate` |
| [`cache`](#cache) | Redis or in-process memory | `cache_read`, `cache_write` |
| [`es`](#es) | Elasticsearch | `data_query`, `data_write` |
| [`smtp`](#smtp) | Transactional email over SMTP | `send_email` |
| [`storage`](#storage) | S3-compatible object storage (presign + metadata only) | `storage_presign`, `storage_head` |

Type values match case-insensitively. Any other value is refused with the valid list. A stored connector whose config no longer parses fails to load and surfaces as a connector load issue.

The task functions and their inputs are specified in the [Function Reference](./functions.md).

## Definition and identity

You create a connector through the [Admin API](./admin-api.md#connectors):

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

- **`name`**: required, at most 255 characters. Workflows and channel stores reference the connector by this name. Connectors are unversioned — an update replaces the stored config.
- **`config`**: the per-type object documented below. Its `type` field selects the shape.
- **`enabled`**: defaults to `true`. A disabled connector is never loaded; export → import preserves the flag ([endpoints](./admin-api.md#connectors)).
- **`tags`**: selection labels for `?tag=` filtering and [package export](../operate/promotion.md).

A read gives the config back **twice**, both copies masked:

| Field | Type | Use |
|---|---|---|
| `config` | object | The shape `POST` and `PUT` accept, so a read response can be edited and written straight back. Read this one. |
| `config_json` | string | The stored document verbatim, as a string. Kept for the life of the 1.x line; a client reading it has to parse the string before it can write it back. |

They are the same document — `config` is parsed *from* the masked string, so it
cannot carry a secret the string form has already replaced. `config` is `null`
only when the stored document no longer parses, the same condition that empties
`content_hash`.

> [!NOTE]
> Connector configs ignore unknown top-level fields, so rows written by older versions keep loading. The `operations`, `retry`, and `dialect` blocks are the exception: each refuses unknown keys, as its section states. A misspelled control would otherwise read as protection while providing none.

## Secrets by reference

Any string field in `config` may hold `env://VAR_NAME` instead of a literal value. Orion resolves the reference each time the connector loads, so credentials never need to be sent to the API or stored in the database. A create or update checks the config's shape, not this host's environment — an unset variable surfaces at the load that follows, where the connector is skipped and its row reports `load_status: "failed"`. [Environment Variables](./environment-variables.md#what-an-unset-variable-does) has the full table.

Name the variables anything you like, with one restriction. Orion [refuses to start](./configuration.md#misspellings-are-startup-errors-not-silent-no-ops) on an `ORION_*` variable that is not one of its own settings. A secret that must live in that namespace needs the reserved prefix: `env://ORION_SECRET_STRIPE_API_KEY`.

`vault://<api-path>#<field>` reads from HashiCorp Vault when `VAULT_ADDR` and `VAULT_TOKEN` are set in the server's environment. The schemes `aws-sm://`, `gcp-sm://`, and `azure-kv://` are reserved. A reference using a reserved scheme without a live resolver is refused — it is never handed to the backend as a literal credential.

<details><summary>Vault reference form</summary>

The api-path is exactly what follows `/v1/` in Vault's HTTP API. A KV v2 secret therefore reads as `vault://secret/data/db#password` — the `data/` segment is KV v2's, not Orion's. Field lookup understands both KV shapes: v2's nested `data.data.<field>` first, then v1's flat `data.<field>`. `VAULT_ADDR` and `VAULT_TOKEN` are re-read on every load, so a renewed token applies at the next reload without a restart.

</details>

## Authentication

`http` and `es` connectors accept an `auth` object. Three schemes carry **static** credentials:

| Scheme | Fields | Example |
|--------|--------|---------|
| `bearer` | `token` | `{ "type": "bearer", "token": "env://API_TOKEN" }` |
| `basic` | `username`, `password` | `{ "type": "basic", "username": "svc", "password": "env://SVC_PASSWORD" }` |
| `apikey` | `header`, `key` | `{ "type": "apikey", "header": "X-API-Key", "key": "env://API_KEY" }` |

The fourth, [`oauth2`](#managed-oauth2), is **managed**: Orion acquires, caches, refreshes, and (under rotation) persists the token itself. `http` connectors only.

`db` and `cache` connectors carry credentials inside their connection URL instead. The `kafka` connector has no credential field; broker authentication is server configuration ([Kafka settings](./configuration.md#kafka)).

### Managed OAuth2

```json
"auth": {
  "type": "oauth2",
  "grant": "refresh_token",
  "token_url": "https://idp.example.com/oauth2/token",
  "client_id": "env://OAUTH_CLIENT_ID",
  "client_secret": "env://OAUTH_CLIENT_SECRET",
  "refresh_token": "env://OAUTH_REFRESH_TOKEN_SEED",
  "scopes": ["api.read"]
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `grant` | string | yes | — | `client_credentials` (server-to-server; tokens re-acquired on expiry), `refresh_token` (rotating service apps), or `account_credentials` (Zoom Server-to-Server OAuth) |
| `token_url` | string | yes | — | The IdP's token endpoint. Gets the same SSRF validation as the connector's own URL (`allow_private_urls` opts out) |
| `client_id` / `client_secret` | string | yes | — | The OAuth client. `client_secret` is masked on reads |
| `client_auth` | string | no | `basic` | How the client authenticates to the token endpoint (RFC 6749 §2.3.1): `basic` (HTTP Basic) or `body` (form parameters) |
| `refresh_token` | string | conditional | — | The bootstrap **seed** — required by the `refresh_token` grant, refused by the other grants. Masked on reads |
| `scopes` | array | no | — | Space-joined into the `scope` parameter |
| `audience` / `resource` | string | no | — | The corresponding token-request parameters (Auth0- / RFC 8707-style IdPs) |
| `account_id` | string | conditional | — | The Zoom account. **Required** by `account_credentials`, refused by every other grant. A tenant identifier rather than a credential, so it stays readable on admin reads and in package diffs |
| `extra_params` | object | no | — | Extra form parameters for provider quirks. Reserved names (`grant_type`, `client_id`, …) are refused; values are masked on reads (use `env://` refs if they must round-trip an export) |
| `refresh_margin_secs` | integer | no | `60` | Refresh this many seconds before expiry (max 3600) |

**Lifecycle.** Tokens are acquired lazily, cached in memory, and refreshed behind the margin — **single-flight**, so concurrent requests wait on the in-flight refresh instead of racing it (the race that, under rotation, invalidates the winner's new token). One acquisition serves every request in the cache window; a 401 from the API drops the cached token so the next call refetches. Token-endpoint outcomes land in the [`orion_oauth_token_requests_total`](./metrics.md) counter, and `POST /api/v1/admin/connectors/{id}/test` acquires a **real token**, validating the whole setup before any workflow depends on it.

**Rotation persistence.** When a refresh response carries a new refresh token, Orion persists it — with the access token and its expiry — to the `connector_oauth_state` table, encrypted when [`storage.connector_encryption_key`](./configuration.md#storage) is set. The connector's own config is never mutated: it stays the declarative seed. The state row is stamped with a fingerprint of the `auth` block, so **editing the connector discards stale state, which is also the recovery story for a burned token: update the connector with a fresh seed, and the seed wins.** In cluster mode a refresh takes a job lease and other nodes adopt the persisted token instead of rotating against each other.

**Failures.** An unreachable token endpoint is retryable and trips the connector's circuit breaker like any outage. A rejection (`invalid_grant`, `invalid_client`) is a non-retryable error naming the OAuth error code, negative-cached for 30 s so a burned token is never retry-looped against the IdP, and it deliberately does not trip the API's breaker (a credential failure says nothing about the API's health).

**Zoom Server-to-Server OAuth** is the `account_credentials` grant — what Zoom moved every server-side integration to when it retired JWT apps in 2023. It exchanges `grant_type=account_credentials` plus the `account_id` with Basic client auth:

```json
{
  "type": "oauth2",
  "grant": "account_credentials",
  "token_url": "https://zoom.us/oauth/token",
  "client_id": "env://ZOOM_CLIENT_ID",
  "client_secret": "env://ZOOM_CLIENT_SECRET",
  "account_id": "env://ZOOM_ACCOUNT_ID"
}
```

Like `client_credentials` it re-acquires from static credentials, so it has no rotation state and takes no cluster lease — everything else in this section (caching, the refresh margin, single-flight, the failure split, the probe) applies unchanged. There is no workflow-level alternative: `http_call` headers are static, so a token a workflow fetched itself could never be attached to a request.

The `password` grant (ROPC) is deliberately absent — removed in OAuth 2.1. Future grants (`jwt-bearer`, token exchange, device code) are new `grant` values, not new auth types.

### Header precedence

When `http_call` builds a request, header layers apply in order. Later layers override earlier ones:

| Priority | Source |
|----------|--------|
| 1 (lowest) | Connector `headers` |
| 2 | Connector `auth` |
| 3 | Default `content-type: application/json` (only when the request has a body) |
| 4 (highest) | Task-level `headers` in the `http_call` input |

Task headers always win. A workflow may override `content-type`, `authorization`, or any header the connector sets.

### Query-parameter precedence

Some APIs authenticate with credentials **in the query string**: legacy SMS and telecom gateways, older payment and lookup APIs. `query_params` is their home:

```json
{
  "type": "http",
  "url": "https://gw.example.com/api.aspx",
  "query_params": {
    "uid": "env://SMS_UID",
    "pwd": "env://SMS_PWD"
  }
}
```

Parameters are applied in this order, and all three layers survive:

| Order | Source |
|-------|--------|
| 1 | The connector `url`'s own query string |
| 2 | A query string on the task's `path` |
| 3 | Connector `query_params` |

**Do not put credentials in the connector `url` instead.** It works, and it fails two ways. Export masks a query value whose *name* looks secret (`pwd` is masked, `pass` is not — the distinction is arbitrary), and re-import then refuses the masked literal, so the connector cannot be promoted between instances. And the resolved URL is interpolated into every timeout and failure message, reaching traces, the DLQ, server logs, OTel spans, the trace read API, which a caller can reach with its own `x-trace-token`, not just an admin, and the admin connector probe's response body.

`query_params` avoids all of that because the values are **never merged into the URL**. They are applied at the request builder, so the SSRF-validated URL and every error message stay credential-free; they cannot ride a cross-host redirect, the same rule headers and auth already follow; and they are percent-encoded, so a secret containing `&`, `=` or a space works where URL interpolation would silently corrupt it.

Two behaviours to know:

- **Order is sorted, not authored.** Parameter order is observable on the wire and matters to signature-based gateways, so the map is stored sorted rather than in an order that would vary per call.
- **A name already present in the connector `url`'s query is refused at authoring time**: the request would otherwise carry it twice with an undefined tie-break.

Values mask on admin reads like header values do, and a `env://`/`vault://` reference survives export → import intact. Use references rather than literals for anything secret: a masked literal cannot be re-imported.

## Operation gates

Every connector type carries an `operations` block that limits what workflows may do through it. Every gate defaults to allowed. A disabled operation turns the call into a validation error naming the operation and the connector — regardless of what any workflow asks for.

| Type | Gate | Blocks |
|------|------|--------|
| `db`, `es` | `read` | `data_query`, `db_read`, `mongo_read`, `mongo_aggregate` |
| `db`, `es` | `insert`, `update`, `delete`, `upsert` | The matching `data_write` operation, and the matching `mongo_write` op (`insert_*` → `insert`, `update_*`/`replace_one` → `update` — or `upsert` when `"upsert": true` — `delete_*` → `delete`) |
| `db`, `es` | `raw_write` | `db_write` — raw SQL cannot be classified per operation |
| `cache` | `read` | `cache_read` |
| `cache` | `write` | `cache_write`, plus channel stores backed by the connector (see below) |
| `kafka` | `publish` | `publish_kafka` |
| `http` | `methods` | Any method not on the allow-list (see below) |
| `storage` | `presign_get`, `presign_put`, `head` | The matching storage function/method |

To make a `db` connector fully delete-proof, disable both `delete` and `raw_write`:

```json
{
  "type": "db",
  "connection_string": "env://ORDERS_DB_URL",
  "operations": { "delete": false, "raw_write": false }
}
```

**`http` gates by method.** An HTTP connector's operation *is* its method, so the gate is an allow-list. Empty — the default — allows every method `http_call` can issue. Naming even one method makes the list exhaustive: `{ "methods": ["GET"] }` locks the connector to reads. Matching ignores case. An entry outside `GET`, `POST`, `PUT`, `PATCH`, `DELETE` is refused on create and update.

**Cache `write` covers channel stores.** A channel's [deduplication store and response cache](./channel-config.md) may name a cache connector, and both write through it. A write-gated connector is refused for those uses: in cluster mode the channel fails to load and says why; on a single node it falls back to process memory with a warning. `read` does not apply to them — the only keys either store reads back are ones Orion wrote. There is no cache `delete` gate, because no workflow function deletes a key.

An `operations` key the connector's type does not have is refused on create and update, naming the key and listing the ones that exist.

---

The sections below list the `config` fields of each type. **Required** `yes` means create and update are refused without the field. `—` in **Default** means the field is optional and unset until you set it.

## `http`

Calls REST APIs and webhooks through [`http_call`](./functions.md#http_call).

```json
{
  "name": "payments-api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.stripe.com/v1",
    "auth": { "type": "bearer", "token": "env://STRIPE_API_KEY" },
    "headers": { "x-source": "orion" }
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `url` | string | yes | — | Base URL for every request through this connector |
| `method` | string | no | `""` | Default HTTP method when the task sets none |
| `headers` | object | no | `{}` | Default headers for every request — see [Header precedence](#header-precedence) |
| `query_params` | object | no | `{}` | Query parameters appended to every request — see [Query-parameter precedence](#query-parameter-precedence). Values are secret-resolvable and masked on reads |
| `auth` | object | no | — | [Authentication](#authentication): `bearer`, `basic`, `apikey`, or managed [`oauth2`](#managed-oauth2) |
| `retry` | object | no | `{"max_retries": 3, "retry_delay_ms": 1000}` | Retry policy — see [Retries](#retries-http-only) |
| `retry_non_idempotent` | boolean | no | `false` | Also retry POST and PATCH — see [Retries](#retries-http-only) |
| `max_response_size` | integer | no | `10485760` | Maximum response body size in bytes (10 MB); a larger response fails the call. Governs a **successful** body only — a non-2xx body contributes at most 512 bytes to the error message, marked `… (truncated)` when cut |
| `allow_private_urls` | boolean | no | `false` | Allow requests to private and internal IP addresses (SSRF protection) |
| `operations` | object | no | all methods allowed | Method allow-list — see [Operation gates](#operation-gates) |

## `kafka`

Produces to Kafka topics through [`publish_kafka`](./functions.md#publish_kafka). The connector is producer-only: consuming is configured under `[kafka]` in the [server config](./configuration.md#kafka), not here.

```json
{
  "name": "event-bus",
  "connector_type": "kafka",
  "config": {
    "type": "kafka",
    "brokers": ["kafka1:9092", "kafka2:9092"],
    "topic": "events"
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `brokers` | array of strings | yes | — | Bare `host:port` entries, no URL scheme. An empty list `[]` publishes through the globally configured `[kafka]` cluster |
| `topic` | string | yes | — | The topic this connector is associated with. Each `publish_kafka` task names its own target topic |
| `allow_private_urls` | boolean | no | `false` | Allow brokers on private and internal IP addresses; entries are checked as host/port pairs (SSRF protection) |
| `operations` | object | no | all allowed | `publish` gate — see [Operation gates](#operation-gates) |

> [!NOTE]
> `publish_kafka` requires `kafka.enabled = true` in the server config, even when the connector names its own brokers.

## `db`

Runs parameterized queries against PostgreSQL, MySQL, SQLite, or MongoDB. The `connection_string` scheme selects the backend: `postgres://`, `mysql://`, `sqlite:`, `mongodb://`, or `mongodb+srv://`. There is no `driver` field.

```json
{
  "name": "orders-db",
  "connector_type": "db",
  "config": {
    "type": "db",
    "connection_string": "env://ORDERS_DB_URL",
    "max_connections": 10,
    "connect_timeout_ms": 5000,
    "query_timeout_ms": 30000
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connection_string` | string | yes | — | Database URL; the scheme selects the backend. Carries credentials, and is always masked in API reads |
| `max_connections` | integer | no | — | Connection pool maximum size |
| `connect_timeout_ms` | integer | no | — | Connection establishment timeout; also caps MongoDB server selection |
| `query_timeout_ms` | integer | no | — | Per-query timeout |
| `allow_private_urls` | boolean | no | `false` | Allow private and internal IP addresses (SSRF protection). Ignored for `sqlite:`, which opens a file |
| `operations` | object | no | all allowed | `read` / `insert` / `update` / `delete` / `upsert` / `raw_write` — see [Operation gates](#operation-gates) |
| `dialect` | object | no | both guards off | [Dialect guards](#dialect-guards) |
| `aggregate_write_stages` | boolean | no | `false` | MongoDB only: permit the `$out`/`$merge` write stages in [`mongo_aggregate`](./functions.md#mongo_aggregate) pipelines. The one default-deny gate — an aggregation must not silently write |

Two ways to talk to it: the portable [`data_query` / `data_write`](./data-dialect.md) dialect, which runs unchanged against SQL, MongoDB, and Elasticsearch; or raw SQL via [`db_read` / `db_write`](./functions.md#db_read).

There is no `retry` field: a statement that timed out may already have been applied, so database calls are never re-driven. See [Retries](#retries-http-only). Bound the call with `connect_timeout_ms` and `query_timeout_ms` instead.

> [!NOTE]
> A `mongodb://` or `mongodb+srv://` scheme makes this a MongoDB connector. `data_query` and `data_write` run against it unchanged (pass a `database` field in the task input); the raw-native surface is the [`mongo_read`](./functions.md#mongo_read) / [`mongo_write`](./functions.md#mongo_write) / [`mongo_aggregate`](./functions.md#mongo_aggregate) trio, whose documents are extended JSON (`$oid`, `$date`, nested shapes).

### Dialect guards

`db` and `es` connectors carry a `dialect` block that bounds what the portable dialect may reach. [Operation gates](#operation-gates) answer *which verbs*; dialect guards answer *which tables*. Both guards default to off.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `require_schema` | boolean | no | `false` | Refuse any `data_query` / `data_write` call without a real [schema](./data-dialect.md): at least one declared entity, and not `"unmapped": "identity"` |
| `allowed_entities` | array of strings | no | `[]` (unrestricted) | Physical table, collection, or index names the dialect may touch. Matched after schema renames apply; covers relation targets and junction tables |

Unknown keys inside `dialect` are refused on create and update.

## `cache`

Key-value storage for lookups, session state, and temporary data, through [`cache_read` / `cache_write`](./functions.md#cache_read).

```json
{
  "name": "session-cache",
  "connector_type": "cache",
  "config": {
    "type": "cache",
    "backend": "redis",
    "url": "env://REDIS_URL"
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `backend` | string | yes | — | `"redis"` or `"memory"` |
| `url` | string | redis only | — | Redis connection URL, carrying credentials when needed: `redis://user:pass@host:6379`. Ignored for `"memory"` |
| `allow_private_urls` | boolean | no | `false` | Allow private and internal IP addresses (SSRF protection). Ignored for `"memory"`, which opens no socket |
| `operations` | object | no | all allowed | `read` / `write` — see [Operation gates](#operation-gates) |

TTL is set per write, via `cache_write`'s `ttl_secs`. There is no connector-level default.

## `es`

An Elasticsearch cluster driven by the portable dialect: [`data_query`](./functions.md#data_query) renders a Query DSL `_search` body; `data_write` renders `_bulk`, `_update_by_query`, `_delete_by_query`, and `_update` calls. Requests go over the shared HTTP client — there is no dedicated ES driver.

```json
{
  "name": "search-cluster",
  "connector_type": "es",
  "config": {
    "type": "es",
    "url": "http://localhost:9200",
    "auth": { "type": "apikey", "header": "Authorization", "key": "env://ES_API_KEY" }
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `url` | string | yes | — | Base URL of the cluster, e.g. `http://localhost:9200` |
| `auth` | object | no | — | [Authentication](#authentication): `bearer`, `basic`, or `apikey` |
| `request_timeout_ms` | integer | no | — | Per-request timeout |
| `allow_private_urls` | boolean | no | `false` | Allow private and internal IP addresses (SSRF protection) |
| `max_response_size` | integer | no | `10485760` | Maximum response body size in bytes (10 MB) |
| `operations` | object | no | all allowed | Same gate set as `db` — see [Operation gates](#operation-gates) |
| `dialect` | object | no | both guards off | [Dialect guards](#dialect-guards) |

There is no `retry` field: the dialect drives `_bulk` and the by-query mutations through this connector as well as `_search`, and none are safe to re-send blind. See [Retries](#retries-http-only). ES-specific dialect semantics (the `_id` rename, forced refresh, capability limits) live in the [Portable Data Dialect](./data-dialect.md#elasticsearch-notes) reference.

## `smtp`

An SMTP server for [`send_email`](./functions.md#send_email): the
lowest-common-denominator mail transport that self-hosted and enterprise
environments already run. Transport, credentials, TLS mode, and the default
sender live here; the message lives on the task.

```json
{
  "name": "mailer",
  "connector_type": "smtp",
  "config": {
    "type": "smtp",
    "host": "smtp.gmail.com",
    "port": 587,
    "tls": "starttls",
    "auth": { "type": "basic", "username": "env://SMTP_USER", "password": "env://SMTP_PASS" },
    "from": "Orion <noreply@example.in>"
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `host` | string | yes | — | Server hostname — a host, not a URL |
| `port` | integer | no | `587` | `587` pairs with `starttls`, `465` with `implicit`, `25` with internal relays |
| `tls` | string | no | `"starttls"` | `starttls` \| `implicit` \| `none`. Certificates validate against the platform trust store — install a private CA at the OS level; there is deliberately no skip-verification knob. `none` is for local dev relays and draws a validation warning |
| `auth` | object | no | `{"type": "none"}` | `none` (unauthenticated smarthost) or `basic` with `username`/`password` (secret references accepted). Future mechanisms are new tagged variants |
| `from` | string | yes | — | Default sender; `addr@example.com` or `Name <addr@example.com>` |
| `allow_from_override` | boolean | no | `false` | Let a task supply its own `from` |
| `allow_private_urls` | boolean | no | `false` | Allow private and internal addresses — the usual opt-in for an internal relay |
| `timeout_ms` | integer | no | `10000` | Per-send timeout (connect + protocol exchange) |

`POST /api/v1/admin/connectors/{name}/test` probes connect + EHLO + TLS +
authentication without sending mail, through the same pooled transport real
sends use. There is no `retry` field, and `send_email` never retries on its
own: SMTP has no idempotency key, so a re-driven timeout is a duplicate
email. See [Retries](#retries-http-only).

## `storage`

S3-compatible object storage for [`storage_presign`](./functions.md#storage_presign)
and [`storage_head`](./functions.md#storage_head): a deliberately
**zero-data-path** surface: presigning is local SigV4 arithmetic over the
connector's credentials, `storage_head` is one bounded metadata request, and
object bytes never move through the runtime. Works against any S3-compatible
store: AWS, Linode/Akamai, Cloudflare R2, Backblaze B2, Wasabi, and
self-hosted Garage / SeaweedFS / RustFS (usually with `force_path_style`).

```json
{
  "name": "media",
  "connector_type": "storage",
  "config": {
    "type": "storage",
    "endpoint": "https://ap-south-1.linodeobjects.com",
    "region": "ap-south-1",
    "bucket": "media-bucket",
    "access_key": "env://S3_ACCESS_KEY",
    "secret_key": "env://S3_SECRET_KEY"
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `provider` | string | no | `"s3"` | Signing scheme. `s3` covers every S3-compatible store; GCS/Azure later are new values |
| `endpoint` | string | yes | — | Base URL, e.g. `https://s3.us-east-1.amazonaws.com` |
| `region` | string | yes | — | SigV4 signing region |
| `bucket` | string | yes | — | The bucket this connector reaches — deliberately connector-owned: a second bucket is a second connector |
| `access_key` | string | yes | — | Access key id (masked on reads — use `env://` references) |
| `secret_key` | string | yes | — | Secret key; literal or `env://VAR` |
| `session_token` | string | no | — | STS temporary-credential token, signed as `X-Amz-Security-Token` |
| `force_path_style` | boolean | no | `false` | Path-style addressing (`endpoint/bucket/key`) — most self-hosted stores want `true` |
| `allow_private_urls` | boolean | no | `false` | Allow a private/internal endpoint for `storage_head`'s network call |
| `timeout_ms` | integer | no | `10000` | `storage_head` timeout; presigning makes no network call |
| `operations` | object | no | all allowed | `presign_get` / `presign_put` / `head` — `presign_put: false` makes a media connector read-only |

`POST /api/v1/admin/connectors/{name}/test` performs one signed HEAD of the
bucket. There is no retry field: presigning is local computation, and
`storage_head` follows the estate rule that only `http` connectors retry.

## Retries (HTTP only)

Only `http` connectors retry. A `retry` block on any other type is refused with 400 on create and update — it would otherwise be silently ignored. No other connector type re-drives a failed call: a call that timed out may already have been applied.

The `retry` object accepts exactly two keys; an unknown key is refused:

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `max_retries` | integer | no | `3` | Retry attempts after the first request. Values above 16 are refused |
| `retry_delay_ms` | integer | no | `1000` | Delay before the first retry, in milliseconds |

The retry loop behaves as follows:

- **Backoff is exponential.** The delay doubles on each attempt and is capped at 60 seconds.
- **The whole loop shares one deadline**: `timeout_ms × (max_retries + 1)`, measured from the first attempt, backoff included. `timeout_ms` is the [`http_call`](./functions.md#http_call) task's timeout.
- **Idempotent methods only**: GET, PUT, and DELETE retry. POST and PATCH retry only when the connector sets `retry_non_idempotent: true` (default `false`); their budget is otherwise exactly one `timeout_ms`.
- **Retryable errors**: HTTP status ≥ 500, `429`, `408`, status `0` (no response), timeouts, and I/O errors. Everything else fails immediately.

> [!WARNING]
> A timed-out POST may already have been applied, so re-sending it can duplicate the side effect. Enable `retry_non_idempotent` only when the endpoint honors an idempotency key the workflow sets in `headers`.

## Secret masking

Connector API reads mask by **allowlist**. A field comes back readable only when its key is on the known-safe list. Every other value — including a secret stored under a key the list never anticipated — returns as `"******"`. Unanticipated secrets fail closed.

- `env://` and `vault://` references pass through unmasked. They are pointers, not secrets, and masking them would break export → import.
- `connection_string` and all header values are always masked.
- Readable URL-shaped values are redacted in band: userinfo and query parameters are stripped.

Exports apply the same masking, so a literal secret does not survive export → import. Author connectors with `env://` references. See [Secrets in an exported bundle](./admin-api.md#secrets-in-an-exported-bundle).

## Circuit breakers

Circuit breakers shed load from a failing dependency. They are global and off by default: the settings live under `[engine.circuit_breaker]` in the [Configuration Reference](./configuration.md#circuit-breaker), not in connector config.

When enabled, breakers behave as follows:

- **One breaker per `channel:connector` pair, per node.** State is in-process and never shared across a cluster.
- **Every connector-backed task function passes through its breaker**, not just `http_call`.
- **Only retryable failures count.** A call the backend rejected — a syntax error, a constraint violation — says nothing about the dependency's health and never trips the breaker.
- **The breaker opens** after `failure_threshold` consecutive retryable failures. While open, calls fail immediately with `503 CIRCUIT_OPEN` ([error codes](./errors.md)).
- **Half-open admits a single probe.** After `recovery_timeout_secs`, one request is let through. Success closes the breaker; failure reopens it.
- **The breaker map is bounded** at `max_breakers` entries with LRU eviction. Eviction prefers a closed victim: evicting an open breaker would re-admit full load to a dependency still known to be broken. Only when every breaker is open does plain LRU apply, with a warning.

List and reset breakers through the [Admin API](./admin-api.md#connectors).

## Related

- [Admin API — Connectors](./admin-api.md#connectors): the endpoints, import/export semantics, and the per-type reachability probe.
- [Function Reference](./functions.md): the task functions that call through each connector type.
- [Portable Data Dialect](./data-dialect.md): the backend-neutral query and write language `db` and `es` serve.
- [Configuration Reference](./configuration.md): circuit-breaker, Kafka, and every other server-level setting.
- [Environment Variables](./environment-variables.md): `${VAR}` versus `env://`, which surfaces resolve which, and what an unset variable does.
