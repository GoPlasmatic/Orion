<!-- description: Create Orion connectors for HTTP APIs, SQL, MongoDB, Redis, Kafka, SMTP and object storage, with secrets by reference and per-operation gates. -->
# Connect Databases & APIs

A connector is how a workflow reaches anything outside the process. You create
one through the admin API, and every workflow references it by name.

## Create one

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/connectors \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "orders-db",
    "connector_type": "db",
    "config": {
      "type": "db",
      "connection_string": "env://ORDERS_DB_URL",
      "max_connections": 5,
      "allow_private_urls": true,
      "operations": { "delete": false, "raw_write": false }
    }
  }'
```

It is live immediately. Connectors have no draft step and no activation — the
registry reloads on every change, and an update replaces the stored config
rather than versioning it.

## Never write a credential into one

```json
{ "connection_string": "env://ORDERS_DB_URL" }
```

The reference resolves from the *server's* environment each time the connector
loads, so the stored row holds a variable name. Three things follow:

- The database never holds the credential, so a dump is not a leak.
- The same JSON works in dev, QA, and production — only the variable's value
  differs.
- The connector survives `export` → `import`. A **literal** credential exports as
  `"******"` and is refused on import, so it cannot be promoted at all.

`vault://<api-path>#<field>` reads HashiCorp Vault. `${VAR}` and
`${VAR:-default}` shell-style substitution also works, which is what the shipped
`postgres-orders` example uses.

If an API wants its credentials **in the query string**, do not put them in the
connector `url` — use
[`query_params`](../reference/connectors.md#query-parameter-precedence), which
keeps the resolved value out of the URL, and therefore out of traces, logs and
error messages.

## Say what it may do

Every connector carries operation gates, all allowed by default. Turning one off
makes the call a validation error regardless of what any workflow asks:

```json
{ "operations": { "read": true, "insert": true, "update": true,
                  "delete": false, "upsert": true, "raw_write": false } }
```

**Both `delete` and `raw_write` must be off to make a SQL connector
delete-proof** — raw SQL cannot be classified per operation, so `db_write` is
gated as a whole.

An HTTP connector gates by method instead, as an allow-list. Empty means every
method; naming even one makes the list exhaustive:

```json
{ "operations": { "methods": ["GET"] } }
```

This is the cheapest blast-radius control Orion offers: it sits below the logic,
and it survives every workflow change.

## Reach a private address on purpose

Connections to private ranges are refused unless the connector says otherwise:

```json
{ "allow_private_urls": true }
```

Most databases and caches *are* private, so most connectors set this. The point
is that reaching an internal address becomes a stated decision, which keeps the
unstated case — a workflow-authored connector reaching a metadata endpoint —
refused by default.

## Test it

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/connectors/orders-db/test
```

For a `db` or `cache` connector this opens a real connection. For `http` it
issues a **real GET with real credentials** — which is the point: a wrong bearer
token is invisible until traffic hits it. A `401` or `403` is reported as *not*
reachable.

Before the server is even running, `orion-server test-connectivity` probes the
configured database and, when enabled, Kafka.

## Use it from a workflow

```json
{ "id": "record", "name": "Insert the order",
  "function": { "name": "data_write", "input": {
    "connector": "orders-db",
    "params": { "total": { "var": "data.req.total" } },
    "write": { "op": "insert", "target": "orders",
               "values": { "total": { "param": "total" } }, "returning": ["id"] },
    "schema": { "entities": { "orders": { "columns": { "id": { "type": "int", "writable": false },
                                                       "total": { "type": "float" } } } } },
    "output": "data.created"
  }}}
```

Two things are doing real work there. `params` is the only door request data
comes through, and every resolved value becomes a bound parameter — so the
dialect is injection-safe by construction. `schema` declares what the task may
touch; undeclared entities and columns are rejected, so a task without one
reaches nothing.

## Choose between the two data APIs

| | `data_query` / `data_write` | `db_read` / `db_write` |
|---|---|---|
| **You write** | A backend-neutral envelope | Raw SQL |
| **Runs against** | SQL, MongoDB, Elasticsearch | SQL only |
| **Injection safety** | By construction — no query text exists | Parameterized, but the text is yours |
| **Bounded by** | The task's `schema`, plus connector gates | Connector gates and the database user |

**Use the portable dialect by default.** Reach for raw SQL only when the dialect
cannot express the query — a window function, a recursive CTE — and know that
you have given up the schema bound when you do.

## Keep it healthy

Each connector has its own circuit breaker and, for HTTP, its own retry policy.
Turn breakers on globally with `engine.circuit_breaker.enabled = true`; they are
off by default. Inspect and reset them at
`/api/v1/admin/connectors/circuit-breakers`. See
[Timeouts, Retries & Circuit Breakers](../operate/failure-handling.md).

## Related

- [Connector Types](../reference/connectors.md) — every field of every type.
- [Portable Data Dialect](../reference/data-dialect.md) — the query and write
  envelope in full.
- [Your First Connector](../getting-started/first-connector.md) — the same job
  as a walkthrough against PostgreSQL.
- [Secure an Instance](../operate/security.md#keep-credentials-out-of-the-database)
  — secrets, masking, and encryption at rest.
