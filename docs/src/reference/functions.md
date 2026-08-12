# Function Reference

A workflow is an ordered list of **tasks**, and every task invokes one built-in
**function** with an `input` object:

```json
{
  "id": "enrich",
  "name": "Look up customer",
  "function": {
    "name": "http_call",
    "input": { "connector": "crm", "path": "/customers/42", "output": "data.customer" }
  }
}
```

Functions read and write the **data context**, the JSON document that flows
through the pipeline; a sync channel returns its `data` object as the response.
The context's exact shape, and how task `condition` expressions are evaluated
against it, are defined in the
[Workflow Reference](./workflows.md#the-data-context).

Orion ships **18 functions** (plus `validate`, an alias for `validation`). Eight
are contributed by the [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs)
engine; ten are Orion handlers that talk to [connectors](./connectors.md)
or compose channels.

| Function | Category | Connector | Purpose |
|----------|----------|:---------:|---------|
| [`parse_json`](#parse_json) | Data | — | Parse the raw payload into the data context |
| [`parse_xml`](#parse_xml) | Data | — | Parse an XML payload into the data context |
| [`map`](#map) | Data | — | Transform/reshape data with JSONLogic |
| [`filter`](#filter) | Data | — | Gate the pipeline on a JSONLogic condition |
| [`validation`](#validation--validate) | Data | — | Collect validation errors from JSONLogic rules |
| [`log`](#log) | Data | — | Emit a structured log line |
| [`publish_json`](#publish_json) | Data | — | Serialize a context field to a JSON string |
| [`publish_xml`](#publish_xml) | Data | — | Serialize a context field to an XML string |
| [`http_call`](#http_call) | Connector | HTTP | Call an external API with retry + circuit breaker |
| [`data_query`](#data_query) | Connector | SQL / MongoDB / ES | Portable, backend-neutral query |
| [`data_write`](#data_write) | Connector | SQL / MongoDB / ES | Portable, backend-neutral insert/update/delete/upsert |
| [`db_read`](#db_read) | Connector | SQL | Run a raw `SELECT`, return rows as JSON |
| [`db_write`](#db_write) | Connector | SQL | Run raw `INSERT`/`UPDATE`/`DELETE`, return affected count |
| [`cache_read`](#cache_read) | Connector | Cache | Read a value from Redis or the in-memory cache |
| [`cache_write`](#cache_write) | Connector | Cache | Write a value to cache with optional TTL |
| [`mongo_read`](#mongo_read) | Connector | MongoDB | Run a raw `find()`, return documents as JSON |
| [`publish_kafka`](#publish_kafka) | Connector | Kafka | Publish a message to a Kafka topic |
| [`channel_call`](#channel_call) | Composition | — | Invoke another channel's workflow in-process |

> [!NOTE]
> The **Category** column above groups the table for reading. It is not the wire
> value: `GET /api/v1/admin/functions` serves a `category` of either `connector`
> or `control` for every function, so tooling should branch on those two rather
> than on the labels here.

> [!NOTE]
> Wherever an input field is described as **JSONLogic**, you pass a JSONLogic
> expression that is evaluated against the data context. A plain JSON literal
> (string, number, object) is also valid JSONLogic and evaluates to itself.

Every field table on this page uses the same **Required** values:

| Value | Meaning |
|-------|---------|
| yes | The field must be present. |
| no | The field is optional; the default applies when it is omitted. |
| one of … | Exactly one of the listed fields must be present. |
| conditional | Required only in the case the Description names. |

---

## Data functions

These come from the dataflow-rs engine. Orion does **not** input-schema-validate
them at workflow-create time (unlike the connector functions below), so an
invalid `input` here surfaces at execution time rather than on create.

### `parse_json`

Reads a raw value (typically the request payload) and parses it as JSON into the
data context. Almost every workflow starts with this — without it, task
conditions that reference `data.*` see an empty context.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `source` | string | yes | — | Where to read the raw value from, e.g. `"payload"` |
| `target` | string | yes | — | Field name under `data`; the parsed value is stored at `data.{target}` |

```json
{ "name": "parse_json", "input": { "source": "payload", "target": "order" } }
```

### `parse_xml`

Same input shape as `parse_json`, but parses an XML payload into a JSON
structure at `data.{target}`.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `source` | string | yes | — | Where to read the raw XML from, e.g. `"payload"` |
| `target` | string | yes | — | Stored at `data.{target}` |

```json
{ "name": "parse_xml", "input": { "source": "payload", "target": "order" } }
```

### `map`

Applies an ordered list of JSONLogic expressions, writing each result to a
dotted path in the context. The primary tool for reshaping, computing, and
enriching data.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `mappings` | array | yes | — | Ordered list of `{ "path", "logic" }` entries |
| `mappings[].path` | string | yes | — | Dotted target path, e.g. `"data.order.total"` |
| `mappings[].logic` | JSONLogic | yes | — | Expression whose result is written to `path` |

```json
{
  "name": "map",
  "input": {
    "mappings": [
      { "path": "data.order.flagged", "logic": true },
      { "path": "data.order.total_with_tax", "logic": { "*": [{ "var": "data.order.total" }, 1.1] } }
    ]
  }
}
```

### `filter`

Evaluates a JSONLogic condition. If it is truthy the pipeline continues;
otherwise the `on_reject` action is taken.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `condition` | JSONLogic | yes | — | Evaluated against the data context |
| `on_reject` | string | no | `"halt"` | `"halt"` stops the whole workflow; `"skip"` skips only this task |

```json
{
  "name": "filter",
  "input": {
    "condition": { ">": [{ "var": "data.order.total" }, 0] },
    "on_reject": "halt"
  }
}
```

### `validation` / `validate`

Evaluates a list of rules. Each rule's `logic` must evaluate to exactly `true`;
any other result records the rule's `message` in the response's error list.
Validation is non-destructive — it never mutates the data context. `validate` is
an accepted alias for `validation`.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `rules` | array | yes | — | List of `{ "logic", "message" }` rules |
| `rules[].logic` | JSONLogic | yes | — | Must evaluate to `true` to pass |
| `rules[].message` | string | yes | — | Error message recorded when the rule fails |

```json
{
  "name": "validation",
  "input": {
    "rules": [
      { "logic": { "!!": [{ "var": "data.order.customer_id" }] }, "message": "customer_id is required" },
      { "logic": { ">": [{ "var": "data.order.total" }, 0] },        "message": "total must be positive" }
    ]
  }
}
```

### `log`

Emits a structured log line. `message` is a JSONLogic expression (a plain string
is valid), and `fields` attaches additional JSONLogic-derived key/values.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `message` | JSONLogic | yes | — | The log message (string literal or expression) |
| `level` | string | no | `"info"` | `trace` \| `debug` \| `info` \| `warn` \| `error` |
| `fields` | object | no | `{}` | Map of name → JSONLogic expression, logged as structured fields |

```json
{
  "name": "log",
  "input": {
    "level": "info",
    "message": "Order processed",
    "fields": { "order_id": { "var": "data.order.id" } }
  }
}
```

### `publish_json`

Serializes a field **inside** the data context to a JSON string and stores it at
another field. (It writes back into the context; it does not publish to an
external system.)

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `source` | string | yes | — | Field under `data` to serialize, e.g. `"order"` (reads `data.order`) |
| `target` | string | yes | — | Field under `data` to receive the serialized string |
| `pretty` | bool | no | `false` | Pretty-print the JSON output |

```json
{ "name": "publish_json", "input": { "source": "order", "target": "order_json", "pretty": true } }
```

### `publish_xml`

Like `publish_json`, but serializes to an XML string.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `source` | string | yes | — | Field under `data` to serialize |
| `target` | string | yes | — | Field under `data` to receive the XML string |
| `root_element` | string | no | `"root"` | Name of the XML root element |

```json
{ "name": "publish_xml", "input": { "source": "order", "target": "order_xml", "root_element": "Order" } }
```

---

## Connector functions

These reference a [connector](./connectors.md) by name — credentials and
endpoints live in the connector, not the workflow. Orion validates their
`input` at workflow create/update time and exposes the schema via
[`GET /api/v1/admin/functions`](./admin-api.md#functions). Connector calls run
through [circuit breakers](./connectors.md) when the global breaker is enabled.

### `http_call`

Makes an HTTP request through an HTTP connector, with retry and circuit-breaker
support. The connector supplies the base URL and auth.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the HTTP connector |
| `method` | string | no | `"GET"` | `GET` \| `POST` \| `PUT` \| `PATCH` \| `DELETE` |
| `path` | string | no | — | Path appended to the connector's base URL |
| `path_logic` | JSONLogic | no | — | Compute the path dynamically (use instead of `path`) |
| `headers` | object | no | `{}` | Extra request headers (string → string) |
| `body` | any | no | — | Static request body (serialized as JSON) |
| `body_logic` | JSONLogic | no | — | Compute the body dynamically (use instead of `body`) |
| `output` | string | no | — | Dotted path where the response body is written; omit to discard it. Accepts the pre-1.0 name `response_path` |
| `timeout_ms` | number | no | `30000` | Per-request timeout in milliseconds |

```json
{
  "name": "http_call",
  "input": {
    "connector": "payment-api",
    "method": "POST",
    "path": "/charge",
    "body_logic": { "var": "data.payment" },
    "output": "data.charge_result",
    "timeout_ms": 5000
  }
}
```

### `data_query`

Runs one **backend-neutral query** against a SQL (PostgreSQL/MySQL/SQLite),
MongoDB, or Elasticsearch connector — the connector decides the rendering
(parameterized SQL via sea-query, a Mongo `find`, or an ES `_search` body).
The full envelope, operator vocabulary, schema registry, and relation support
are documented in the [Portable Data Dialect](./data-dialect.md) reference.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of a `db` or `es` connector |
| `query` | object | yes | — | The query envelope: `source`, `filter`, `fields`, `sort`, `limit`, `skip`, `include` — see the [query envelope](./data-dialect.md#query-envelope-data_query) |
| `params` | object | no | `{}` | Named values referenced as `{ "param": "name" }` inside the filter; each value is JSONLogic resolved against the context |
| `schema` | object | yes | — | Inline entity schema: renames, types, allowlist, relations. Undeclared entities and columns are rejected; `{"unmapped": "identity"}` accepts undeclared names as physical ones |
| `database` | string | conditional | — | Database name; required when the connector is MongoDB (checked at workflow activation), unused otherwise |
| `output` | string | no | `"data"` | Dotted path where the row array is written |

> [!NOTE]
> The `schema` requirement is enforced when the query runs, not when the
> workflow is created: a task without one is accepted at create and refused at
> its first request, with an error naming the key to add. Every entity the
> dialect resolves goes through the schema, so no schema-less call can succeed.

```json
{
  "name": "data_query",
  "input": {
    "connector": "orders-db",
    "query": {
      "source": "orders",
      "filter": { "and": [
        { "==": [{ "field": "customer_id" }, { "param": "cid" }] },
        { ">":  [{ "field": "total" }, 100] }
      ] },
      "sort": [{ "created_at": "desc" }],
      "limit": 20
    },
    "params": { "cid": { "var": "data.customer_id" } },
    "schema": {
      "entities": {
        "orders": {
          "columns": {
            "id": { "type": "int" }, "customer_id": { "type": "int" },
            "total": { "type": "float" }, "created_at": { "type": "timestamp" }
          }
        }
      }
    },
    "output": "data.orders"
  }
}
```

Page sizes are bounded by the [`[query]` config section](./configuration.md)
(`default_limit` / `max_limit`); a query asking for more than the cap is
rejected, never clamped.

### `data_write`

The write counterpart of `data_query`: one **backend-neutral mutation** —
`insert`, `update`, `delete`, or `upsert` — rendered natively for SQL, MongoDB,
or Elasticsearch. The `filter` of an update/delete is the query dialect's
filter, unchanged. See the [Portable Data Dialect](./data-dialect.md) reference
for the full envelope, backend mapping, and safety rules.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of a `db` or `es` connector |
| `write` | object | yes | — | The mutation envelope — fields below |
| `params` | object | no | `{}` | Named values referenced as `{ "param": "name" }` inside `values`, `set`, and `filter`; each value is JSONLogic resolved against the context |
| `schema` | object | yes | — | Inline entity schema: renames, allowlist, `writable` flags. Undeclared entities and columns are rejected; `{"unmapped": "identity"}` accepts undeclared names as physical ones. Enforced at run time, like `data_query`'s |
| `database` | string | conditional | — | Database name; required when the connector is MongoDB (checked at workflow activation), unused otherwise |
| `output` | string | no | `"data"` | Dotted path where the write result is written |

Inside `write`:

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `op` | string | yes | — | `insert` \| `update` \| `delete` \| `upsert` |
| `target` | string | yes | — | Logical entity → table / collection / index |
| `values` | object \| array | conditional | — | Row object(s) to insert; required for `insert` and `upsert` |
| `set` | object | conditional | — | Column → value/param assignments; required for `update`, optional overrides on `upsert` conflict |
| `filter` | JSONLogic | conditional | — | Row selection for `update`/`delete` (same operators as `data_query`); required unless the unfiltered opt-in below is used |
| `on_conflict` | object | conditional | — | `{ "target": [cols], "action": "update" \| "nothing" }`; required for `upsert` |
| `returning` | array | no | — | Columns returned from mutated rows (PostgreSQL/SQLite only) |
| `all` | bool | no | `false` | Acknowledge an intentionally unfiltered update/delete |

```json
{
  "name": "data_write",
  "input": {
    "connector": "orders-db",
    "params": { "id": { "var": "data.order_id" } },
    "schema": {
      "entities": {
        "orders": {
          "columns": {
            "id": { "type": "int", "writable": false }, "status": { "type": "text" }
          }
        }
      }
    },
    "output": "data.write_result",
    "write": {
      "op": "update",
      "target": "orders",
      "set": { "status": "shipped" },
      "filter": { "==": [{ "field": "id" }, { "param": "id" }] }
    }
  }
}
```

Safety guards: unfiltered mutations are rejected unless `"all": true` **and**
`write.allow_unfiltered` are both set; bulk inserts over `write.max_rows` are
rejected; and a connector's
[operation gates](./data-dialect.md#connector-operation-gates) can disable
individual ops entirely. Results are normalized per backend — SQL returns
`{ "status": "ok", "rows_affected": n }` (plus `returning` / `last_insert_id`
where supported); MongoDB and Elasticsearch return doc-store counts
(`inserted`/`ids`, `matched`/`modified`, `deleted`). Every result carries a
`status`; a bulk insert that applied only some of its rows reports
`"partial"` with a per-item array — see
[Bulk writes](./data-dialect.md#bulk-writes).

### `db_read`

The raw-SQL escape hatch for reads — anything outside the portable dialect's
vocabulary (joins, aggregations, CTEs, database-specific SQL). Runs a `SELECT`
against a SQL connector and writes the result rows as a JSON array. Use
placeholders bound from `params` — `?` for SQLite/MySQL, `$1`, `$2`,
… for PostgreSQL.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the SQL connector |
| `query` | string | yes | — | `SELECT` statement with bind placeholders |
| `params` | array | no | — | Values bound to the placeholders, in order |
| `output` | string | no | `"data"` | Dotted path where the row array is written |

```json
{
  "name": "db_read",
  "input": {
    "connector": "primary-db",
    "query": "SELECT id, name, tier FROM customers WHERE id = ?",
    "params": [{ "var": "data.order.customer_id" }],
    "output": "data.customer"
  }
}
```

### `db_write`

The raw-SQL escape hatch for writes (multi-table statements, `UPDATE … FROM`,
SQL functions in `SET`, DDL). Runs an `INSERT`/`UPDATE`/`DELETE` against a SQL
connector and writes `{ "rows_affected": N }`. Note: the author writes
dialect-specific SQL, and a connector can disable this function entirely via
its [`raw_write` operation gate](./data-dialect.md#connector-operation-gates).

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the SQL connector |
| `query` | string | yes | — | `INSERT`/`UPDATE`/`DELETE` statement with bind placeholders |
| `params` | array | no | — | Values bound to the placeholders, in order |
| `output` | string | no | `"data"` | Dotted path where `{ "rows_affected": N }` is written |

```json
{
  "name": "db_write",
  "input": {
    "connector": "primary-db",
    "query": "INSERT INTO orders (id, total) VALUES (?, ?)",
    "params": [{ "var": "data.order.id" }, { "var": "data.order.total" }],
    "output": "data.write_result"
  }
}
```

### `cache_read`

Reads a key from a cache connector (Redis or the built-in in-memory backend).
Missing keys yield `null`.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the cache connector |
| `key` | string | yes | — | Cache key to read |
| `output` | string | no | `"data"` | Dotted path where the value is written |

```json
{ "name": "cache_read", "input": { "connector": "redis", "key": "rate:42", "output": "data.cached" } }
```

### `cache_write`

Writes a key to a cache connector, optionally with a TTL.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the cache connector |
| `key` | string | yes | — | Cache key to set |
| `value` | any | yes | — | Value to store (non-strings are JSON-serialized) |
| `ttl_secs` | number | no | no expiry | Time-to-live in seconds |

```json
{ "name": "cache_write", "input": { "connector": "redis", "key": "rate:42", "value": 1, "ttl_secs": 60 } }
```

### `mongo_read`

The raw escape hatch for MongoDB reads: runs a `find()` with a hand-written
Mongo filter document and writes the matched documents as a JSON array. For
backend-portable queries, prefer [`data_query`](#data_query).

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the MongoDB connector |
| `database` | string | yes | — | Database name |
| `collection` | string | yes | — | Collection name |
| `filter` | object | no | `{}` | MongoDB find filter document |
| `output` | string | no | `"data"` | Dotted path where matched documents are written |

```json
{
  "name": "mongo_read",
  "input": {
    "connector": "mongo",
    "database": "shop",
    "collection": "customers",
    "filter": { "tier": "vip" },
    "output": "data.vips"
  }
}
```

### `publish_kafka`

Publishes a message to a Kafka topic through a Kafka connector. Requires Kafka to
be enabled in config. If `value_logic` is omitted, the full data context is
published.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the Kafka connector |
| `topic` | string | yes | — | Target topic |
| `key_logic` | JSONLogic | no | — | Expression that derives the message key |
| `value_logic` | JSONLogic | no | full `data` | Expression that derives the message value |

```json
{
  "name": "publish_kafka",
  "input": {
    "connector": "events",
    "topic": "order.placed",
    "key_logic": { "var": "data.order.id" },
    "value_logic": { "var": "data.order" }
  }
}
```

---

## Composition functions

### `channel_call`

Invokes another channel's workflow **in-process** — no network hop. The called
channel keeps its own versioning and governance. Cycle detection and a max call
depth prevent runaway recursion. Provide exactly one of `channel`/`channel_logic`
and at most one of `data`/`data_logic`.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `channel` | string | one of `channel`/`channel_logic` | — | Static target channel name |
| `channel_logic` | JSONLogic | one of `channel`/`channel_logic` | — | Expression that resolves to the target channel name |
| `data` | any | no | request payload | Static payload passed to the target channel |
| `data_logic` | JSONLogic | no | — | Expression that derives the payload |
| `output` | string | no | `"data"` | Dotted path where the called channel's response is stored. Accepts the pre-1.0 name `response_path` |
| `timeout_ms` | number | no | from config | Per-call timeout in milliseconds |

```json
{
  "name": "channel_call",
  "input": {
    "channel": "customer-lookup",
    "data_logic": { "var": "data.order.customer_id" },
    "output": "data.customer"
  }
}
```

---

## Inspecting schemas at runtime

`GET /api/v1/admin/functions` returns the live input schema for the connector and
composition functions (the data functions are provided by dataflow-rs and are not
cataloged there). The [Orion CLI MCP server](../ai/mcp-setup.md) surfaces
the same schemas to AI assistants so generated workflows use correct field names.

## Related

- [Workflow Reference](./workflows.md) — the workflow schema, the data context,
  and how task conditions select which tasks run.
- [Portable Data Dialect](./data-dialect.md) — the full envelope, operator
  vocabulary, and backend parity rules behind `data_query`/`data_write`.
- [Connectors](./connectors.md) — per-type connector fields, retries, and
  circuit-breaker behaviour.
- [Admin API](./admin-api.md) — creating, validating, and activating the
  workflows these functions run in.
