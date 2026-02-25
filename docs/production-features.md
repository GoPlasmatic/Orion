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

## Paused Rule Lifecycle

Pause rules to temporarily disable processing without deleting them. Paused rules remain in the database and retain their configuration — useful for validating AI-generated rules before activating them.

```bash
# Rule is active — data gets transformed
orion-cli send content -d '{"text": "Hello"}'
# → { "data": { "post": { "text": "Hello", "moderated": true, "status": "reviewed" } } }

# Pause the rule
orion-cli rules pause <rule-id>
orion-cli engine reload

# Rule is paused — data passes through unmodified
orion-cli send content -d '{"text": "Hello"}'
# → { "data": {} }

# Reactivate
orion-cli rules activate <rule-id>
orion-cli engine reload
# Rule is active again
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
