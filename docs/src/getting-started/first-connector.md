<!-- description: Connect Orion to PostgreSQL and build a service that writes an order and reads back the customer's history, using the portable, injection-safe data dialect. -->
# Your First Connector

**Tested with:** Orion 1.5.1 · **Last reviewed:** 2026-09-04

[Your first service](./first-service.md) transformed data in process. Real
services talk to databases. This tutorial connects Orion to PostgreSQL and
builds a service that **writes an order and reads back the customer's history**.

A **connector** is a named, reusable connection to an external system:
configured once through the admin API, referenced by name from any workflow,
with credentials, pooling, retries, and circuit breaking handled by the runtime.

In this guide, you will:

- start a PostgreSQL database with seed data,
- create a connector that reaches it without storing a password,
- write a workflow that inserts a row and reads related rows back,
- call the result as a REST endpoint.

## Before you start

You need a running Orion server on `http://localhost:8080`, Git, Docker with
Compose, and `curl`. Complete [Understand the HTTP Flow](./first-service.md) first if
the workflow and channel lifecycle is new to you. The commands use a
POSIX-compatible shell; on Windows, run them from WSL or adapt them for
PowerShell.

Every file below ships in the repository, so clone it first:

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion/examples/packages/postgres-orders
```

> [!TIP]
> **Fast path.** `docker compose up -d && cd ../.. && ./deploy.sh postgres-orders`
> builds the whole thing in two commands. The steps below do it one piece at a
> time, so you can see what each piece is for.

## 1. Start a database

The directory ships a compose file and seed data — two `customers`, three
`orders`:

```bash
docker compose up -d postgres
```

Then start Orion, telling it where the database is:

```bash
ORDERS_DB_URL=postgres://orion:orion@localhost:5432/orion_orders orion-server
```

## 2. Create the connector

```json
{{#include ../../../examples/packages/postgres-orders/connector.json}}
```

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/connectors \
  -H 'Content-Type: application/json' --data @connector.json
```

Three details are worth copying into every real deployment:

- **The connection string is an environment reference.** `${ORDERS_DB_URL:-…}`
  is substituted from the *server's* environment when the connector loads. The
  saved config carries no credentials, and the same JSON works in every
  environment.
- **`"operations": { "delete": false }` makes the connector delete-proof.**
  Operation gates are enforced at the connector, whatever a workflow asks for.
- **`allow_private_urls` is required for a private address.** Orion blocks
  connections to private ranges by default; a database on `localhost` or a
  container network is the normal case for saying so explicitly.

The connector is live immediately — no activation step. The registry reloads on
every connector change.

## 3. Create the workflow

Three tasks: parse the request, insert the order, read back the customer with
their order history.

```json
{{#include ../../../examples/packages/postgres-orders/workflow.json}}
```

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H 'Content-Type: application/json' --data @workflow.json

curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/record-order/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

How the pieces fit:

- **`{ "param": "total" }` marks a value slot.** The `params` map is the only
  place request data enters a query, and every resolved value is a bound
  parameter — never string-interpolated. The dialect is injection-safe by
  construction.
- **The inline `schema` declares `customers has_many orders`.** That relation is
  what powers `"include": { "orders": … }`. The schema also *permits* the query:
  the dialect rejects undeclared entities and columns, so a task without one
  reaches nothing.
- **An `include` states its own `sort`.** The per-customer page is cut inside the
  database, so "the latest 10 orders" needs an order key. Without one, the ten
  you get are not a defined answer.
- **`"returning": ["id"]`** captures the generated key from the insert.

## 4. Expose it as a service

```json
{{#include ../../../examples/packages/postgres-orders/channel.json}}
```

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/channels \
  -H 'Content-Type: application/json' --data @channel.json

curl -s -X PATCH http://localhost:8080/api/v1/admin/channels/record-order/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

Expected result: both calls return successful admin envelopes and the channel
appears as `active` in `orion-cli channels list`.

## 5. Call it

```bash
curl -s -X POST http://localhost:8080/api/v1/data/record-order \
  -H 'Content-Type: application/json' --data @request.json
```

```json
{
  "status": "ok",
  "data": {
    "created": { "status": "ok", "rows_affected": 1, "returning": [{ "id": 4 }] },
    "customer": [{
      "id": 1, "name": "Ada Lovelace", "email": "ada@example.com",
      "orders": [
        { "id": 1, "item": "Analytical Engine Manual", "total": 120.0 },
        { "id": 2, "item": "Punch Card Set", "total": 35.5 },
        { "id": 4, "item": "Difference Engine Blueprint", "total": 4200.0 }
      ]
    }]
  }
}
```

## What you just got

One request did a parameterized insert and then a relation-hydrated read. Four
things came with the connector, and none of them are in the workflow:

- **A connection pool**, capped at `max_connections`, shared by every workflow
  that names `orders-db`.
- **A circuit breaker**, so a database outage fails fast instead of piling up
  requests against a dead socket.
- **A delete gate**, enforced below the logic — no workflow change can turn this
  connector into one that deletes rows.
- **Credentials that never entered the database**, because the config holds a
  reference rather than a password.

## Switching backends

Nothing in that workflow is Postgres-specific. Point `orders-db` at MySQL or
SQLite and it renders different SQL; point it at MongoDB or Elasticsearch and
the same envelope renders a `find` filter or a Query DSL search. See the
[Portable Data Dialect](../reference/data-dialect.md) for the vocabulary and the
per-backend notes.

## Clean up or run it again

The deployment script skips definitions that already exist, but every request
inserts another order. Stop the local database and remove its tutorial data
volume from the package directory:

```bash
docker compose down -v
```

If Orion was running outside that Compose project, delete `record-order` as a
channel and workflow, then delete the `orders-db` connector through the CLI.
Deletion is permanent; keep the definitions if you are continuing to the
testing guide.

## Next steps

- [Test & Promote a Service](./test-and-promote.md) — dry-run this workflow with
  the database stubbed out, then ship it to another instance.
- [Connectors](../concepts/connectors.md) — the idea, and the other types.
- [Connector Types](../reference/connectors.md) — every field of every type,
  with gates, retries, and secret handling.
- [Portable Data Dialect](../reference/data-dialect.md) — operators, schema
  registry, relations, write envelopes, safety guards.
