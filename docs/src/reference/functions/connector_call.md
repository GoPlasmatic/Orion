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
