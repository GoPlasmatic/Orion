# Data API

The data API handles runtime request processing: routing messages to channels, executing workflows, and returning results.

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/v1/data/{channel}` | Process message synchronously (simple channel name) |
| `POST` | `/api/v1/data/{channel}/async` | Submit for async processing (returns trace ID) |
| `ANY` | `/api/v1/data/{path...}` | REST route matching: method + path matched against channel route patterns |
| `ANY` | `/api/v1/data/{path...}/async` | Async submission via REST route matching |
| `GET` | `/api/v1/admin/traces` | List traces (payload-free rows). Filter with `?status=`, `?channel=`, `?mode=` |
| `GET` | `/api/v1/admin/traces/{id}` | Poll one trace. Requires the submission's `trace_token` or an admin credential |

> **Note:** the trace *list* is guarded like `/api/v1/admin/*` and `/metrics`
> when admin auth is enabled (`[admin_auth]`), and its rows carry no payloads.
> The single-trace GET follows a two-lane rule instead: a valid admin
> credential always works, and an async submission's `trace_token` grants
> access to that one trace — so data-plane callers can poll their own results
> without holding an admin key. Channel endpoints stay unauthenticated.

## Route Resolution

When a request arrives at `/api/v1/data/{path}`, Orion resolves the target channel in this order:

1. **Async check:** strip trailing `/async` suffix (switches to async mode)
2. **REST route table:** match HTTP method + path against channel `route_pattern` values (e.g., `GET /orders/{order_id}`)
3. **Channel name fallback:** direct lookup by single path segment (e.g., `/api/v1/data/orders` → channel named `orders`)

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

### Per-request profiling

Add `X-Orion-Profile: 1` (or `?profile=1`) to the request and the response gains a `_orion.profile` block that breaks the request down by phase. The header is opt-in so you only pay the cost on the requests you care about, and `tracing.profile_enabled` in config gates the surface entirely. The debug surface always sits under the `_orion` namespace so workflow-produced output keys can never collide with future debug fields.

```json
{
  "status": "ok",
  "data": { ... },
  "errors": [],
  "_orion": {
    "profile": {
      "version": 2,
      "totals_ms": 6.75,
      "phases": [
        { "name": "engine_lock_wait",  "ms": 0.04, "pct": 0.59 },
        { "name": "handlers",          "ms": 5.33, "pct": 78.96 },
        { "name": "workflow_overhead", "ms": 1.34, "pct": 19.85 }
      ],
      "handlers": [
        { "function": "db_read",      "connector": "orders-db", "duration_ms": 4.91, "pct_of_workflow": 73.2 },
        { "function": "channel_call", "connector": "enrich-ch", "duration_ms": 0.42, "pct_of_workflow": 6.3,
          "nested": [ { "function": "cache_read", "connector": "hot", "duration_ms": 0.11, "depth": 1 } ] }
      ],
      "by_function":  { "db_read": { "count": 1, "total_ms": 4.91 } },
      "by_connector": { "orders-db": { "count": 1, "total_ms": 4.91 } }
    }
  }
}
```

`phases[]` is the iterable view — same numbers as the `*_ms` detail fields, so a
client that does not want to hard-code each key can just walk it. `nested[]`
lists the handler calls that ran *inside* a `channel_call`, matched by when they
ran; a call with no children omits the key.

Branch on `version`: **v2** (1.0) corrected `nested[]`, which in v1 attached
every nested sample to every top-level `channel_call`.

## Asynchronous Processing

Append `/async` to submit for background processing:

```bash
curl -s -X POST http://localhost:8080/api/v1/data/orders/async \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-456" } }'
```

**Response:** returns immediately with a trace ID and a capability token:

```json
{
  "trace_id": "550e8400-e29b-41d4-a716-446655440000",
  "trace_token": "b1946ac92492d2347c6235b4d2611184"
}
```

The token is shown once, here — only its hash is stored. It scopes the poll
to this submission: without it (or an admin credential), the trace is not
readable, so one caller can never read another's async result.

**Poll for the result** (header form, or `?token=` for clients that cannot
set headers):

```bash
curl -s http://localhost:8080/api/v1/admin/traces/550e8400-e29b-41d4-a716-446655440000 \
  -H "x-trace-token: b1946ac92492d2347c6235b4d2611184"
```

**Trace statuses:** `pending` → `completed` or `failed`.

## Trace Endpoints

List and filter traces:

```bash
# List all traces
curl -s http://localhost:8080/api/v1/admin/traces

# Filter by channel and status
curl -s "http://localhost:8080/api/v1/admin/traces?channel=orders&status=completed"

# Filter by mode
curl -s "http://localhost:8080/api/v1/admin/traces?mode=async"
```

Get a specific trace (async traces need their `trace_token`; sync traces
follow the admin trust model):

```bash
curl -s "http://localhost:8080/api/v1/admin/traces/{trace-id}?token={trace-token}"
```

List rows are payload-free projections — `input_json`, `result_json` and
`task_trace_json` are served only by the single-trace GET, and the served
message omits the submitter's request context (`context.metadata`).

## Operational Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check (200 OK / 503 degraded). Checks DB, engine, uptime |
| GET | `/healthz` | Kubernetes liveness probe. Always returns 200 |
| GET | `/readyz` | Kubernetes readiness probe. 503 if DB, engine, or startup not ready |
| GET | `/metrics` | Prometheus metrics (when enabled) |
| GET | `/docs` | Swagger UI |
| GET | `/api/v1/openapi.json` | OpenAPI 3.0 specification |
