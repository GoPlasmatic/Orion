<!-- description: The mongo_write task function: insert, update, replace or delete MongoDB documents with extended-JSON operators, array_filters and upsert. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `mongo_write`

The write twin of [`mongo_read`](./mongo_read.md): inserts, updates, replaces, or deletes documents with hand-written Mongo documents. Nested arrays and objects are included, since every document field is extended JSON. For
backend-portable mutations, prefer [`data_write`](./data_write.md).

## Synopsis

```json
{
  "name": "mongo_write",
  "input": {
    "connector": "mongo",
    "database": "shop",
    "collection": "meetings",
    "op": "update_one",
    "document": {},
    "documents": [],
    "filter": {
      "_id": {
        "$oid": {
          "var": "data.payload.object.id"
        }
      }
    },
    "update": {},
    "array_filters": [],
    "upsert": true,
    "ordered": true,
    "all": false,
    "output": "temp_data.write_result"
  }
}
```

## Description

`mongo_write` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `depends_on` `op`. See [Retry safety](./retry-safety.md) for what the answer costs.

`op` is an open value set. Each op reads a specific subset of the fields below, and naming a field the op ignores is an authoring-time error.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the MongoDB connector |
| `database` | string | yes | — | Database name |
| `collection` | string | yes | — | Collection name |
| `op` | string | yes | — | `insert_one`, `insert_many`, `update_one`, `update_many`, `replace_one`, `delete_one`, or `delete_many` |
| `document` | object | conditional | — | The document for `insert_one` / `replace_one` (a replacement must be a plain document, no `$` operators) |
| `documents` | array | conditional | — | Documents for `insert_many`; the batch is capped by `write.max_rows` |
| `filter` | object | conditional | — | Selection filter for update/replace/delete ops (extended JSON) |
| `update` | object | conditional | — | Update document for `update_one`/`update_many`; top-level keys must be atomic operators (`$set`, `$inc`, `$push`, …). Field paths may target array elements — see [Updating array elements](#updating-array-elements) |
| `array_filters` | array | no | — | `update_one`/`update_many` only: filter documents naming the `$[identifier]` paths used in `update` |
| `upsert` | bool | no | `false` | Insert when nothing matches (update/replace ops). Gated as `upsert` on the connector when true, `update` otherwise |
| `ordered` | bool | no | `true` | `insert_many` only: stop at the first failure (`true`) or attempt every document (`false`) |
| `all` | bool | no | `false` | Acknowledge an intentionally unfiltered update/replace/delete — also requires `write.allow_unfiltered` in config |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the write result is written |

## Examples

The result mirrors `data_write`'s Mongo envelopes. Inserts report `{ "status", "inserted", "ids" }`; a partially applied `insert_many` reports per-item outcomes and audits as **207**, exactly like `data_write`. Updates and replaces report `{ "status", "matched", "modified", "upserted_id"? }`. Deletes report `{ "status", "deleted" }`.

```json
{
  "name": "mongo_write",
  "input": {
    "connector": "mongo",
    "database": "shop",
    "collection": "meetings",
    "op": "update_one",
    "filter": { "_id": { "$oid": { "var": "data.payload.object.id" } } },
    "update": { "$set": {
      "payload": { "var": "data.payload" },
      "updated_at": { "$date": { "var": "metadata.timestamp" } },
      "deleted": false
    } },
    "upsert": true,
    "output": "temp_data.write_result"
  }
}
```

## Caveats

### Updating array elements

Three path forms reach elements inside an array field, and **the simplest one that fits is the right one**:

| Path | Updates | Needs `array_filters` |
|---|---|---|
| `sessions.$.active` | the **first** element the `filter` matched | no |
| `sessions.$[].active` | **every** element, unconditionally | no |
| `sessions.$[s].active` | every element matching an `array_filters` entry | yes |

For "flip the one embedded entry whose `deviceId` matches", `$` is enough — atomically, in one round trip, with no `array_filters`:

```json
{ "op": "update_one",
  "filter": { "_id": {"var": "temp_data.user_id"},
              "sessions.deviceId": {"var": "data.deviceId"} },
  "update": { "$set": { "sessions.$.active": false } } }
```

`array_filters` is for what `$` and `$[]` cannot express. That is updating **every** element matching a predicate, reaching **nested** arrays (`$[a].items.$[b]`), and using several independent identifiers in one update.

```json
{ "op": "update_many",
  "filter": { "_id": {"var": "temp_data.user_id"} },
  "update": { "$set": { "sessions.$[s].active": false } },
  "array_filters": [ { "s.expiresAt": { "$lt": { "$date": {"var": "temp_data.now"} } } } ] }
```

Each entry constrains exactly one identifier (`$and`/`$or`/`$nor` take theirs from their branches). Orion cross-checks the two before the driver call. An identifier with no filter, a filter nothing uses, or `array_filters` with no `$[identifier]` anywhere is a `400` naming the problem. MongoDB refuses all three, but its message would reach you as an opaque `500`.

`upsert: true` is permitted. On the *insert* branch there is no array to match. A filter matching no element is **not** an error — the update succeeds with `matched: 1, modified: 0`, which the result envelope reports faithfully.

> [!NOTE]
> Whole-array `$set` — read the array, modify it in memory, write it back — is racy: two concurrent writers each write the full array and the second silently clobbers the first. Orion has no transaction surface to fix that, so prefer a positional path, which the server applies atomically.

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Portable data dialect](../data-dialect.md): the backend-neutral alternative for portable mutations.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
