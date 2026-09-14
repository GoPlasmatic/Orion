<!-- description: Every Orion connector type and its config — http, kafka, db, cache, es, smtp and storage — with secret references, auth schemes and per-operation gates. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Connector types

A named connection to an external system, and what every type of one carries.

Connector validation errors return `400 VALIDATION_ERROR` with field paths.
Workflow execution failures that cannot be exposed return `500 ENGINE_ERROR`,
while an open circuit returns `503 CIRCUIT_OPEN`. Correct the connector definition,
test it through `POST /connectors/{id}/test`, and use [Errors & Response
Envelopes](../errors.md) for the complete contract.

A **connector** is a named connection to an external system — an API, a database, a cache, a Kafka cluster, or a search cluster. Workflows reference connectors by name. Credentials stay in the connector, never in workflow JSON.

There are exactly seven connector types:

| Type | Backs | Task functions |
|------|-------|----------------|
| [`http`](./http.md) | REST APIs and webhooks | `http_call` |
| [`kafka`](./kafka.md) | Kafka topics (produce only) | `publish_kafka` |
| [`db`](./db.md) | PostgreSQL, MySQL, SQLite, MongoDB | `data_query`, `data_write`, `db_read`, `db_write`, `mongo_read`, `mongo_write`, `mongo_aggregate` |
| [`cache`](./cache.md) | Redis or in-process memory | `cache_read`, `cache_write` |
| [`es`](./es.md) | Elasticsearch | `data_query`, `data_write` |
| [`smtp`](./smtp.md) | Transactional email over SMTP | `send_email` |
| [`storage`](./storage.md) | S3-compatible object storage (presign + metadata only) | `storage_presign`, `storage_head` |

Type values match case-insensitively. Any other value is refused with the valid list. A stored connector whose config no longer parses fails to load and surfaces as a connector load issue.

The task functions and their inputs are specified in the [Function Reference](../functions/index.md).

| Page | Holds |
|---|---|
| [Shared connector blocks](./common.md) | identity, secret references, masking, authentication, request layering, gates and retries. |
| [The seven types](./types.md) | the seven types, what each backs, and the `config` each takes. |

## Related

- [Admin API — Connectors](../admin-api/connectors.md): the endpoints, and the reachability probe.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Portable data dialect](../data-dialect.md): the backend-neutral language `db` and `es` serve.
- [Environment variables](../environment-variables.md): `${VAR}` versus `env://`, and what an unset variable does.
