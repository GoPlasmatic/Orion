<!-- description: The mongo_aggregate task function: run a stage-allowlisted aggregation pipeline against a MongoDB connector, with $out and $merge gated per connector. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `mongo_aggregate`

Runs an aggregation pipeline, the surface `find()` cannot reach: `$group`, `$unwind`, `$lookup`, `$facet`, and the rest. Stages are extended JSON, and **stage names are allowlisted**. The read-only stages are always available.

## Synopsis

```json
{
  "name": "mongo_aggregate",
  "input": {
    "connector": "mongo",
    "database": "shop",
    "collection": "recordings",
    "pipeline": [],
    "allow_disk_use": false,
    "output": "temp_data.by_quality"
  }
}
```

## Description

`mongo_aggregate` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `read`. See [Retry safety](./retry-safety.md) for what the answer costs.

The write stages `$out`/`$merge` run only on a connector that sets [`aggregate_write_stages: true`](../connectors/db.md); the default is false, because an aggregation must not silently write. An unknown stage is refused by name, at authoring time for a literal pipeline and again at runtime after `{"var": ..}` substitution. Message data therefore cannot smuggle a stage in.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the MongoDB connector |
| `database` | string | yes | — | Database name |
| `collection` | string | yes | — | Collection name |
| `pipeline` | array | yes | — | Aggregation stages, each `{"$stage": …}` (extended JSON) |
| `allow_disk_use` | bool | no | `false` | Let the server spill large stages to disk |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where result documents are written |

Results are bounded by `query.max_limit` like `mongo_read`; a `$out`/`$merge`
pipeline returns an empty array (Mongo's own contract for those stages).

## Examples

```json
{
  "name": "mongo_aggregate",
  "input": {
    "connector": "mongo",
    "database": "shop",
    "collection": "recordings",
    "pipeline": [
      { "$match": { "meetingId": { "var": "data.meeting_id" } } },
      { "$unwind": "$videos" },
      { "$group": { "_id": "$videos.quality", "count": { "$sum": 1 } } }
    ],
    "output": "temp_data.by_quality"
  }
}
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
