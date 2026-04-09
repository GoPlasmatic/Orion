# Data API

The data API handles runtime request processing — routing messages to channels, executing workflows, and returning results.

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/v1/data/{channel}` | Process message synchronously (simple channel name) |
| `POST` | `/api/v1/data/{channel}/async` | Submit for async processing (returns trace ID) |
| `ANY` | `/api/v1/data/{path...}` | REST route matching — method + path matched against channel route patterns |
| `ANY` | `/api/v1/data/{path...}/async` | Async submission via REST route matching |
| `GET` | `/api/v1/data/traces` | List traces — filter with `?status=`, `?channel=`, `?mode=` |
| `GET` | `/api/v1/data/traces/{id}` | Poll async trace result |

## Route Resolution

When a request arrives at `/api/v1/data/{path}`, Orion resolves the target channel in this order:

1. **Async check** — strip trailing `/async` suffix (switches to async mode)
2. **REST route table** — match HTTP method + path against channel `route_pattern` values (e.g., `GET /orders/{order_id}`)
3. **Channel name fallback** — direct lookup by single path segment (e.g., `/api/v1/data/orders` → channel named `orders`)

REST routes are matched by priority (descending) then specificity (segment count). Path parameters are extracted and injected into the message metadata.

## Synchronous Processing

Send a POST to the channel name or a matching REST route:

```bash
# By channel name
curl -s -X POST http://localhost:8080/api/v1/data/orders \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-123", "total": 25000 } }'

# By REST route pattern
curl -s -X GET http://localhost:8080/api/v1/data/orders/ORD-123/items/ITEM-1
```

**Response:**

```json
{
  "status": "ok",
  "data": {
    "order": { "order_id": "ORD-123", "total": 25000, "flagged": true }
  },
  "errors": []
}
```

## Asynchronous Processing

Append `/async` to submit for background processing:

```bash
curl -s -X POST http://localhost:8080/api/v1/data/orders/async \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-456" } }'
```

**Response** — returns immediately with a trace ID:

```json
{
  "trace_id": "550e8400-e29b-41d4-a716-446655440000",
  "status": "pending"
}
```

**Poll for the result:**

```bash
curl -s http://localhost:8080/api/v1/data/traces/550e8400-e29b-41d4-a716-446655440000
```

**Trace statuses:** `pending` → `completed` or `failed`.

## Trace Endpoints

List and filter traces:

```bash
# List all traces
curl -s http://localhost:8080/api/v1/data/traces

# Filter by channel and status
curl -s "http://localhost:8080/api/v1/data/traces?channel=orders&status=completed"

# Filter by mode
curl -s "http://localhost:8080/api/v1/data/traces?mode=async"
```

Get a specific trace:

```bash
curl -s http://localhost:8080/api/v1/data/traces/{trace-id}
```

## Operational Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check (200 OK / 503 degraded) — checks DB, engine, uptime |
| GET | `/healthz` | Kubernetes liveness probe — always returns 200 |
| GET | `/readyz` | Kubernetes readiness probe — 503 if DB, engine, or startup not ready |
| GET | `/metrics` | Prometheus metrics (when enabled) |
| GET | `/docs` | Swagger UI |
| GET | `/api/v1/openapi.json` | OpenAPI 3.0 specification |
