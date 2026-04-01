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
- **Input validation** — JSONLogic expression evaluated against incoming data before processing

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

## Request ID Propagation

Every request gets a UUID `x-request-id` header — pass your own or let Orion generate one. The ID is propagated to the response for distributed tracing.

## CORS

CORS is enabled by default with permissive settings, so browser-based admin UIs and dashboards work out of the box. Per-channel CORS can be configured via the channel's `config_json`.
