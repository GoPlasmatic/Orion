# Production Features

[← Back to README](../README.md)

## Workflow Versioning

Workflows follow a **draft → active → archived** lifecycle with automatic version tracking:

- **Create** — version 1 is recorded as `draft`
- **Activate** — status changes to `active`, workflow is loaded into the engine
- **New version** — `POST /api/v1/admin/workflows/{id}/versions` creates a new draft version from the active workflow
- **Update** — only draft versions can be updated via `PUT`
- **Archive** — removes the workflow from the engine

All versions are stored in the `workflows` table with incrementing version numbers. Use `GET /api/v1/admin/workflows/{id}/versions` to list the version history.

## Channel Versioning

Channels follow the same draft → active → archived lifecycle:

- **Create** — version 1 as `draft`
- **Activate** — channel becomes available for data processing
- **New version** — `POST /api/v1/admin/channels/{id}/versions` creates a new draft
- **Archive** — removes the channel from routing

## Rollout Management

Control traffic exposure for active workflows with rollout percentages:

```bash
# Activate a workflow at 10% rollout
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/<workflow-id>/status \
  -H "Content-Type: application/json" -d '{"status": "active", "rollout_percentage": 10}'

# Increase rollout to 50%
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/<workflow-id>/rollout \
  -H "Content-Type: application/json" -d '{"rollout_percentage": 50}'

# Full rollout
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/<workflow-id>/rollout \
  -H "Content-Type: application/json" -d '{"rollout_percentage": 100}'
```

## Custom Workflow IDs

Supply your own workflow IDs for stable, human-readable identifiers — ideal for GitOps workflows and infrastructure-as-code:

```json
{
  "id": "high-value-order-alert",
  "name": "High-Value Order Alert",
  ...
}
```

If `id` is omitted, a UUID is generated automatically.

## Fault-Tolerant Pipelines

Set `continue_on_error: true` on a workflow to keep the task pipeline running even if individual tasks fail. Errors are collected in the response rather than halting execution:

```json
{
  "name": "HTTP Call Error Test",
  "continue_on_error": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "req" }
    }},
    { "id": "call", "name": "Call External API", "function": {
        "name": "http_call", "input": {
          "connector": "my-api",
          "method": "POST",
          "body": { "test": true },
          "timeout_ms": 2000
        }
    }}
  ]
}
```

When the external call fails, the response still returns `"status": "ok"` with errors collected:

```json
{
  "status": "ok",
  "data": { "req": { "action": "test-call" } },
  "errors": [
    { "code": "TASK_ERROR", "task_id": "call", "message": "IO error: HTTP request failed..." }
  ]
}
```

Without `continue_on_error`, the same failure would return a hard error and stop the pipeline.

## Draft Workflow Lifecycle

Workflows are created as drafts, allowing validation before activation. Draft workflows remain in the database but are not loaded into the engine — useful for validating AI-generated workflows before activating them.

```bash
# Create a workflow (starts as draft)
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H "Content-Type: application/json" \
  -d '{ "name": "Content Moderation", ... }'

# Dry-run test the workflow
curl -s -X POST http://localhost:8080/api/v1/admin/workflows/<workflow-id>/test \
  -H "Content-Type: application/json" \
  -d '{"data": {"text": "Hello"}}'

# Activate once satisfied
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/<workflow-id>/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
# Workflow is now active — data gets transformed

# Archive to disable
curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/<workflow-id>/status \
  -H "Content-Type: application/json" -d '{"status": "archived"}'
# Workflow is archived — no longer processes data
```

See [API Reference](api-reference.md) for status management endpoints.

## Tag-Based Organization

Tag workflows for filtering and organization:

```json
{ "tags": ["fraud", "high-priority", "v2"] }
```

Filter by tag in the API: `GET /api/v1/admin/workflows?tag=fraud`

## Per-Channel Configuration

Each channel can carry its own runtime configuration via `config_json`:

```json
{
  "rate_limit": { "requests_per_second": 100, "burst": 50 },
  "timeout_ms": 5000,
  "cors": { "allowed_origins": ["https://app.example.com"] },
  "backpressure": { "max_concurrent": 200 },
  "deduplication": { "header": "Idempotency-Key", "retention_secs": 300 },
  "cache": { "enabled": true, "ttl_secs": 60, "cache_key_fields": ["data.user_id"] },
  "validation_logic": { "and": [
    { "!!": [{ "var": "data.order_id" }] },
    { ">": [{ "var": "data.amount" }, 0] }
  ]}
}
```

- **Rate limiting** — per-channel request limits with optional JSONLogic key computation
- **Timeout** — per-channel processing timeout in milliseconds
- **CORS** — per-channel CORS settings
- **Backpressure** — semaphore-based concurrency limit
- **Deduplication** — idempotency key-based duplicate request prevention
- **Caching** — response caching with TTL and configurable cache key
- **Input validation** — JSONLogic expression evaluated against incoming data before processing

## Request Deduplication

Prevent duplicate processing of the same request using idempotency keys:

```json
{
  "deduplication": {
    "header": "Idempotency-Key",
    "retention_secs": 300
  }
}
```

When enabled, the handler extracts the idempotency key from the configured header. If a request with the same key was already processed within the retention window, it returns 409 Conflict instead of re-processing.

## Response Caching

Cache responses for identical requests to reduce redundant workflow execution:

```json
{
  "cache": {
    "enabled": true,
    "ttl_secs": 60,
    "cache_key_fields": ["data.user_id", "data.action"]
  }
}
```

Cache keys are computed from the specified fields. Cached responses are returned directly without executing the workflow. The cache backend is in-memory by default; Redis-backed caching is available when using a cache connector.

## REST Route Matching

Channels with `protocol: "rest"` can define route patterns for RESTful API routing:

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

## Inter-Channel Invocation

Use the `channel_call` task function to invoke another channel's workflow in-process — no HTTP round-trip:

```json
{
  "id": "enrich", "name": "Enrich from lookup channel",
  "function": {
    "name": "channel_call",
    "input": {
      "channel": "lookup-service",
      "data_logic": { "var": "data.order" },
      "response_path": "data.enrichment"
    }
  }
}
```

Features cycle detection and configurable max call depth (default 10) to prevent infinite recursion.

## Dynamic Paths and Bodies

Use `path_logic` and `body_logic` in `http_call` tasks to compute URLs and request bodies dynamically from message data:

```json
{
  "function": {
    "name": "http_call",
    "input": {
      "connector": "my_api",
      "method": "POST",
      "path_logic": { "cat": ["/users/", { "var": "data.user_id" }, "/notify"] },
      "body_logic": {
        "map": [
          ["message", { "var": "data.alert_message" }],
          ["priority", { "var": "data.priority" }]
        ]
      }
    }
  }
}
```

## Admin API Authentication

Protect admin endpoints with bearer token or API key authentication:

```toml
[admin_auth]
enabled = true
api_key = "your-secret-key"
# header = "Authorization"      # Bearer format (default)
# header = "X-API-Key"          # Raw key format
```

When `header` is `"Authorization"`, the key is expected as `Bearer <key>`. For any other header name, the raw key value is matched directly.

## Audit Logging

All admin actions are recorded in the audit log for compliance and debugging:

```bash
curl -s http://localhost:8080/api/v1/admin/audit-logs
```

Each entry captures: principal, action, resource type, resource ID, details, and timestamp.

## Database Backup & Restore

Export and restore the database via the admin API:

```bash
# Export backup
curl -s -X POST http://localhost:8080/api/v1/admin/backup -o backup.json

# Restore from backup
curl -s -X POST http://localhost:8080/api/v1/admin/restore \
  -H "Content-Type: application/json" -d @backup.json
```

## SSRF Protection

HTTP connectors validate URLs to prevent Server-Side Request Forgery. By default, requests to private/internal IP addresses (RFC 1918, loopback, link-local) are blocked. Override per-connector with `allow_private_urls: true` when calling internal services.

## Request ID Propagation

Every request gets a UUID `x-request-id` header — pass your own or let Orion generate one. The ID is propagated to the response for distributed tracing.

## Security Headers

Orion sets the following security headers on all responses:

- `X-Content-Type-Options: nosniff`
- `X-Frame-Options: DENY`
- `Content-Security-Policy: default-src 'none'; frame-ancestors 'none'`
- `Referrer-Policy: strict-origin-when-cross-origin`
- `Permissions-Policy: camera=(), microphone=(), geolocation=()`
- `Strict-Transport-Security` (when TLS is enabled)

## CORS

CORS is enabled by default with permissive settings, so browser-based admin UIs and dashboards work out of the box. Per-channel CORS can be configured via the channel's `config_json`.

## Channel Loading Filters

Control which channels an instance loads using include/exclude patterns:

```toml
[channels]
include = ["orders.*", "payments.*"]    # Only load matching channels
exclude = ["analytics.*"]               # Exclude matching channels
```

This enables topology control — run different Orion instances for different channel groups without changing channel definitions.
