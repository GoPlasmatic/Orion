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

Sensitive fields (`token`, `password`, `key`, `secret`, `api_key`, `connection_string`) are automatically masked as `"******"` in all API responses. Secrets are stored but never exposed through the API. Workflows reference connectors by name; they never see or embed actual credentials.

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
| `retry` | 3 retries, 1000ms | Retry with exponential backoff |
| `max_response_size` | 10 MB | Maximum response body size to prevent OOM |
| `allow_private_urls` | `false` | Allow requests to private/internal IPs (SSRF protection) |

### Kafka Connector

Produce to Kafka topics:

```json
{
  "name": "event-bus",
  "connector_type": "kafka",
  "config": {
    "type": "kafka",
    "brokers": ["kafka1:9092", "kafka2:9092"],
    "topic": "events",
    "group_id": "orion-producer"
  }
}
```

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
| `kafka.max_inflight` | `100` | Max in-flight messages |
| `kafka.lag_poll_interval_secs` | `30` | Consumer lag polling interval |

### Database Connector (SQL)

Parameterized SQL queries against PostgreSQL, MySQL, or SQLite:

```json
{
  "name": "orders-db",
  "connector_type": "db",
  "config": {
    "type": "db",
    "connection_string": "postgres://user:pass@db-host:5432/orders",
    "driver": "postgres",
    "max_connections": 10,
    "connect_timeout_ms": 5000,
    "query_timeout_ms": 30000
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `connection_string` | required | Database URL (auto-masked in API responses) |
| `driver` | `"postgres"` | Driver type: `postgres`, `mysql`, or `sqlite` |
| `max_connections` | `null` | Connection pool max size |
| `connect_timeout_ms` | `null` | Connection establishment timeout |
| `query_timeout_ms` | `null` | Individual query timeout |
| `retry` | 3 retries, 1000ms | Retry with exponential backoff |

Use `db_read` for SELECT (returns rows as JSON array) and `db_write` for INSERT/UPDATE/DELETE (returns affected count):

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

### Cache Connector

In-memory or Redis cache for lookups, session state, and temporary storage:

```json
{
  "name": "session-cache",
  "connector_type": "cache",
  "config": {
    "type": "cache",
    "backend": "redis",
    "url": "redis://localhost:6379",
    "default_ttl_secs": 300,
    "max_connections": 10
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `backend` | required | `"redis"` or `"memory"` |
| `url` | required (redis) | Redis connection URL |
| `default_ttl_secs` | `null` | Default TTL for cache entries |
| `max_connections` | `null` | Connection pool max size |
| `retry` | 3 retries, 1000ms | Retry with exponential backoff |

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

### Storage Connector (S3/GCS)

S3, GCS, or local filesystem for file operations:

```json
{
  "name": "uploads",
  "connector_type": "storage",
  "config": {
    "type": "storage",
    "provider": "s3",
    "bucket": "my-uploads",
    "region": "us-east-1",
    "base_path": "/data"
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `provider` | required | Storage provider: `s3`, `gcs`, `local` |
| `bucket` | `null` | Bucket or container name |
| `region` | `null` | Cloud region |
| `base_path` | `null` | Base path prefix for all operations |
| `retry` | 3 retries, 1000ms | Retry with exponential backoff |

### MongoDB Connector (NoSQL)

MongoDB document queries with BSON-to-JSON conversion:

```json
{
  "name": "analytics-db",
  "connector_type": "db",
  "config": {
    "type": "db",
    "connection_string": "mongodb://localhost:27017",
    "driver": "mongodb"
  }
}
```

Use `mongo_read` in workflows:

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

## Custom Functions

Orion provides 8 async function handlers that can be used in workflow tasks:

| Function | Description |
|----------|-------------|
| `http_call` | Call external APIs via HTTP connectors |
| `channel_call` | Invoke another channel's workflow in-process (no HTTP round-trip) |
| `db_read` | Execute SELECT queries against SQL connectors |
| `db_write` | Execute INSERT/UPDATE/DELETE against SQL connectors |
| `cache_read` | Read from memory or Redis cache connectors |
| `cache_write` | Write to memory or Redis cache connectors |
| `mongo_read` | Query MongoDB collections |
| `publish_kafka` | Produce messages to Kafka topics |

In addition to async functions, Orion includes a **built-in function library** for data transformation:

| Function | Description |
|----------|-------------|
| `parse_json` | Parse raw payload into structured data |
| `parse_xml` | Parse XML payload into structured data |
| `filter` | Filter arrays using JSONLogic conditions |
| `map` | Transform data with field mappings |
| `validation` | Validate data against JSONLogic rules |
| `log` | Log data at a specified level |

**JSONLogic expressions** power all conditions and dynamic values. Use `{ "var": "data.field" }` to reference data, `{ "cat": [...] }` for string concatenation, arithmetic operators, and more. Dynamic paths (`path_logic`) and bodies (`body_logic`) let you compute URLs and request payloads from message data.

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
