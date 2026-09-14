<!-- description: The parse_xml task function: parse an XML payload into a JSON structure under data.{target}, with the same source and target fields as parse_json. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `parse_xml`

Same input shape as `parse_json`, but parses an XML payload into a JSON
structure at `data.{target}`.

## Synopsis

```json
{
  "name": "parse_xml",
  "input": {
    "source": "payload",
    "target": "order"
  }
}
```

## Description

`parse_xml` is an engine built-in from dataflow-rs. Orion declares no input schema for it, so a mistake in `input` surfaces when the task runs rather than when the workflow is saved.

**Retry safety:** `pure`. An engine built-in reads and writes the message and nothing else.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `source` | string | yes | — | Where to read the raw XML from, for example `"payload"` |
| `target` | string | yes | — | Stored at `data.{target}` |

## Examples

```json
{ "name": "parse_xml", "input": { "source": "payload", "target": "order" } }
```

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Author a workflow](../../guides/author/workflows.md): conditions, mapping and validation in practice.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Workflow definition](../workflows.md#the-data-context): the data context every function reads and writes.
