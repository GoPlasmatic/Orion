<!-- description: Connect Orion to PostgreSQL and build a service that writes an order and reads back the customer's history, using the portable, injection-safe data dialect. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-10-25 -->

# Add your first connector

[Your first service](./first-service.md) transformed data in process; real services talk to databases. This tutorial connects Orion to PostgreSQL and builds a service that writes an order and reads the customer's history back.

## What you will learn

- What a connector is, and why credentials never enter the definition.
- How the portable data dialect keeps request data out of SQL text.
- What an operation gate does, and where it is enforced.
- How a workflow inserts a row and reads related rows back in one request.

## Before you start

Tested with Orion 1.12.0. You need:

- an Orion server on `http://localhost:8080`, and [Build your first service](./first-service.md) behind you
- `orion-server` and `orion-cli` on your `PATH`
- Git, and Docker with Compose
- `curl` and a POSIX shell (on Windows, WSL)

A *connector* is a named, reusable connection to an external system. You declare it once, as a file beside the workflows, and reference it by name from any workflow. `compile` turns the file into a connection revision, and the node holds the pool, the retries and the circuit breaker.

Every file below ships in the repository, so clone it first:

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion/examples/packages/postgres-orders
```

## 1. Start a database

The directory ships a compose file and seed data, with two `customers` and three `orders`:

```bash
docker compose up -d postgres
```

Then start Orion, telling it where the database is:

```bash
ORDERS_DB_URL=postgres://orion:orion@localhost:5432/orion_orders orion-server
```

## 2. Read the connector

This is the connector file:

```json
{{#include ../../../../examples/packages/postgres-orders/connector.json}}
```

`compile` turns it into a connection revision. Three details are worth copying into every real deployment:

- **The connection string is an environment reference.** `env://ORDERS_DB_URL` is resolved from the server's environment when the connection is used. The deployment carries the reference, never the credential, and the same JSON works in every environment.
- **`"operations": { "delete": false }` makes the connector delete-proof.** Operation gates are enforced at the connector, whatever a workflow asks for.
- **`allow_private_urls` is required for a private address.** Orion blocks connections to private ranges by default. A database on `localhost` or a container network is the normal case for saying so explicitly.

## 3. Read the workflow

Three tasks: parse the request, insert the order, read back the customer with their order history:

```json
{{#include ../../../../examples/packages/postgres-orders/workflow.json}}
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

Compile the directory and deploy it:

```bash
orion-server compile . -o /tmp/postgres-orders
orion-cli deployments apply -d /tmp/postgres-orders
```

`compile` reports 1 connector, 1 workflow and 1 channel, and writes `connections/orders-db.json` with the connection string under `secret_locators`. `apply` plans three members, `+ connection orders-db@1` among them, then creates the connection revision, uploads the definitions and commits the deployment. The workflow `requires` the `orders-db` connection, so the deployment cannot leave it behind.

## 5. Call it

Send the sample request that ships beside the definitions:

```bash
curl -s -X POST http://localhost:8080/api/v2/data/record-order \
  -H 'Content-Type: application/json' --data @request.json
```

Output, cut to the two fields the workflow wrote:

```json
{
  "status": "ok",
  "data": {
    "created": { "rows_affected": 1, "returning": [{ "id": 4 }], "status": "ok" },
    "customer": [{
      "id": 1, "name": "Ada Lovelace", "email": "ada@example.com",
      "orders": [
        { "id": 4, "item": "Difference Engine Blueprint", "total": 4200.0 },
        { "id": 2, "item": "Punch Card Set", "total": 35.5 },
        { "id": 1, "item": "Analytical Engine Manual", "total": 120.0 }
      ]
    }]
  },
  "errors": []
}
```

One request did a parameterized insert and then a relation-hydrated read.

## Switching backends

Nothing in that workflow is PostgreSQL-specific. Point `orders-db` at MySQL or SQLite and the same envelope renders that backend's SQL. [Portable data dialect](../../reference/data-dialect.md) has the vocabulary and the per-backend notes.

## Clean up or run it again

Applying the same directory again changes nothing, but every request inserts another order. Stop the local database and remove its data volume from the package directory:

```bash
docker compose down -v
```

If Orion was running outside that Compose project, [roll back](../../reference/admin-api/deployments.md#rollback) to the revision before this one to stop serving the service.

## Recap

- A connector is a file in the set, deployed as a connection revision and referenced by name. Four things came with it, and none of them are in the workflow: a pool capped at `max_connections`, a circuit breaker, a delete gate below the logic, and a credential that never entered the deployment.
- Request data enters a query only through `params`, as bound values.
- The inline schema both permits the query and declares the relations an `include` can hydrate.
- The same envelope renders PostgreSQL, MySQL or SQLite SQL, depending only on the connector it names.

## Next steps

- [Test and deploy a service](./test-and-promote.md): dry-run a workflow with its calls stubbed out, then deploy it.
- [Connectors](../../concepts/connectors.md): the connector file, the connection revision it becomes, and the other two types.
- [Connector types](../../reference/connectors/index.md): every field of every type, with gates, retries and secret handling.
- [Portable data dialect](../../reference/data-dialect.md): operators, the schema registry, relations, write envelopes and safety guards.
