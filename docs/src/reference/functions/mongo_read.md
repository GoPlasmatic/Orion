<!-- description: The mongo_read task function: run a hand-written find() with an extended-JSON filter, projection, sort, limit and skip against a MongoDB connector. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `mongo_read`

The raw escape hatch for MongoDB reads: runs a `find()` with a hand-written
Mongo filter document and writes the matched documents as a JSON array. For
backend-portable queries, prefer [`data_query`](./data_query.md).

## Synopsis

```json
{
  "name": "mongo_read",
  "input": {
    "connector": "mongo",
    "database": "shop",
    "collection": "customers",
    "filter": {
      "tier": "vip",
      "since": {
        "$gte": {
          "$date": "2024-01-01T00:00:00Z"
        }
      }
    },
    "projection": {},
    "sort": {
      "since": -1
    },
    "limit": 50,
    "skip": 0,
    "output": "data.vips"
  }
}
```

## Description

`mongo_read` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `read`. See [Retry safety](./retry-safety.md) for what the answer costs.

Documents are **extended JSON**. BSON types with an extended-JSON spelling become real BSON values in the filter. `{"$oid": "…"}` is an ObjectId, `{"$date": "…"}` a typed date, and the rest of the family follows. They come back in their canonical spellings in the output, so a value read from one document can drive the next task's filter unchanged.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the MongoDB connector |
| `database` | string | yes | — | Database name |
| `collection` | string | yes | — | Collection name |
| `filter` | object | no | `{}` | MongoDB find filter document (extended JSON) |
| `projection` | object | no | all fields | MongoDB projection document, for example `{"name": 1, "_id": 0}` |
| `sort` | object | no | natural order | MongoDB sort document, for example `{"created_at": -1}` |
| `limit` | number | no | unlimited* | Maximum documents to return; must not exceed `query.max_limit` |
| `skip` | number | no | `0` | Documents to skip; must not exceed `query.max_skip` |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where matched documents are written |

\* an unlimited read is still bounded: a result larger than `query.max_limit`
is an error rather than an OOM.

## Examples

```json
{
  "name": "mongo_read",
  "input": {
    "connector": "mongo",
    "database": "shop",
    "collection": "customers",
    "filter": { "tier": "vip", "since": { "$gte": { "$date": "2024-01-01T00:00:00Z" } } },
    "sort": { "since": -1 },
    "limit": 50,
    "output": "data.vips"
  }
}
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Portable data dialect](../data-dialect.md): the backend-neutral alternative for portable queries.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
