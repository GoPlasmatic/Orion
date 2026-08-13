# Connectors

A **connector** is a named connection to an external system. You configure it
once, then reference it by name from any workflow. Credentials, pooling,
retries, and circuit breaking belong to the connector, not to the tasks that use
it.

```orion-diagram
{
  "direction": "LR",
  "groups": [ { "id": "orion", "label": "Orion" } ],
  "nodes": [
    { "id": "wf1", "label": "order-processing", "type": "service", "group": "orion" },
    { "id": "wf2", "label": "refund-handler", "type": "service", "group": "orion" },
    { "id": "c1", "label": "orders-db", "sublabel": "db", "type": "accent", "group": "orion" },
    { "id": "c2", "label": "payments-api", "sublabel": "http", "type": "accent", "group": "orion" },
    { "id": "pg", "label": "PostgreSQL", "type": "datastore" },
    { "id": "ext", "label": "Stripe", "type": "datastore", "shape": "cloud" }
  ],
  "edges": [
    { "from": "wf1", "to": "c1" }, { "from": "wf1", "to": "c2" }, { "from": "wf2", "to": "c2" },
    { "from": "c1", "to": "pg" }, { "from": "c2", "to": "ext" }
  ]
}
```

Two workflows pointing at `payments-api` share one configured connection, one
credential, and one circuit breaker. Change the endpoint and both follow.

## The types

| Type | Reaches | Used by |
|---|---|---|
| `http` | Any HTTP API | `http_call` |
| `db` | PostgreSQL, MySQL, SQLite, MongoDB | `data_query`, `data_write`, `db_read`, `db_write`, `mongo_read` |
| `cache` | Redis, or process memory | `cache_read`, `cache_write` |
| `es` | Elasticsearch | `data_query`, `data_write` |
| `kafka` | A Kafka cluster (producing) | `publish_kafka` |

One `db` type covers both SQL and MongoDB: the connection-string scheme selects
the backend. Connectors are **unversioned** — unlike channels and workflows, an
update replaces the stored config and the registry reloads immediately. There is
no draft step and no activation.

## Secrets live outside the config

Any string field can hold a reference instead of a value:

```json
{ "name": "orders-db", "config": { "type": "db", "connection_string": "env://ORDERS_DB_URL" } }
```

Orion resolves `env://` from the *server's* environment each time the connector
loads. Three things follow, and they are the reason to always author connectors
this way:

- **The database never stores the credential**, so a database dump is not a
  credential leak.
- **The same JSON works in every environment.** Dev, QA, and production differ
  only in what the variable holds — which is what makes a connector
  [promotable](./packages.md).
- **A connector holding a literal secret exports as `"******"` and is refused on
  import.** Masked values cannot round-trip, by design.

`vault://` reads from HashiCorp Vault. Cloud secret-manager schemes are
reserved: a reference using one without a live resolver is refused rather than
passed through as a literal.

## Gates: what a connector permits

Every connector declares which operations workflows may perform through it, and
every gate defaults to allowed. Turning one off makes it a validation error
regardless of what a workflow asks for:

```json
{ "type": "db", "connection_string": "env://ORDERS_DB_URL",
  "operations": { "delete": false, "raw_write": false } }
```

That connector cannot delete a row — not because no workflow tries, but because
the connector refuses. Gates are the cheapest blast-radius control Orion has:
they are enforced at the connection, one level below the logic, and they survive
every workflow change.

## Breakers: what a connector does when the far side fails

Connectors can be guarded by a **circuit breaker**. Repeated failures open it,
calls through it then fail fast with `503` instead of piling up against a dead
backend, and it closes again on its own once calls succeed. HTTP connectors also
retry with exponential backoff.

Breakers are off by default — turn them on with
`engine.circuit_breaker.enabled = true`
([configuration](../reference/configuration.md#circuit-breaker)). A breaker is
per `channel:connector` pair and per node, so one failing vendor API cannot
exhaust the request capacity a healthy one needs, and one noisy channel cannot
trip a shared connector for every other channel using it.

## Next steps

- [Your First Connector](../getting-started/first-connector.md) — configure one
  against PostgreSQL and use it from a workflow.
- [Connector Types](../reference/connectors.md) — every field of every type,
  with defaults, gates, retries, and masking rules.
- [Portable Data Dialect](../reference/data-dialect.md) — the backend-neutral
  query and write envelope, so switching databases is a connector change.
- [Task Functions](../reference/functions.md) — the functions that call
  connectors, and their inputs.
