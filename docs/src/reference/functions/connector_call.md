<!-- description: The connector_call task function: invoke a v2 connector provider operation (rust, workflow, or wasm) under an owned RunContext. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-24 -->

# `connector_call`

Invokes a v2 connector provider operation (`rust`, `workflow`, or `wasm`) under an owned `RunContext`, enforcing admission, capability grants, reserved-operation denial, and normalized `ConnectorResult` / `ConnectorError` outcomes.

## Synopsis

```json
{
  "name": "connector_call",
  "input": {
    "connection": "orion.foundation.memory-cache",
    "operation": "cache.set",
    "kind": "rust",
    "input": {
      "key": { "var": "data.order_id" },
      "value": { "var": "data.payload" }
    },
    "output": "data.cached"
  }
}
```

## Description

`connector_call` dispatches a public operation on a registered v2 provider backend (`rust`, `workflow`, or `wasm`). Internal lifecycle operations prefixed with `orion.internal.` are forbidden from workflow steps and fail immediately with `CONNECTOR_OPERATION_FORBIDDEN`.

### The foundation SQL and HTTP providers

The Community providers `org.orion.postgresql`, `org.orion.sqlite` and `org.orion.http` take their target from the connection revision, never from `input`. An `input` carrying `connection_string`, `url` or `base_url` is refused with `CONNECTOR_CONNECTION_FORBIDDEN`. A revision's `public_config` names the target (`connection_string`, or `url` for HTTP). A credential may live only behind its `secret_locator`, a reference such as `env://ORDERS_DB_URL`, which resolves to the whole connection string.

A SQL statement (`input.sql`, or `source` for a query and `target` for a write) must be a literal string. A task that builds one from the message, such as `{"sql": {"var": "data.q"}}`, is refused when it loads. The message reaches the database only through `input.params`, an array bound to the statement's placeholders (`$1` on PostgreSQL, `?` on SQLite):

```json
{
  "connection": "orders-db",
  "target_id": "org.orion.postgresql",
  "operation": "query",
  "input": {
    "sql": "SELECT id, total FROM orders WHERE customer_id = $1",
    "params": [{ "var": "data.customer_id" }]
  },
  "output": "data.orders"
}
```

A query's result is held to `query.max_limit` rows and `query.max_result_bytes`, counted as rows stream, the same caps [`db_read`](./db_read.md) has. A transport failure, a serialization failure or a deadlock is `CONNECTOR_UNAVAILABLE` and retryable; any other error the database raises is `CONNECTOR_INVALID_INPUT` and is not. An HTTP response body is capped at 10 MiB.

**Retry safety:** `depends_on` `operation`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connection` | string | yes | — | Logical connection ID or driver identifier |
| `operation` | string | yes | — | Public operation name (e.g. `http.request`, `sql.query`, `cache.get`) |
| `kind` | string | no | `"rust"` | Provider implementation kind (`rust`, `workflow`, or `wasm`) |
| `target_id` | string | no | `connection` | Optional provider target ID (`driver_id` for `rust`, `package_digest` for `workflow`/`wasm`) |
| `input` | object | no | `{}` | Operation input object (`{"var": ...}` references are resolved from the message context) |
| `idempotency_key` | string \| JSONLogic | no | — | Optional idempotency key expression for retry-capable write operations |
| `output` | string \| JSONLogic | no | `"data"` | Dotted context path where the operation result is written |

## Related

- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Retry safety](./retry-safety.md): what a retry of each function costs.
- [Runtime function discovery](./runtime-discovery.md): the live catalogue.
