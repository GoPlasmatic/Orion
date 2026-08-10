# Extensibility

Orion integrates with external systems through connectors, exposes custom logic via async function handlers, and supports multiple channel protocols for different ingestion patterns.

## Connectors

Connectors are named external service configurations. Secrets stay in connectors, out of your workflows.

### Authentication

Three auth schemes are supported:

| Auth Type | Fields | Example |
|-----------|--------|---------|
| `bearer` | `token` | `{ "type": "bearer", "token": "sk-..." }` |
| `basic` | `username`, `password` | `{ "type": "basic", "username": "user", "password": "pass" }` |
| `apikey` | `header`, `key` | `{ "type": "apikey", "header": "X-API-Key", "key": "abc123" }` |

### Header Precedence

When `http_call` builds a request, headers are applied in this order (later layers override earlier ones):

| Priority | Source | Example |
|----------|--------|---------|
| 1 (lowest) | Connector default headers | `"headers": {"x-source": "orion"}` in connector config |
| 2 | Connector auth | Bearer token, Basic auth, API key |
| 3 | Default `content-type` | `application/json` (only when a body is present) |
| 4 (highest) | Task-level headers | `"headers": {"content-type": "text/xml"}` in the task input |

Task-level headers always win. This means a workflow developer can override `content-type`, `authorization`, or any other header set by the connector.

### Secret Masking

Connector reads mask by **allowlist**: a field comes back readable only when its key is on the runtime's known-safe list (`type`, `url`, `username`, timeouts, operation gates, and similar). Every other value — including a secret stored under a key the list never anticipated — is returned as `"******"`. `env://` and `vault://` references pass through unmasked (they are pointers, not secrets); `connection_string` and all header values are always masked. Secrets are stored but never exposed through the API. Workflows reference connectors by name; they never see or embed actual credentials.

### Pulling Secrets from the Environment

Any string field inside a connector's `config` may use an `env://VAR_NAME` reference instead of a literal value. The resolver runs once when the connector is loaded:

```json
{
  "name": "payments-api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.stripe.com/v1",
    "auth": { "type": "bearer", "token": "env://STRIPE_API_KEY" }
  }
}
```

If `STRIPE_API_KEY` is not set in the process environment, startup (or the create/update call) fails with a structured error pointing at the field — production credentials never have to be POSTed into the admin API or stored in the database. The same `env://` scheme works on every string field in every connector type.

Name these variables anything you like, with one restriction: Orion refuses to start on an `ORION_*` variable that is not one of its own settings ([why](../configuration/reference.md#misspellings-are-startup-errors-not-silent-no-ops)), so a secret that must live in the `ORION_` namespace needs the reserved `ORION_SECRET_` prefix — `env://ORION_SECRET_STRIPE_API_KEY`. The same applies to a `${VAR}` placeholder inside a connector's `config_json`: unlike a placeholder in the config file, Orion cannot see it while the config is loading (connectors live in the database), so an `ORION_*` name there has to be `ORION_SECRET_*` too.

### HTTP Connector

REST API calls, webhooks, and external service integration:

```json
{
  "name": "payments-api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.stripe.com/v1",
    "auth": { "type": "bearer", "token": "sk-..." },
    "headers": { "x-source": "orion" },
    "retry": { "max_retries": 3, "retry_delay_ms": 1000 },
    "max_response_size": 10485760,
    "allow_private_urls": false
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `url` | required | Base URL for all requests |
| `method` | `""` | Default HTTP method |
| `headers` | `{}` | Default headers applied to every request |
| `auth` | `null` | Authentication config (bearer, basic, or apikey) |
| `retry` | 3 retries, 1000ms | Retry with exponential backoff. Idempotent methods only (GET, HEAD, PUT, DELETE, OPTIONS, TRACE) |
| `retry_non_idempotent` | `false` | Also retry POST/PATCH. A timed-out POST may already have been applied — enable only when the endpoint honours an idempotency key |
| `max_response_size` | 10 MB | Maximum response body size to prevent OOM |
| `allow_private_urls` | `false` | Allow requests to private/internal IPs (SSRF protection) |
| `operations.methods` | `[]` (all) | Method allow-list — see below |

An HTTP connector's [operation gate](#operation-gates) is a method allow-list,
because its operation *is* the method. Empty — the default — allows everything
`http_call` can issue. Name even one method and the list becomes exhaustive, so
a connector pointed at an upstream you must not mutate can be locked to reads
without trusting every workflow that references it:

```json
{
  "name": "partner-api-readonly",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://partner.example.com/v1",
    "operations": { "methods": ["GET"] }
  }
}
```

A `POST` through that connector is refused with a validation error before any
request is made. Matching ignores case, and a method `http_call` cannot issue
(anything outside `GET`, `POST`, `PUT`, `PATCH`, `DELETE`) is rejected when the
connector is created rather than silently never matching.

### Kafka Connector

Produce to Kafka topics:

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

| Field | Default | Description |
|-------|---------|-------------|
| `brokers` | required | Broker list for this connector's cluster |
| `topic` | required | Default topic |
| `operations.publish` | `true` | [Operation gate](#operation-gates) — set `false` to make the connector publish-proof |

The connector is producer-only, so `publish` is its whole gate surface;
consumers are configured under `[kafka]` in the server config.

Use the `publish_kafka` task function with optional JSONLogic for dynamic keys and values:

```json
{
  "function": {
    "name": "publish_kafka",
    "input": {
      "connector": "event-bus",
      "topic": "processed-orders",
      "key_logic": { "var": "data.order_id" }
    }
  }
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `connector` | Yes | Kafka connector name |
| `topic` | Yes | Target topic |
| `key_logic` | No | JSONLogic expression for partition key |
| `value_logic` | No | JSONLogic expression for message value (default: `message.data`) |

**Kafka consumer configuration:** map topics to channels in your config file:

```toml
[kafka]
enabled = true
brokers = ["localhost:9092"]
group_id = "orion"

[[kafka.topics]]
topic = "incoming-orders"
channel = "orders"
```

Async channels with `protocol: "kafka"` can also register topics via the API (DB-driven). Config-file and DB-driven topics are merged; duplicates are deduplicated with config-file entries taking precedence. The consumer restarts automatically on engine reload when the topic set changes.

**Metadata injection:** Kafka metadata is automatically injected into every message:

| Field | Description |
|-------|-------------|
| `kafka_topic` | Source topic name |
| `kafka_key` | Message key (if present) |
| `kafka_partition` | Partition number |
| `kafka_offset` | Offset within partition |

Access these in workflows via `{ "var": "metadata.kafka_topic" }`.

**Dead letter queue:** failed messages are routed to a configurable DLQ topic:

```toml
[kafka.dlq]
enabled = true
topic = "orion-dlq"
```

**Consumer settings:**

| Config | Default | Description |
|--------|---------|-------------|
| `kafka.processing_timeout_ms` | `60000` | Per-message processing timeout |
| `kafka.lag_poll_interval_secs` | `30` | Consumer lag polling interval |

Messages are processed strictly sequentially per consumer — required by the at-least-once commit contract. Scale throughput by running more instances in the same consumer group.

### Database Connector (SQL)

Parameterized SQL queries against PostgreSQL, MySQL, or SQLite:

```json
{
  "name": "orders-db",
  "connector_type": "db",
  "config": {
    "type": "db",
    "connection_string": "postgres://user:pass@db-host:5432/orders",
    "max_connections": 10,
    "connect_timeout_ms": 5000,
    "query_timeout_ms": 30000
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `connection_string` | required | Database URL — the scheme (`postgres://`, `mysql://`, `sqlite:`, `mongodb://`) selects the backend. Carries credentials; auto-masked in API responses |
| `max_connections` | `null` | Connection pool max size |
| `connect_timeout_ms` | `null` | Connection establishment timeout (also caps MongoDB server selection) |
| `query_timeout_ms` | `null` | Individual query timeout |
| `operations` | all allowed | [Operation gates](#operation-gates) — en/disable read / insert / update / delete / upsert / raw_write |

There is no `retry` here: database calls are not re-driven on failure. A
statement that timed out may already have been applied, so a blind re-send
duplicates the write — the same reason `http_call` retries idempotent methods
only. Use `connect_timeout_ms` / `query_timeout_ms` to bound the call, and the
[circuit breaker](resilience.md) to shed load from a backend in trouble.

Two ways to talk to it:

- **Portable dialect** — `data_query` / `data_write` express backend-neutral
  queries and mutations that run unchanged against SQL, MongoDB, or
  Elasticsearch connectors. See the
  [Portable Data Dialect](../reference/data-dialect.md) reference.
- **Raw SQL** — `db_read` for SELECT (returns rows as JSON array) and
  `db_write` for INSERT/UPDATE/DELETE (returns affected count):

```json
{
  "function": {
    "name": "db_read",
    "input": {
      "connector": "orders-db",
      "query": "SELECT * FROM orders WHERE customer_id = $1",
      "params": [{ "var": "data.customer_id" }],
      "output": "data.orders"
    }
  }
}
```

### Operation Gates

**Every** connector type carries per-operation gates — everything defaults to
allowed, and disabling an operation rejects the call with a validation error
naming the op and connector, regardless of what a workflow asks for. The gates
a connector has are the operations it can perform, so the shape differs by
type; this section is the `db` / `es` set, and the [cache](#cache-connector),
[Kafka](#kafka-connector) and [HTTP](#http-connector) sections cover theirs.

```json
{
  "name": "orders-db-readonly",
  "connector_type": "db",
  "config": {
    "type": "db",
    "connection_string": "postgres://user:pass@db-host:5432/orders",
    "operations": { "insert": false, "update": false, "delete": false, "upsert": false, "raw_write": false }
  }
}
```

| Gate | Default | Blocks |
|------|---------|--------|
| `read` | `true` | `data_query`, `db_read`, `mongo_read` |
| `insert` / `update` / `delete` / `upsert` | `true` | The matching `data_write` operation |
| `raw_write` | `true` | The raw-SQL `db_write` escape hatch (raw SQL cannot be classified per-op) |

To make a connector fully delete-proof, disable both `delete` and `raw_write`.

A gate key that the connector's type does not have is a 400 on create and
update, naming the key and listing the ones that exist. Connector configs
otherwise ignore fields they do not know — a row written by an older Orion has
to keep loading — and a gate silently ignored would read as a control while
allowing the operation, so the keys are checked at the door instead.

### Cache Connector

In-memory or Redis cache for lookups, session state, and temporary storage:

```json
{
  "name": "session-cache",
  "connector_type": "cache",
  "config": {
    "type": "cache",
    "backend": "redis",
    "url": "redis://localhost:6379"
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `backend` | required | `"redis"` or `"memory"` |
| `url` | required (redis) | Redis connection URL, including credentials when needed: `redis://user:pass@host:6379` |
| `operations` | all allowed | [Operation gates](#operation-gates) — `read` gates `cache_read`, `write` gates `cache_write` and any channel store backed by the connector |

TTL is set per write, via `cache_write`'s `ttl_secs` — there is no
connector-level default.

A cache connector shared with something else — a Redis holding another
system's keys — can be made read-only in its config, so nothing in Orion
writes through it whatever any workflow or channel asks for:

```json
{
  "name": "shared-redis-readonly",
  "connector_type": "cache",
  "config": {
    "type": "cache",
    "backend": "redis",
    "url": "redis://localhost:6379",
    "operations": { "write": false }
  }
}
```

`write` covers more than `cache_write`: a channel's deduplication store and
response cache may also name a cache connector, and both write through it, so
a write-gated connector is refused for those too — in cluster mode the channel
fails to load and says why; on a single node it falls back to process memory
with a warning, exactly as an unreachable connector does. `read` is not
applied to them, because the only key either store reads back is one Orion
itself wrote.

There is no `delete` gate: the `CacheBackend` trait exposes get / set / set_ex
and the dedup check only, no workflow function deletes a key, and a gate over
an operation that cannot be performed would be a setting that reads as
meaningful and is not.

Use `cache_read` and `cache_write` in workflows:

```json
{
  "function": {
    "name": "cache_write",
    "input": {
      "connector": "session-cache",
      "key": "session:user123",
      "value": { "var": "data.session" },
      "ttl_secs": 3600
    }
  }
}
```

### MongoDB Connector (NoSQL)

MongoDB uses a `db` connector with a `mongodb://` connection string:

```json
{
  "name": "analytics-db",
  "connector_type": "db",
  "config": {
    "type": "db",
    "connection_string": "mongodb://localhost:27017"
  }
}
```

The portable `data_query` / `data_write` functions run against it unchanged
(pass a `database` field in the task input). For raw `find()` filters, use
`mongo_read`:

```json
{
  "function": {
    "name": "mongo_read",
    "input": {
      "connector": "analytics-db",
      "database": "analytics",
      "collection": "events",
      "filter": { "user_id": { "var": "data.user_id" } },
      "output": "data.events"
    }
  }
}
```

### Elasticsearch Connector

A REST endpoint driven by the portable dialect: `data_query` renders an ES
Query DSL `_search` body; `data_write` renders `_bulk` /
`_update_by_query` / `_delete_by_query` / `_update` calls. Executed via the
shared HTTP client — no dedicated ES driver:

```json
{
  "name": "search-cluster",
  "connector_type": "es",
  "config": {
    "type": "es",
    "url": "http://localhost:9200",
    "auth": { "type": "apikey", "header": "Authorization", "key": "ApiKey ..." },
    "request_timeout_ms": 10000
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `url` | required | Base URL of the cluster, e.g. `http://localhost:9200` |
| `auth` | `null` | Authentication config (bearer, basic, or apikey) |
| `request_timeout_ms` | `null` | Per-request timeout |
| `max_response_size` | 10 MB | Maximum response body size to prevent OOM |
| `allow_private_urls` | `false` | Allow private/internal IPs (SSRF protection) |
| `operations` | all allowed | [Operation gates](#operation-gates) |

As with the database connector there is no `retry`: the dialect drives `_bulk`,
`_update_by_query` and `_delete_by_query` through this connector as well as
`_search`, and none of those are safe to re-send blind on a timeout.

ES-specific dialect semantics (the `_id` schema rename, forced refresh for
read-your-writes, capability limits) are documented in the
[Portable Data Dialect](../reference/data-dialect.md#elasticsearch-notes)
reference.

## Custom Functions

Orion provides 10 async function handlers that can be used in workflow tasks:

| Function | Description |
|----------|-------------|
| `http_call` | Call external APIs via HTTP connectors |
| `channel_call` | Invoke another channel's workflow in-process (no HTTP round-trip) |
| `data_query` | Portable, backend-neutral query against SQL / MongoDB / Elasticsearch |
| `data_write` | Portable, backend-neutral insert/update/delete/upsert against SQL / MongoDB / Elasticsearch |
| `db_read` | Execute raw SELECT queries against SQL connectors |
| `db_write` | Execute raw INSERT/UPDATE/DELETE against SQL connectors |
| `cache_read` | Read from memory or Redis cache connectors |
| `cache_write` | Write to memory or Redis cache connectors |
| `mongo_read` | Query MongoDB collections with raw find() filters |
| `publish_kafka` | Produce messages to Kafka topics |

In addition to the Orion-specific handlers, the dataflow-rs 3.1 engine contributes a **built-in function library** for parsing, transformation, and output:

| Function | Description |
|----------|-------------|
| `parse_json` | Parse raw payload into structured data |
| `parse_xml` | Parse XML payload into structured data |
| `filter` | Filter arrays using JSONLogic conditions |
| `map` | Transform data with field mappings |
| `validation` | Validate data against JSONLogic rules |
| `publish_json` | Serialize the data context into a JSON response body |
| `publish_xml` | Serialize the data context into an XML response body |
| `log` | Log data at a specified level |

The Orion-specific handlers have machine-readable input schemas surfaced at `GET /api/v1/admin/functions`; workflow create/update calls validate `function.input` against those schemas with field-pathed errors before the workflow can be activated.

**JSONLogic expressions** power all conditions and dynamic values. Use `{ "var": "data.field" }` to reference data, `{ "cat": [...] }` for string concatenation, arithmetic operators, and more. Dynamic paths (`path_logic`) and bodies (`body_logic`) let you compute URLs and request payloads from message data. Under the hood, datalogic-rs 5 compiles each JSONLogic expression once at engine-construction time and evaluates it via arena-mode dispatch, so per-request cost is constant regardless of expression complexity.

## Channel Protocols

Channels support three protocol modes:

### REST (Sync)

REST channels define route patterns for RESTful API routing with method and path matching:

```json
{
  "name": "order-detail",
  "channel_type": "sync",
  "protocol": "rest",
  "methods": ["GET", "POST"],
  "route_pattern": "/orders/{order_id}/items/{item_id}",
  "workflow_id": "order-detail-workflow"
}
```

Path parameters are extracted and injected into the message metadata. Routes are matched by priority (descending) then specificity (segment count).

### Simple HTTP (Sync)

Simple HTTP channels are matched by channel name. Requests to `/api/v1/data/{channel-name}` are routed directly:

```json
{
  "name": "orders",
  "channel_type": "sync",
  "protocol": "http",
  "workflow_id": "order-processing"
}
```

### Kafka (Async)

Kafka channels consume from topics and process messages asynchronously:

```json
{
  "name": "kafka-orders",
  "channel_type": "async",
  "protocol": "kafka",
  "topic": "incoming-orders",
  "consumer_group": "orion-orders",
  "workflow_id": "order-processing"
}
```

DB-driven Kafka channels are automatically registered as consumers at startup and on engine reload. Add Kafka ingestion via the API without restarting Orion.
