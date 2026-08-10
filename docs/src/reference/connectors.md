# Connector Types

A **connector** is a named connection to an external system — an API, a database, a cache, a Kafka cluster, or a search cluster. Workflows reference connectors by name. Credentials stay in the connector, never in workflow JSON.

There are exactly five connector types:

| Type | Backs | Task functions |
|------|-------|----------------|
| [`http`](#http) | REST APIs and webhooks | `http_call` |
| [`kafka`](#kafka) | Kafka topics (produce only) | `publish_kafka` |
| [`db`](#db) | PostgreSQL, MySQL, SQLite, MongoDB | `data_query`, `data_write`, `db_read`, `db_write`, `mongo_read` |
| [`cache`](#cache) | Redis or in-process memory | `cache_read`, `cache_write` |
| [`es`](#es) | Elasticsearch | `data_query`, `data_write` |

Type values match case-insensitively. Any other value is refused with the valid list. A stored connector of a type that no longer exists (such as the removed `storage` type) fails to load and surfaces as a connector load issue.

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

> [!NOTE]
> Connector configs ignore unknown top-level fields, so rows written by older versions keep loading. The `operations`, `retry`, and `dialect` blocks are the exception: each refuses unknown keys, as its section states. A misspelled control would otherwise read as protection while providing none.

## Secrets by reference

Any string field in `config` may hold `env://VAR_NAME` instead of a literal value. Orion resolves the reference each time the connector loads. An unset variable fails the create or update call — or startup — with an error naming the field. Credentials therefore never need to be sent to the API or stored in the database.

Name the variables anything you like, with one restriction. Orion [refuses to start](./configuration.md#misspellings-are-startup-errors-not-silent-no-ops) on an `ORION_*` variable that is not one of its own settings. A secret that must live in that namespace needs the reserved prefix: `env://ORION_SECRET_STRIPE_API_KEY`.

`vault://<api-path>#<field>` reads from HashiCorp Vault when `VAULT_ADDR` and `VAULT_TOKEN` are set in the server's environment. The schemes `aws-sm://`, `gcp-sm://`, and `azure-kv://` are reserved. A reference using a reserved scheme without a live resolver is refused — it is never handed to the backend as a literal credential.

<details><summary>Vault reference form</summary>

The api-path is exactly what follows `/v1/` in Vault's HTTP API. A KV v2 secret therefore reads as `vault://secret/data/db#password` — the `data/` segment is KV v2's, not Orion's. Field lookup understands both KV shapes: v2's nested `data.data.<field>` first, then v1's flat `data.<field>`. `VAULT_ADDR` and `VAULT_TOKEN` are re-read on every load, so a renewed token applies at the next reload without a restart.

</details>

## Authentication

`http` and `es` connectors accept an `auth` object with three schemes:

| Scheme | Fields | Example |
|--------|--------|---------|
| `bearer` | `token` | `{ "type": "bearer", "token": "env://API_TOKEN" }` |
| `basic` | `username`, `password` | `{ "type": "basic", "username": "svc", "password": "env://SVC_PASSWORD" }` |
| `apikey` | `header`, `key` | `{ "type": "apikey", "header": "X-API-Key", "key": "env://API_KEY" }` |

`db` and `cache` connectors carry credentials inside their connection URL instead. The `kafka` connector has no credential field; broker authentication is server configuration ([Kafka settings](./configuration.md#kafka)).

### Header precedence

When `http_call` builds a request, header layers apply in order. Later layers override earlier ones:

| Priority | Source |
|----------|--------|
| 1 (lowest) | Connector `headers` |
| 2 | Connector `auth` |
| 3 | Default `content-type: application/json` (only when the request has a body) |
| 4 (highest) | Task-level `headers` in the `http_call` input |

Task headers always win. A workflow may override `content-type`, `authorization`, or any header the connector sets.

## Operation gates

Every connector type carries an `operations` block that limits what workflows may do through it. Every gate defaults to allowed. A disabled operation turns the call into a validation error naming the operation and the connector — regardless of what any workflow asks for.

| Type | Gate | Blocks |
|------|------|--------|
| `db`, `es` | `read` | `data_query`, `db_read`, `mongo_read` |
| `db`, `es` | `insert`, `update`, `delete`, `upsert` | The matching `data_write` operation |
| `db`, `es` | `raw_write` | `db_write` — raw SQL cannot be classified per operation |
| `cache` | `read` | `cache_read` |
| `cache` | `write` | `cache_write`, plus channel stores backed by the connector (see below) |
| `kafka` | `publish` | `publish_kafka` |
| `http` | `methods` | Any method not on the allow-list (see below) |

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
| `auth` | object | no | — | [Authentication](#authentication): `bearer`, `basic`, or `apikey` |
| `retry` | object | no | `{"max_retries": 3, "retry_delay_ms": 1000}` | Retry policy — see [Retries](#retries-http-only) |
| `retry_non_idempotent` | boolean | no | `false` | Also retry POST and PATCH — see [Retries](#retries-http-only) |
| `max_response_size` | integer | no | `10485760` | Maximum response body size in bytes (10 MB); a larger response fails the call |
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

Two ways to talk to it: the portable [`data_query` / `data_write`](./data-dialect.md) dialect, which runs unchanged against SQL, MongoDB, and Elasticsearch; or raw SQL via [`db_read` / `db_write`](./functions.md#db_read).

There is no `retry` field: a statement that timed out may already have been applied, so database calls are never re-driven — see [Retries](#retries-http-only). Bound the call with `connect_timeout_ms` and `query_timeout_ms` instead.

> [!NOTE]
> A `mongodb://` or `mongodb+srv://` scheme makes this a MongoDB connector. `data_query` and `data_write` run against it unchanged (pass a `database` field in the task input); raw `find()` filters use [`mongo_read`](./functions.md#mongo_read).

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

There is no `retry` field: the dialect drives `_bulk` and the by-query mutations through this connector as well as `_search`, and none are safe to re-send blind — see [Retries](#retries-http-only). ES-specific dialect semantics (the `_id` rename, forced refresh, capability limits) live in the [Portable Data Dialect](./data-dialect.md#elasticsearch-notes) reference.

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

Exports apply the same masking, so a literal secret does not survive export → import. Author connectors with `env://` references — see [Secrets in an exported bundle](./admin-api.md#secrets-in-an-exported-bundle).

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
