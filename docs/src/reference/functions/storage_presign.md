<!-- description: The storage_presign task function: compute a time-limited presigned GET or PUT URL for one object in a storage connector's bucket, with no data path. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `storage_presign`

Computes a time-limited presigned URL for one object in a
[storage connector](../connectors/storage.md)'s bucket. It is **pure local computation**: no bytes move through the runtime, and the client talks to the object store directly. GET presigns downloads; PUT presigns direct client uploads.

## Synopsis

```json
{
  "name": "storage_presign",
  "input": {
    "connector": "media",
    "method": "GET",
    "key": {
      "var": "temp_data.object_key"
    },
    "expires_in": "7d",
    "response_content_type": "…",
    "response_content_disposition": "…",
    "content_type": "…",
    "output": "temp_data.play_url"
  }
}
```

## Description

`storage_presign` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `pure`. See [Retry safety](./retry-safety.md) for what the answer costs.

Each method answers to its own connector gate (`presign_get` / `presign_put`).

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the storage connector |
| `method` | string | no | `"GET"` | `GET` \| `PUT` — an open value set |
| `key` | string | yes | — | Object key within the connector's bucket |
| `expires_in` | number \| string | yes | — | URL lifetime: integer seconds or `"<n>s\|m\|h\|d"`; at most 7 days (S3's own ceiling) |
| `response_content_type` | string | no | — | GET only: forces the answered Content-Type; signed, so the client cannot alter it |
| `response_content_disposition` | string | no | — | GET only: forces Content-Disposition — the download-filename knob; signed |
| `content_type` | string | no | — | PUT only: the Content-Type the uploader must send — a signed header, so any other type is refused by the store |
| `output` | string \| JSONLogic | no | `"data"` | Where the presigned URL (string) is stored |

## Examples

```json
{
  "name": "storage_presign",
  "input": {
    "connector": "media",
    "key": { "var": "temp_data.object_key" },
    "expires_in": "7d",
    "output": "temp_data.play_url"
  }
}
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
