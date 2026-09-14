<!-- description: The storage_head task function: one signed HEAD request for object metadata, answering exists, size, etag, last_modified and content_type. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `storage_head`

One SigV4-signed HEAD for object metadata. A missing object is **data, not failure**: 404 answers `{ "exists": false }`, because "is it there yet?" is the question this function exists to ask. Auth failures, timeouts, and other statuses fail the task.

## Synopsis

```json
{
  "name": "storage_head",
  "input": {
    "connector": "media",
    "key": {
      "var": "temp_data.object_key"
    },
    "output": "temp_data.object_meta"
  }
}
```

## Description

`storage_head` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `read`. See [Retry safety](./retry-safety.md) for what the answer costs.

One attempt runs inside the circuit breaker; a workflow can loop if it wants polling.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the storage connector |
| `key` | string | yes | — | Object key within the connector's bucket |
| `output` | string \| JSONLogic | no | `"data"` | Where `{ exists, size, etag, last_modified, content_type }` is stored |

## Examples

```json
{
  "name": "storage_head",
  "input": {
    "connector": "media",
    "key": { "var": "temp_data.object_key" },
    "output": "temp_data.object_meta"
  }
}
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
