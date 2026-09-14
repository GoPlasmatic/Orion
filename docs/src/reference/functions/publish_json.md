<!-- description: The publish_json task function: serialize a field of the data context to a JSON string stored at another field, optionally pretty-printed. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `publish_json`

Serializes a field **inside** the data context to a JSON string and stores it at
another field. (It writes back into the context; it does not publish to an
external system.)

## Synopsis

```json
{
  "name": "publish_json",
  "input": {
    "source": "order",
    "target": "order_json",
    "pretty": true
  }
}
```

## Description

`publish_json` is an engine built-in from dataflow-rs. Orion declares no input schema for it, so a mistake in `input` surfaces when the task runs rather than when the workflow is saved.

**Retry safety:** `pure`. An engine built-in reads and writes the message and nothing else.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `source` | string | yes | — | Field under `data` to serialize, for example `"order"` (reads `data.order`) |
| `target` | string | yes | — | Field under `data` to receive the serialized string |
| `pretty` | bool | no | `false` | Pretty-print the JSON output |

## Examples

```json
{ "name": "publish_json", "input": { "source": "order", "target": "order_json", "pretty": true } }
```

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Author a workflow](../../guides/author/workflows.md): conditions, mapping and validation in practice.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Workflow definition](../workflows.md#the-data-context): the data context every function reads and writes.
