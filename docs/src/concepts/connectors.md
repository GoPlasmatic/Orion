<!-- description: A connector is a named connection to an external system: a file in your definition set that compiles into an immutable connection revision a node serves. -->
<!-- type: concept -->
<!-- last_verified: 2026-10-25 -->

# Connectors

A *connector* is a named connection to an external system, such as a database, an HTTP API or a cache. You write it once as a file in your definition set, and every workflow that needs it names it. Credentials, pooling, retries and circuit breaking belong to the connector, not to the tasks that use it.

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

The diagram shows two workflows sharing `payments-api`. They share one configured connection, one credential and one circuit breaker. Change the connector and both follow.

## The problem it solves

Without a connector, every task that reaches a database would carry its own address, its own credential and its own retry policy. A credential rotation would touch every workflow, and nothing would stop one task from deleting rows another relies on.

A connector moves all of that below the logic. A task names `orders-db` and nothing else. What `orders-db` points at, which credential it presents, and what it permits are decided once, in one file.

## Two forms: the file and the connection

A connector exists in two forms. You write the first; a node serves the second.

| Form | Where it lives | Who writes it | Changes by |
|---|---|---|---|
| Connector file | Your definition set, beside the workflows | You | Editing the file |
| Connection revision | The node's deployment, named by the manifest | `compile` and `deployments apply -d` | A new revision; a revision never changes |

This is a connector file. It is the one [Add your first connector](../get-started/tutorials/first-connector.md) deploys:

```json
{{#include ../../../examples/packages/postgres-orders/connector.json}}
```

`orion-cli compile` turns it into `connections/orders-db.json`. The connector's `name` becomes the connection id. Its `driver` picks a built-in driver. Every secret reference moves to `secret_locators`, and the rest becomes `public_config`:

```json
{
  "connection_id": "orders-db",
  "driver": "org.orion.postgresql",
  "provider_digest": "sha256:5e9d3f17…",
  "public_config": { "allow_private_urls": true, "operations": { "delete": false }, "max_connections": 5 },
  "secret_locators": { "connection_string": "env://ORDERS_DB_URL" }
}
```

`deployments apply -d` stores that as a connection revision. It reuses a stored revision whose settings and locators match, and otherwise creates the next one. The workflow that names `orders-db` requires that revision in the manifest, so the two always deploy together. [Deployments](./deployments.md) explains the manifest.

## The types

| Type | Reaches | Driver | Used by |
|---|---|---|---|
| `http` | Any HTTP API | `org.orion.http` | `http_call` |
| `db` | PostgreSQL, MySQL, SQLite | `org.orion.postgresql`, `org.orion.mysql`, `org.orion.sqlite` | `data_query`, `data_write`, `db_read`, `db_write` |
| `cache` | Per-node process memory | `org.orion.memory-cache` | `cache_read`, `cache_write`, `cache_delete`, `cache_incr` |

A `db` connector names its backend in its `driver` field: `postgresql`, `mysql` or `sqlite`. The connection string can then be a secret reference, because nothing has to be read from it. `compile` refuses a `db` connector without a `driver`.

## Secrets live outside the definition

Any string field can hold a reference instead of a value. `env://ORDERS_DB_URL` reads the node's environment, and `vault://` reads HashiCorp Vault. A node resolves the reference when it loads the connection. Three things follow:

- **No deployment holds a credential.** A connection revision stores the reference, so a database dump is not a credential leak.
- **One definition set serves every environment.** Staging and production differ only in what the variable holds.
- **A literal credential is refused.** `compile` rejects a secret-shaped key with a literal value, or a password inside a URL.

Cloud secret-manager schemes are reserved. A reference using one with no live resolver is refused rather than passed through as a literal.

## Gates: what a connector permits

Every connector declares which operations workflows may perform through it, and every gate defaults to allowed. Turning one off refuses that operation whatever a workflow asks for:

```json
{ "type": "db", "driver": "postgresql", "connection_string": "env://ORDERS_DB_URL",
  "operations": { "delete": false, "raw_write": false } }
```

That connector cannot delete a row, not because no workflow tries but because the connector refuses. Gates are enforced at the connection, one level below the logic, and they survive every workflow change.

## Breakers: what a connector does when the far side fails

A connector can be guarded by a *circuit breaker*. Repeated failures open it. Calls through it then fail fast with `503` instead of piling up against a dead backend. It closes again on its own once calls succeed. HTTP connectors also retry with exponential backoff.

Breakers are off by default. Turn them on with `engine.circuit_breaker.enabled = true` ([configuration](../reference/configuration/engine.md#circuit-breaker)). A breaker is per `channel:connector` pair and per node, so one noisy channel cannot trip a shared connector for every other channel.

## Boundaries

- **A connector is not created on a running node.** It reaches a node only inside a deployment, as a connection revision.
- **A connection revision never changes.** Editing the file and applying again creates the next revision; the earlier one stays for rollback.
- **Rotating a secret changes no revision.** The revision holds the reference, and the value lives in the environment or in Vault.
- **Only the built-in drivers deploy from a definition set.** A connector of any other type is a `compile` error.

## Next steps

- [Add your first connector](../get-started/tutorials/first-connector.md): deploy one against PostgreSQL and use it from a workflow.
- [Connect a database or API](../guides/author/connectors.md): write a connector file for each type, and keep its credential out.
- [Connector types](../reference/connectors/index.md): every field of every type, with defaults, gates and retries.
- [Deployments](./deployments.md): how a connection revision joins the manifest beside the workflows that use it.
