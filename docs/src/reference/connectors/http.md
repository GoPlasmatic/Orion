<!-- description: The `http` connector config: the base URL, default method and headers, query parameters, auth, retries and the response size cap. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `http` connectors

The `config` fields of a `http` connector, which backs REST APIs and webhooks.

Calls REST APIs and webhooks through [`http_call`](../functions/http_call.md).

```json
{
  "name": "payments-api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.stripe.com/v1",
    "auth": { "type": "bearer", "token": "env://STRIPE_API_KEY" },
    "headers": { "x-source": "orion" }
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `url` | string | yes | — | Base URL for every request through this connector |
| `method` | string | no | `""` | Default HTTP method when the task sets none |
| `headers` | object | no | `{}` | Default headers for every request — see [Header precedence](./request-layering.md#header-precedence) |
| `query_params` | object | no | `{}` | Query parameters appended to every request — see [Query-parameter precedence](./request-layering.md#query-parameter-precedence). Values are secret-resolvable and masked on reads |
| `auth` | object | no | — | [Authentication](./authentication.md): `bearer`, `basic`, `apikey`, or managed [`oauth2`](./authentication.md#managed-oauth2) |
| `retry` | object | no | `{"max_retries": 3, "retry_delay_ms": 1000}` | Retry policy — see [Retries](./reliability.md#retries) |
| `retry_non_idempotent` | boolean | no | `false` | Also retry POST and PATCH — see [Retries](./reliability.md#retries) |
| `max_response_size` | integer | no | `10485760` | Maximum response body size in bytes (10 MB); a larger response fails the call. Governs a **successful** body only — a non-2xx body contributes at most 512 bytes to the error message, marked `… (truncated)` when cut |
| `allow_private_urls` | boolean | no | `false` | Allow requests to private and internal IP addresses (SSRF protection) |
| `operations` | object | no | all methods allowed | Method allow-list — see [Operation gates](./operation-gates.md) |

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Request layering](./request-layering.md): the order `headers` and `query_params` apply in.
- [Retries and circuit breakers](./reliability.md): the `retry` block, and what sheds load.
- [Definition and identity](./identity.md): the row the `config` sits in.
