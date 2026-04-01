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

Sensitive fields (`token`, `password`, `key`, `secret`, `api_key`) are automatically masked as `"******"` in all API responses. Secrets are stored but never exposed through the API.

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
