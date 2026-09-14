<!-- description: The `es` connector config: the cluster URL, authentication, the portable dialect it serves, and its per-operation gates and dialect guards. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `es` connectors

The `config` fields of a `es` connector, which backs Elasticsearch.

An Elasticsearch cluster driven by the portable dialect: [`data_query`](../functions/data_query.md) renders a Query DSL `_search` body; `data_write` renders `_bulk`, `_update_by_query`, `_delete_by_query`, and `_update` calls. Requests go over the shared HTTP client — there is no dedicated ES driver.

```json
{
  "name": "search-cluster",
  "connector_type": "es",
  "config": {
    "type": "es",
    "url": "http://localhost:9200",
    "auth": { "type": "apikey", "header": "Authorization", "key": "env://ES_API_KEY" }
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `url` | string | yes | — | Base URL of the cluster, for example `http://localhost:9200` |
| `auth` | object | no | — | [Authentication](./authentication.md): `bearer`, `basic`, or `apikey` |
| `request_timeout_ms` | integer | no | — | Per-request timeout |
| `allow_private_urls` | boolean | no | `false` | Allow private and internal IP addresses (SSRF protection) |
| `max_response_size` | integer | no | `10485760` | Maximum response body size in bytes (10 MB) |
| `operations` | object | no | all allowed | Same gate set as `db` — see [Operation gates](./operation-gates.md) |
| `dialect` | object | no | both guards off | [Dialect guards](./db.md#dialect-guards) |

There is no `retry` field. The dialect drives `_bulk` and the by-query mutations through this connector as well as `_search`, and none are safe to re-send blind. See [Retries](./reliability.md#retries). ES-specific dialect semantics (the `_id` rename, forced refresh, capability limits) live in the [Portable Data Dialect](../data-dialect.md#elasticsearch-notes) reference.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Portable data dialect](../data-dialect.md): the language `data_query` and `data_write` speak.
- [Operation gates](./operation-gates.md): the `operations` block this type carries.
- [Definition and identity](./identity.md): the row the `config` sits in.
