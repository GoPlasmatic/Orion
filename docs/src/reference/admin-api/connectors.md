<!-- description: The connector endpoints: create, validate, update, reload and the per-type reachability probe, plus listing and resetting circuit breakers. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Connector endpoints

The endpoints that create, test and reload a connector, and the breaker endpoints beside them.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/connectors` | Create connector. String fields may use `env://VAR_NAME` to pull values from the process environment. Optional `tags: ["..."]` (selection labels for `?tag=` and package export) and `enabled` (default `true`; a disabled connector is never loaded into the registry, and export → import preserves the flag) |
| GET | `/api/v1/admin/connectors` | List connectors (secrets masked). Filter with `?tag=` |
| GET | `/api/v1/admin/connectors/{id}` | Get connector by ID (secrets masked). The config comes back both parsed (`config`) and as the stored string (`config_json`) — [which to read](../connectors/identity.md) |
| PUT | `/api/v1/admin/connectors/{id}` | Update connector |
| DELETE | `/api/v1/admin/connectors/{id}` | Delete connector |
| POST | `/api/v1/admin/connectors/import` | Bulk import connectors. `?dry_run=true` validates without writing; `?on_conflict=fail\|skip\|new_version` picks what an existing name means (connectors are unversioned, so `new_version` updates in place) |
| GET | `/api/v1/admin/connectors/export` | Export every matching connector, secrets masked. Filter with `?tag=` |
| POST | `/api/v1/admin/connectors/validate` | Validate a connector definition without saving |
| POST | `/api/v1/admin/connectors/{id}/test` | Probe the connector's backend and report whether it is reachable |
| GET | `/api/v1/admin/connectors/circuit-breakers` | List circuit breaker states |
| POST | `/api/v1/admin/connectors/circuit-breakers/{key}` | Reset a circuit breaker |

Connector types: `http`, `kafka`, `db` (PostgreSQL/MySQL/SQLite/MongoDB), `cache`, `es` (Elasticsearch). Every connector config accepts an optional `operations` block that enables or disables operation types per connector. The gates are `read` / `insert` / `update` / `delete` / `upsert` / `raw_write` on `db` and `es`. On `cache` they are `read` / `write`, on `kafka` `publish`, and on `http` a `methods` allow-list. Per-type fields and gates are specified in [Connector Types](../connectors/operation-gates.md).

## Testing a connector

`POST /api/v1/admin/connectors/{id}/test` probes the saved connector's backend, so wrong credentials surface when they are saved rather than at the first real request. It reads the **stored row** with its `env://` references resolved, not the registry. A connector that failed to load has no registry entry, and that is exactly when this endpoint is useful.

```json
{ "data": { "reachable": true, "supported": true, "connector_type": "db", "probe": "SELECT 1" } }
```

A backend that cannot be reached is still a `200`: the probe ran, and `reachable: false` with an `error` string is its answer. The failure is the backend's, not Orion's. Three kinds have no probe: `es`, `kafka`, and a `db` connector pointing at MongoDB through a `mongodb://` URL. For those, `supported: false` distinguishes the permanent capability gap from an outage. Key monitoring on `supported && !reachable`, not on `reachable` alone.

| Type | Probe | Touches the backend? |
|---|---|---|
| `db` (SQL) | `SELECT 1` through the shared pool | Yes, read-only |
| `db` (MongoDB) | not implemented (`supported: false`) | No |
| `cache` | reads one probe key | Yes, read-only — nothing is written |
| `http` | `GET` the configured URL with the connector's auth, 5 s timeout | **Yes — one real request** |
| `es`, `kafka` | not implemented (`supported: false`) | No |

The HTTP probe issues a genuine request with genuine credentials, which is the point: a wrong bearer token is invisible until traffic hits it. A `401`/`403` is reported as **not** reachable. The host answered, but the connector's credentials are wrong, and that is the failure the endpoint exists to surface. It goes through the same client and SSRF policy as a real `http_call`, so a probe cannot pass where traffic would fail. Every call is written to the audit log.

Kafka brokers are covered by `orion-server test-connectivity`.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Connector types](../connectors/index.md): every field of every type.
- [Secret masking](../connectors/masking.md): what these endpoints give back readable.
- [Retries and circuit breakers](../connectors/reliability.md): the breakers these endpoints list and reset.
