# Production Features

[← Back to README](../README.md)

## Rule Versioning

Every rule change is automatically versioned in a transactional audit trail:

- **Create** — version 1 is recorded
- **Update** — version increments, full snapshot saved
- **Status change** (active/paused/archived) — new version created

All versions are stored in a `rule_versions` table with the complete rule state at that point in time. Versions cascade-delete when the parent rule is removed.

The `GET /api/v1/admin/rules/{id}` response includes `version_count` showing the total number of versions for that rule.

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

Set `continue_on_error: true` on a rule to keep the task pipeline running even if individual tasks fail:

```json
{
  "name": "Multi-step pipeline",
  "continue_on_error": true,
  "tasks": [
    { "id": "step1", "function": "http_call", ... },
    { "id": "step2", "function": "http_call", ... }
  ]
}
```

If `step1` fails, `step2` still executes. Without this flag, the pipeline stops at the first error.

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
  "function": "http_call",
  "config": {
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
```

## Request ID Propagation

Every request gets a UUID `x-request-id` header — pass your own or let Orion generate one. The ID is propagated to the response for distributed tracing.

## CORS

CORS is enabled by default with permissive settings, so browser-based admin UIs and dashboards work out of the box.
