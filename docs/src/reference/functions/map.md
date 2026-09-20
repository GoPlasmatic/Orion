<!-- description: The map task function: an ordered list of JSONLogic mappings, each writing its result to a dotted path in the data context, for reshaping and enriching. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `map`

Applies an ordered list of JSONLogic expressions, writing each result to a
dotted path in the context. The primary tool for reshaping, computing, and
enriching data.

## Synopsis

```json
{
  "name": "map",
  "input": {
    "mappings": []
  }
}
```

## Description

`map` is an engine built-in from dataflow-rs. Orion declares no input schema for it, so a mistake in `input` surfaces when the task runs rather than when the workflow is saved.

**Retry safety:** `pure`. An engine built-in reads and writes the message and nothing else.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `mappings` | array | yes | — | Ordered list of `{ "path", "logic" }` entries |
| `mappings[].path` | string | yes | — | Dotted target path, for example `"data.order.total"` |
| `mappings[].logic` | JSONLogic | yes | — | Expression whose result is written to `path` |

A mapping whose result is `null` writes nothing: the path keeps the value it had. So `"logic": null` cannot clear a slot. Write `false` instead. [`correctness.mapping_always_null`](../clippy/correctness-mapping-always-null.md) reports a mapping that is always null.

## Examples

```json
{
  "name": "map",
  "input": {
    "mappings": [
      { "path": "data.order.flagged", "logic": true },
      { "path": "data.order.total_with_tax", "logic": { "*": [{ "var": "data.order.total" }, 1.1] } }
    ]
  }
}
```

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Author a workflow](../../guides/author/workflows.md): conditions, mapping and validation in practice.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Workflow definition](../workflows.md#the-data-context): the data context every function reads and writes.
