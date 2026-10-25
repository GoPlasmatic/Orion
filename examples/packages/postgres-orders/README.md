# postgres-orders — connectors and the portable data dialect

The other examples are deliberately zero-dependency. This one shows the part
of Orion they can't: a workflow talking to a real database through a
**connector**, using the portable data dialect
([`data_query` / `data_write`](../../../docs/src/reference/data-dialect.md)).

`POST /record-order` does two things in one pipeline:

1. **`data_write`** inserts the incoming order — every value a bound
   parameter, `RETURNING id` to capture the new row's key.
2. **`data_query`** returns the customer with their order history — a
   declared `has_many` relation hydrated via `include`.

The queries are backend-neutral: point the connector at MySQL or SQLite
instead of Postgres and the same workflow JSON runs unchanged.

## Run it

```bash
docker compose up -d          # Postgres (seeded) + Orion
cd ../.. && orion-server compile examples/packages/postgres-orders -o dist/postgres-orders
orion-cli deployments apply -d dist/postgres-orders
```

Or run a locally installed `orion-server` against just the database:

```bash
docker compose up -d postgres
ORDERS_DB_URL=postgres://orion:orion@localhost:5432/orion_orders orion-server
# in another terminal:
cd ../.. && orion-server compile examples/packages/postgres-orders -o dist/postgres-orders
orion-cli deployments apply -d dist/postgres-orders
```

> Requires Orion ≥ 1.0.0 — the dialect envelope changed shape for 1.0 (see
> the [upgrade guide](../../../docs/src/operate/upgrading-to-1.0.md)), and this
> example is written against the 1.0 form.

Then send the sample request:

```bash
curl -s -X POST http://localhost:8080/api/v2/data/record-order \
  -H 'Content-Type: application/json' --data @examples/packages/postgres-orders/request.json
```

Expected response (ids vary):

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

Applying the same directory again is a no-op, but each request still inserts a
new order, so the history grows.

## What to look at

**`connector.json`** — the two governance features worth copying:

- The connection string is a secret reference, `env://ORDERS_DB_URL`. The
  server resolves it from its own environment when the connector loads, so
  the saved config carries no credentials and the same JSON works in compose
  (`postgres` host, set in `docker-compose.yml`) and on a laptop
  (`localhost`).
- `"driver": "postgresql"` names the backend the reference hides. A
  definition set must name it on every `db` connector: it is how
  `orion-server compile` picks the connection's
  provider.
- `"operations": { "delete": false }` makes the connector **delete-proof**:
  any workflow that tries a `data_write` delete through it is rejected at
  the connector, no matter what its tasks say.

**`workflow.json`** — the dialect's token model in action:

- `{ "param": "total" }` marks a value slot; the `params` map is the only
  place workflow data enters the query, and every value is bound, never
  interpolated — injection-safe by construction.
- The inline `schema` declares `customers has_many orders`, which is what
  makes `"include": { "orders": ... }` (and `some`/`all`/`none` filters over
  orders) work.

**`seed.sql`** — plain SQL, loaded by the Postgres container on first start.
