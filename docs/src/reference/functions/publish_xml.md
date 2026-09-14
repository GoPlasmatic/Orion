<!-- description: The publish_xml task function: serialize a field of the data context to an XML string under a named root element, stored at another field. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `publish_xml`

Like `publish_json`, but serializes to an XML string.

## Synopsis

```json
{
  "name": "publish_xml",
  "input": {
    "source": "order",
    "target": "order_xml",
    "root_element": "Order"
  }
}
```

## Description

`publish_xml` is an engine built-in from dataflow-rs. Orion declares no input schema for it, so a mistake in `input` surfaces when the task runs rather than when the workflow is saved.

**Retry safety:** `pure`. An engine built-in reads and writes the message and nothing else.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `source` | string | yes | — | Field under `data` to serialize |
| `target` | string | yes | — | Field under `data` to receive the XML string |
| `root_element` | string | no | `"root"` | Name of the XML root element |

## Examples

```json
{ "name": "publish_xml", "input": { "source": "order", "target": "order_xml", "root_element": "Order" } }
```

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Author a workflow](../../guides/author/workflows.md): conditions, mapping and validation in practice.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Workflow definition](../workflows.md#the-data-context): the data context every function reads and writes.
