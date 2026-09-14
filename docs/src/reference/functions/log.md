<!-- description: The log task function: emit a structured log line at a chosen level, with a JSONLogic message and JSONLogic-derived structured fields. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `log`

Emits a structured log line. `message` is a JSONLogic expression (a plain string
is valid), and `fields` attaches more JSONLogic-derived key/values.

## Synopsis

```json
{
  "name": "log",
  "input": {
    "message": "Order processed",
    "level": "info",
    "fields": {
      "order_id": {
        "var": "data.order.id"
      }
    }
  }
}
```

## Description

`log` is an engine built-in from dataflow-rs. Orion declares no input schema for it, so a mistake in `input` surfaces when the task runs rather than when the workflow is saved.

**Retry safety:** `pure`. An engine built-in reads and writes the message and nothing else.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `message` | JSONLogic | yes | — | The log message (string literal or expression) |
| `level` | string | no | `"info"` | `trace` \| `debug` \| `info` \| `warn` \| `error` |
| `fields` | object | no | `{}` | Map of name → JSONLogic expression, logged as structured fields |

## Examples

```json
{
  "name": "log",
  "input": {
    "level": "info",
    "message": "Order processed",
    "fields": { "order_id": { "var": "data.order.id" } }
  }
}
```

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Author a workflow](../../guides/author/workflows.md): conditions, mapping and validation in practice.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Workflow definition](../workflows.md#the-data-context): the data context every function reads and writes.
