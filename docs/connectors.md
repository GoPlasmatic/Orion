# Connectors

[← Back to README](../README.md)

Connectors are named external service configurations. Secrets stay in connectors — out of your workflows.

## Authentication

Three auth schemes are supported:

```json
{
  "name": "my_api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.example.com",
    "auth": {
      "type": "bearer",
      "token": "sk-secret-token"
    },
    "retry": {
      "max_retries": 3,
      "retry_delay_ms": 1000
    }
  }
}
```

| Auth Type | Fields | Example |
|-----------|--------|---------|
| `bearer` | `token` | `{ "type": "bearer", "token": "sk-..." }` |
| `basic` | `username`, `password` | `{ "type": "basic", "username": "user", "password": "pass" }` |
| `apikey` | `header`, `key` | `{ "type": "apikey", "header": "X-API-Key", "key": "abc123" }` |

## Header Precedence

When `http_call` builds a request, headers are applied in this order — later layers override earlier ones:

| Priority | Source | Example |
|----------|--------|---------|
| 1 (lowest) | Connector default headers | `"headers": {"x-source": "orion"}` in connector config |
| 2 | Connector auth | Bearer token, Basic auth, API key |
| 3 | Default `content-type` | `application/json` (only when a body is present) |
| 4 (highest) | Task-level headers | `"headers": {"content-type": "text/xml"}` in the task input |

Task-level headers always win. This means a workflow developer can override `content-type`, `authorization`, or any other header set by the connector.

## Retry with Exponential Backoff

All HTTP connectors support automatic retries with exponential backoff, capped at 60 seconds:

```json
{
  "retry": {
    "max_retries": 5,
    "retry_delay_ms": 500
  }
}
```

Delay doubles on each retry: 500ms → 1s → 2s → 4s → ... → capped at 60s.

## Secret Masking

Sensitive fields (`token`, `password`, `key`, `secret`, `api_key`, `connection_string`) are automatically masked as `"******"` in all API responses. Secrets are stored but never exposed through the API.

Create a connector with real credentials:

```json
{
  "name": "bearer-auth-api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.example.com/v1",
    "auth": { "type": "bearer", "token": "super-secret-bearer-token-123" }
  }
}
```

Read it back — secrets are masked:

```bash
curl -s http://localhost:8080/api/v1/admin/connectors/<id>
# auth.token → "******"
# auth.password → "******"
# auth.key → "******"
```

Usernames and non-sensitive fields are returned as-is. Workflows reference connectors by name (`"connector": "bearer-auth-api"`) — they never see or embed actual credentials.

## Circuit Breakers

Every connector gets automatic circuit breaker protection. When failures exceed a threshold, the breaker opens and short-circuits requests to prevent cascading failures.

| State | Behavior |
|-------|----------|
| **Closed** | Normal operation — requests flow through |
| **Open** | Requests rejected immediately (503) after failure threshold exceeded |
| **Half-Open** | After cooldown, one probe request allowed to test recovery |

Configure globally via `[engine.circuit_breaker]` in config:

```toml
[engine.circuit_breaker]
enabled = true
failure_threshold = 5
recovery_timeout_secs = 30
```

Inspect and reset breakers via the admin API:

```bash
# List all circuit breaker states
curl -s http://localhost:8080/api/v1/admin/connectors/circuit-breakers

# Reset a specific breaker
curl -s -X POST http://localhost:8080/api/v1/admin/connectors/circuit-breakers/{key}
```

## Connector Types

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

Produce to Kafka topics. Consumer configuration is separate (see [Kafka Integration](kafka.md)).

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

### DB Connector

Parameterized SQL queries against external databases. Supports PostgreSQL, MySQL, and SQLite.

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
    "query_timeout_ms": 30000,
    "retry": { "max_retries": 2, "retry_delay_ms": 500 }
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
| `auth` | `null` | Optional auth config |
| `retry` | 3 retries, 1000ms | Retry with exponential backoff |

Use `db_read` for SELECT queries (returns rows as JSON array) and `db_write` for INSERT/UPDATE/DELETE (returns affected count):

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

In-memory or Redis cache for lookups, session state, and temporary storage.

```json
{
  "name": "session-cache",
  "connector_type": "cache",
  "config": {
    "type": "cache",
    "backend": "redis",
    "url": "redis://localhost:6379",
    "default_ttl_secs": 300,
    "max_connections": 10,
    "retry": { "max_retries": 2, "retry_delay_ms": 200 }
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `backend` | required | `"redis"` or `"memory"` |
| `url` | required (redis) | Redis connection URL |
| `default_ttl_secs` | `null` | Default TTL for cache entries |
| `max_connections` | `null` | Connection pool max size |
| `auth` | `null` | Optional auth config |
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

### MongoDB Connector

MongoDB document queries with BSON-to-JSON conversion.

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

### Storage Connector

S3, GCS, or local filesystem for file read/write operations.

```json
{
  "name": "uploads",
  "connector_type": "storage",
  "config": {
    "type": "storage",
    "provider": "s3",
    "bucket": "my-uploads",
    "region": "us-east-1",
    "base_path": "/data",
    "auth": { "type": "apikey", "header": "Authorization", "key": "..." },
    "retry": { "max_retries": 3, "retry_delay_ms": 1000 }
  }
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `provider` | required | Storage provider: `s3`, `gcs`, `local` |
| `bucket` | `null` | Bucket or container name |
| `region` | `null` | Cloud region |
| `base_path` | `null` | Base path prefix for all operations |
| `auth` | `null` | Optional auth config |
| `retry` | 3 retries, 1000ms | Retry with exponential backoff |
