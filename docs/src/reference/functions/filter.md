<!-- description: The filter task function: gate the pipeline on a JSONLogic condition, halting the workflow or skipping only this task when the condition is falsy. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `filter`

Evaluates a JSONLogic condition. If it is truthy the pipeline continues;
otherwise the `on_reject` action is taken.

## Synopsis

```json
{
  "name": "filter",
  "input": {
    "condition": {
      ">": [
        {
          "var": "data.order.total"
        },
        0
      ]
    },
    "on_reject": "halt"
  }
}
```

## Description

`filter` is an engine built-in from dataflow-rs. Orion declares no input schema for it, so a mistake in `input` surfaces when the task runs rather than when the workflow is saved.

**Retry safety:** `pure`. An engine built-in reads and writes the message and nothing else.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `condition` | JSONLogic | yes | — | Evaluated against the data context |
| `on_reject` | string | no | `"halt"` | `"halt"` stops the whole workflow; `"skip"` skips only this task |

## Examples

```json
{
  "name": "filter",
  "input": {
    "condition": { ">": [{ "var": "data.order.total" }, 0] },
    "on_reject": "halt"
  }
}
```

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Author a workflow](../../guides/author/workflows.md): conditions, mapping and validation in practice.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Workflow definition](../workflows.md#the-data-context): the data context every function reads and writes.
