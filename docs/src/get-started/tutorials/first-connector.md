<!-- description: Connect Orion to PostgreSQL and build a service that writes an order and reads back the customer's history, using the portable, injection-safe data dialect. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-09-14 -->

# Add your first connector

[Your first service](./first-service.md) transformed data in process; real services talk to databases. This tutorial connects Orion to PostgreSQL and builds a service that writes an order and reads the customer's history back.

## What you will learn

- What a connector is, and why credentials never enter the definition.
- How the portable data dialect keeps request data out of SQL text.
- What an operation gate does, and where it is enforced.
- How a workflow inserts a row and reads related rows back in one request.

## Before you start

Tested with Orion 1.8.1. You need:

- an Orion server on `http://localhost:8080`, and [Build your first service](./first-service.md) behind you
- Git, and Docker with Compose
- `curl` and a POSIX shell (on Windows, WSL)

A *connector* is a named, reusable connection to an external system. You configure it once through the admin API and reference it by name from any workflow. The runtime holds the credentials, the pool, the retries and the circuit breaker.

Every file below ships in the repository, so clone it first:

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion/examples/packages/postgres-orders
```

> [!TIP]
> `docker compose up -d && cd ../.. && ./deploy.sh postgres-orders` builds the whole thing in two commands. The steps below do it one piece at a time, so you can see what each piece is for.

## 1. Start a database

The directory ships a compose file and seed data, with two `customers` and three `orders`:

```bash
docker compose up -d postgres
```

Then start Orion, telling it where the database is:

```bash
ORDERS_DB_URL=postgres://orion:orion@localhost:5432/orion_orders orion-server
```

## 2. Create the connector

This is the connector definition:

```json
{{#include ../../../../examples/packages/postgres-orders/connector.json}}
```

Post it. Connectors are live on creation; there is no activation step, and the registry reloads on every connector change:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/connectors \
  -H 'Content-Type: application/json' --data @connector.json
```

Three details are worth copying into every real deployment:

- **The connection string is an environment reference.** `${ORDERS_DB_URL:-…}` is substituted from the server's environment when the connector loads. The saved config carries no credentials, and the same JSON works in every environment.
- **`"operations": { "delete": false }` makes the connector delete-proof.** Operation gates are enforced at the connector, whatever a workflow asks for.
- **`allow_private_urls` is required for a private address.** Orion blocks connections to private ranges by default. A database on `localhost` or a container network is the normal case for saying so explicitly.

## 3. Create the workflow

Three tasks: parse the request, insert the order, read back the customer with their order history:

```json
{{#include ../../../../examples/packages/postgres-orders/workflow.json}}
```

Create and activate it:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H 'Content-Type: application/json' --data @workflow.json

curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/record-order/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

How the pieces fit:

- **`{ "param": "total" }` marks a value slot.** The `params` map is the only place request data enters a query, and every resolved value is a bound parameter, never interpolated text. The dialect is injection-safe by construction.
- **The inline `schema` declares `customers has_many orders`.** That relation is what powers `"include": { "orders": … }`. The schema also permits the query: the dialect rejects undeclared entities and columns, so a task without one reaches nothing.
- **An `include` states its own `sort`.** The per-customer page is cut inside the database, so "the latest 10 orders" needs an order key.
- **`"returning": ["id"]`** captures the generated key from the insert.

## 4. Expose it as a service

The channel binds `POST /record-order` to the workflow:

```json
{{#include ../../../../examples/packages/postgres-orders/channel.json}}
```

Create and activate it:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/channels \
  -H 'Content-Type: application/json' --data @channel.json

curl -s -X PATCH http://localhost:8080/api/v1/admin/channels/record-order/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

Both calls answer a successful admin envelope, and `orion-cli channels list` shows `record-order` as active.

## 5. Call it

Send the sample request that ships beside the definitions:

```bash
curl -s -X POST http://localhost:8080/api/v1/data/record-order \
  -H 'Content-Type: application/json' --data @request.json
```

Output:

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

One request did a parameterized insert and then a relation-hydrated read.

## Switching backends

Nothing in that workflow is PostgreSQL-specific. Point `orders-db` at MySQL or SQLite and it renders different SQL. Point it at MongoDB or Elasticsearch and the same envelope renders a `find` filter or a Query DSL search. [Portable data dialect](../../reference/data-dialect.md) has the vocabulary and the per-backend notes.

## Clean up or run it again

The deployment script skips definitions that already exist, but every request inserts another order. Stop the local database and remove its data volume from the package directory:

```bash
docker compose down -v
```

If Orion was running outside that Compose project, delete `record-order` as a channel and as a workflow, then delete the `orders-db` connector, with `orion-cli … delete --yes`. Deletion is permanent; keep the definitions if you are continuing to the next tutorial.

## Recap

- A connector is configured once and referenced by name. Four things came with it, and none of them are in the workflow: a connection pool capped at `max_connections`, a circuit breaker, a delete gate enforced below the logic, and credentials that never entered the database.
- Request data enters a query only through `params`, as bound values.
- The inline schema both permits the query and declares the relations an `include` can hydrate.
- The same envelope renders SQL, a MongoDB filter or an Elasticsearch query, depending only on the connector it names.

## Next steps

- [Test and promote a service](./test-and-promote.md): dry-run this workflow with the database stubbed out, then ship it to another instance.
- [Connectors](../../concepts/connectors.md): the idea, and the other six types.
- [Connector types](../../reference/connectors/index.md): every field of every type, with gates, retries and secret handling.
- [Portable data dialect](../../reference/data-dialect.md): operators, the schema registry, relations, write envelopes and safety guards.
