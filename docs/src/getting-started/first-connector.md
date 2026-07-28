# Your First Connector

The [first service](../tutorials/cli-setup.md#create-your-first-service) you
built transformed data in-process. Real services talk to databases. This
tutorial connects Orion to PostgreSQL and builds a service that **writes an
order and reads back the customer's history** — using the portable data
dialect (`data_write` / `data_query`), so the same workflow JSON would run
unchanged against MongoDB or Elasticsearch.

A **connector** is a named, reusable connection to an external system:
configured once through the admin API, referenced by name from any workflow,
with credentials, pooling, retries, and circuit breaking handled by the
runtime.

> **Fast path:** this tutorial is shipped as a runnable example. From a repo
> clone: `cd examples/postgres-orders && docker compose up -d && cd .. &&
> ./deploy.sh postgres-orders`. The steps below build the same thing by hand.

## 1. Start a database

From the repo's [`examples/postgres-orders/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/postgres-orders)
directory (which ships a compose file and seed data — two `customers`, three
`orders`):

```bash
docker compose up -d postgres
```

Then start Orion, telling it where the database is:

```bash
ORDERS_DB_URL=postgres://orion:orion@localhost:5432/orion_orders orion-server
```

## 2. Create the connector

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/connectors \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "orders-db",
    "name": "orders-db",
    "connector_type": "db",
    "config": {
      "type": "db",
      "connection_string": "${ORDERS_DB_URL:-postgres://orion:orion@postgres:5432/orion_orders}",
      "max_connections": 5,
      "operations": { "delete": false }
    }
  }'
```

Two details here are worth copying into every real deployment:

- **The connection string is an environment reference.** `${ORDERS_DB_URL:-…}`
  is substituted from the *server's* environment when the connector loads —
  the saved config carries no credentials, and the same JSON works in every
  environment.
- **`"operations": { "delete": false }` makes the connector delete-proof.**
  Operation gates (`read` / `insert` / `update` / `delete` / `upsert` /
  `raw_write`) are enforced at the connector, regardless of what any workflow
  asks for.

The connector is live immediately — no activation step, and the registry
hot-reloads on every connector change.

## 3. Create the workflow

Three tasks: parse the request, insert the order, read back the customer with
their order history.

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H 'Content-Type: application/json' \
  -d '{
    "workflow_id": "record-order",
    "name": "Record Order",
    "condition": true,
    "tasks": [
      { "id": "parse", "name": "Parse payload",
        "function": { "name": "parse_json", "input": { "source": "payload", "target": "req" } } },
      { "id": "record", "name": "Insert the order",
        "function": { "name": "data_write", "input": {
          "connector": "orders-db",
          "op": "insert",
          "target": "orders",
          "values": {
            "customer_id": { "param": "customer_id" },
            "item":        { "param": "item" },
            "total":       { "param": "total" }
          },
          "params": {
            "customer_id": { "var": "data.req.customer_id" },
            "item":        { "var": "data.req.item" },
            "total":       { "var": "data.req.total" }
          },
          "returning": ["id"],
          "output": "data.created"
        } } },
      { "id": "history", "name": "Fetch customer with order history",
        "function": { "name": "data_query", "input": {
          "connector": "orders-db",
          "query": {
            "source": "customers",
            "filter": { "==": [{ "field": "id" }, { "param": "customer_id" }] },
            "include": { "orders": { "fields": ["id", "item", "total"], "limit": 10 } }
          },
          "params": { "customer_id": { "var": "data.req.customer_id" } },
          "schema": {
            "entities": {
              "customers": {
                "relations": {
                  "orders": { "to": "orders", "kind": "has_many", "local": "id", "foreign": "customer_id" }
                }
              },
              "orders": {}
            }
          },
          "output": "data.customer"
        } } }
    ]
  }'

curl -s -X PATCH http://localhost:8080/api/v1/admin/workflows/record-order/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

How the pieces fit:

- **`{ "param": "total" }` marks a value slot.** The `params` map is the only
  place request data enters a query, and every resolved value is a bound
  parameter — never string-interpolated. The dialect is injection-safe by
  construction.
- **The inline `schema` declares `customers has_many orders`.** That relation
  is what powers `"include": { "orders": … }` (and `some`/`all`/`none`
  filters over related records). Without a schema the dialect runs in
  identity mode — names pass through as-is.
- **`"returning": ["id"]`** captures the generated key from the insert.

## 4. Expose it as a service

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/channels \
  -H 'Content-Type: application/json' \
  -d '{ "channel_id": "record-order", "name": "record-order",
        "channel_type": "sync", "protocol": "rest",
        "route_pattern": "/record-order", "methods": ["POST"],
        "workflow_id": "record-order" }'

curl -s -X PATCH http://localhost:8080/api/v1/admin/channels/record-order/status \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

## 5. Call it

```bash
curl -s -X POST http://localhost:8080/api/v1/data/record-order \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "customer_id": 1, "item": "Difference Engine Blueprint", "total": 4200.00 } }'
```

```json
{
  "status": "ok",
  "data": {
    "created": { "rows_affected": 1, "returning": [{ "id": 4 }] },
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

One request: a parameterized insert, then a relation-hydrated read — through a
pooled, circuit-broken, delete-proof connector you configured with one API
call.

## Switching backends

Nothing in the workflow above is Postgres-specific. Point `orders-db` at
MySQL or SQLite and it renders different SQL; point it at a MongoDB or
Elasticsearch connector and the same envelope renders a `find` filter or a
Query DSL search. Switching backends is a connector change, not a rewrite —
see the [Portable Data Dialect](../reference/data-dialect.md) for the full
vocabulary and per-backend notes.

## Next steps

- [Portable Data Dialect](../reference/data-dialect.md) — operators, schema
  registry, relations, write envelopes, safety guards
- [Connectors & Extensibility](../features/extensibility.md) — HTTP, cache,
  MongoDB, Elasticsearch, and Kafka connectors; auth, retries, circuit
  breakers
- [Use Cases & Patterns](../tutorials/use-cases.md) — larger worked examples
