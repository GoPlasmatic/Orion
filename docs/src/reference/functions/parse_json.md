<!-- description: The parse_json task function: parse the raw request payload into the data context so later tasks and conditions can read data.* fields. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `parse_json`

Reads a raw value (typically the request payload) and parses it as JSON into the
data context. Almost every workflow starts with this — without it, task
conditions that reference `data.*` see an empty context.

## Synopsis

```json
{
  "name": "parse_json",
  "input": {
    "source": "payload",
    "target": "order"
  }
}
```

## Description

`parse_json` is an engine built-in from dataflow-rs. Orion declares no input schema for it, so a mistake in `input` surfaces when the task runs rather than when the workflow is saved.

**Retry safety:** `pure`. An engine built-in reads and writes the message and nothing else.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `source` | string | yes | — | Where to read the raw value from, for example `"payload"` |
| `target` | string | yes | — | Field name under `data`; the parsed value is stored at `data.{target}` |

## Examples

```json
{ "name": "parse_json", "input": { "source": "payload", "target": "order" } }
```

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Author a workflow](../../guides/author/workflows.md): conditions, mapping and validation in practice.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Workflow definition](../workflows.md#the-data-context): the data context every function reads and writes.
