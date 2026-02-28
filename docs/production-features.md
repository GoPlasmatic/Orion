# Production Features

[← Back to README](../README.md)

## Rule Versioning

Rules follow a **draft → active → archived** lifecycle with automatic version tracking:

- **Create** — version 1 is recorded as `draft`
- **Activate** — status changes to `active`, rule is loaded into the engine
- **New version** — `POST /api/v1/admin/rules/{id}/versions` creates a new draft version from the active rule
- **Update** — only draft versions can be updated via `PUT`
- **Archive** — removes the rule from the engine

All versions are stored in the `rules` table with incrementing version numbers. Use `GET /api/v1/admin/rules/{id}/versions` to list the version history.

## Rollout Management

Control traffic exposure for active rules with rollout percentages:

```bash
# Activate a rule at 10% rollout
curl -s -X PATCH http://localhost:8080/api/v1/admin/rules/<rule-id>/status \
  -H "Content-Type: application/json" -d '{"status": "active", "rollout_percentage": 10}'

# Increase rollout to 50%
curl -s -X PATCH http://localhost:8080/api/v1/admin/rules/<rule-id>/rollout \
  -H "Content-Type: application/json" -d '{"rollout_percentage": 50}'

# Full rollout
curl -s -X PATCH http://localhost:8080/api/v1/admin/rules/<rule-id>/rollout \
  -H "Content-Type: application/json" -d '{"rollout_percentage": 100}'
```

## Custom Rule IDs

Supply your own rule IDs for stable, human-readable identifiers — ideal for GitOps workflows and infrastructure-as-code:

```json
{
  "id": "high-value-order-alert",
  "name": "High-Value Order Alert",
  "channel": "orders",
  ...
}
```

If `id` is omitted, a UUID is generated automatically.

## Fault-Tolerant Pipelines

Set `continue_on_error: true` on a rule to keep the task pipeline running even if individual tasks fail. Errors are collected in the response rather than halting execution:

```json
{
  "name": "HTTP Call Error Test",
  "channel": "http-test",
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

## Draft Rule Lifecycle

Rules are created as drafts, allowing validation before activation. Draft rules remain in the database but are not loaded into the engine — useful for validating AI-generated rules before activating them.

```bash
# Create a rule (starts as draft)
curl -s -X POST http://localhost:8080/api/v1/admin/rules \
  -H "Content-Type: application/json" \
  -d '{ "name": "Content Moderation", "channel": "content", ... }'

# Dry-run test the rule
curl -s -X POST http://localhost:8080/api/v1/admin/rules/<rule-id>/test \
  -H "Content-Type: application/json" \
  -d '{"data": {"text": "Hello"}}'

# Activate once satisfied
curl -s -X PATCH http://localhost:8080/api/v1/admin/rules/<rule-id>/status \
  -H "Content-Type: application/json" -d '{"status": "active"}'
# Rule is now active — data gets transformed

# Archive to disable
curl -s -X PATCH http://localhost:8080/api/v1/admin/rules/<rule-id>/status \
  -H "Content-Type: application/json" -d '{"status": "archived"}'
# Rule is archived — no longer processes data
```

See [API Reference](api-reference.md) for status management endpoints.

## Tag-Based Organization

Tag rules for filtering and organization:

```json
{ "tags": ["fraud", "high-priority", "v2"] }
```

Filter by tag in the API: `GET /api/v1/admin/rules?tag=fraud`

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

CORS is enabled by default with permissive settings, so browser-based admin UIs and dashboards work out of the box.
