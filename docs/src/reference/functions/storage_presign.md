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
    "content_length": 1048576,
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
| `content_length` | number \| JSONLogic | no | — | PUT only: the exact byte count the uploader must send — a signed header, so any other size is refused by the store. At least 1 |
| `output` | string \| JSONLogic | no | `"data"` | Where the presigned URL (string) is stored |

## Bounding an upload

A presigned PUT with neither constraint is an unbounded write. The URL commits
to the bucket, the key and the expiry, and to nothing else. `content_type` and
`content_length` are the two bounds the signature itself can carry. Both are
signed headers, so the store enforces them rather than the runtime. A client
that sends anything else gets `SignatureDoesNotMatch`, not a stored object.

`content_length` is an **exact** byte count, not a ceiling. SigV4 query-string
auth signs a header's value and has no range form. A range needs S3's
POST-policy scheme (`content-length-range`), which Orion does not implement. So
the caller must know the size before it uploads — which it does whenever it
already declares a digest.

What the uploader must do:

- Send exactly that many bytes, with a real `Content-Length` header. A chunked
  or streaming body sends no length and cannot satisfy the signature.
- From a browser, do not set the header. `Content-Length` is forbidden to
  `fetch`, which sets it from the body. A `Blob` or `File` body therefore
  matches when its size matches; a `ReadableStream` body sends no length and is
  refused.

The value must be at least 1. AWS's front end rewrites an incoming
`Content-Length: 0` to an empty value, so a URL signed for zero bytes can never
match its own signature. Orion refuses it at authoring time rather than hand out
a URL that fails with no way to tell why.

There is no upper bound here. A store's own single-PUT ceiling (5 GiB on S3)
answers for that, and it differs between S3-compatible implementations.

[`storage_head`](./storage_head.md) still answers how big the object that landed
is. It is the check to keep for anything the signature cannot bind.

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

An upload bounded to the size and type the caller declared:

```json
{
  "name": "storage_presign",
  "input": {
    "connector": "models",
    "method": "PUT",
    "key": { "var": "temp_data.artifact_key" },
    "content_type": "application/octet-stream",
    "content_length": { "var": "temp_data.declared_bytes" },
    "expires_in": "15m",
    "output": "temp_data.put_url"
  }
}
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
