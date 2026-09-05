<!-- description: Every built-in Orion task function with its input schema: parse, map, filter, validation, HTTP and channel calls, SQL, MongoDB, cache, Kafka, email and JWT. -->
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

When a function fails, the data-plane response follows [Errors & Response
Envelopes](./errors.md). Use its stable error code and the trace's task ID to
identify the failing step; do not parse the message text. Individual entries
below describe function-specific validation and runtime failures.

The index below lists the functions available in the release documented by
this site. Some are contributed by the
[dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) engine; the rest are
Orion handlers that talk to [connectors](./connectors.md), compose channels, or
compute locally. Query `GET /api/v1/admin/functions` for the authoritative list
on a running instance.

<div class="table-filter" data-label="Filter functions"></div>

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
| [`mongo_write`](#mongo_write) | Connector | MongoDB | Insert/update/replace/delete documents, nested shapes included |
| [`mongo_aggregate`](#mongo_aggregate) | Connector | MongoDB | Run a stage-allowlisted aggregation pipeline |
| [`publish_kafka`](#publish_kafka) | Connector | Kafka | Publish a message to a Kafka topic |
| [`send_email`](#send_email) | Connector | SMTP | Send transactional email through an SMTP connector |
| [`storage_presign`](#storage_presign) | Connector | Storage | Compute a time-limited presigned object URL — no data path |
| [`storage_head`](#storage_head) | Connector | Storage | Object metadata (exists/size/etag) |
| [`channel_call`](#channel_call) | Composition | — | Invoke another channel's workflow in-process |
| [`crypto`](#crypto) | Utility | — | Digests, HMAC compute/verify, password hashing |
| [`jwt_sign`](#jwt_sign) | Utility | — | Mint a signed JWT (login, refresh, client assertions) |
| [`jwt_verify`](#jwt_verify) | Utility | — | Verify a JWT against static keys or a JWKS |

> [!NOTE]
> The **Category** column above groups the table for reading. It is not the wire
> value: `GET /api/v1/admin/functions` serves a `category` of `connector`,
> `control`, `data`, or `utility` for every function, so tooling should branch
> on those rather than on the labels here.
>
> That endpoint serves **all** of the functions above. Functions contributed by
> the engine carry `source: "engine"` and **no** `input_fields`, because Orion
> declares no schema for them and so does not input-validate them at create
> time; the rest carry `source: "orion"` and their schema. `validation` carries
> `validate` in `aliases` rather than appearing twice.

## Retry safety

Orion retries a task in more places than a workflow author necessarily has in
mind: the [trace DLQ](./admin-api.md#trace-dlq) replays a failed async delivery,
a Kafka redelivery re-runs everything after an uncommitted offset, and
`http_call` retries its own transport failures. Whether that is harmless
depends on the function.

This is a different question from whether an *error* was transient. Orion
already classifies that per error — a connection failure is retryable, a
rejected query is not. The table below answers the other half: **if the retry
happens, what does it cost?**

`GET /api/v1/admin/functions` serves the same answer per function as
`retry_safety`, so tooling can read it rather than hard-coding this table.

| Answer | Meaning |
|---|---|
| `pure` | No effect outside the message. Free to retry. |
| `read` | Observes state without changing it. A retry costs a round trip and may see a newer value. |
| `idempotent_write` | Writes, but a second run lands the same end state. |
| `unsafe_write` | Writes, and a second run duplicates the effect — the second email, the second record. |
| `depends_on` | The task decides, and the answer carries the input to look at. |

| Function | Retry safety | Notes |
|---|---|---|
| [`crypto`](#crypto) | `pure` | Local computation. |
| [`jwt_sign`](#jwt_sign) | `pure` | Local signing. |
| [`storage_presign`](#storage_presign) | `pure` | SigV4 arithmetic over the connector's credentials; zero bytes move. |
| [`cache_read`](#cache_read) | `read` | |
| [`db_read`](#db_read) | `read` | |
| [`data_query`](#data_query) | `read` | |
| [`mongo_read`](#mongo_read) | `read` | |
| [`mongo_aggregate`](#mongo_aggregate) | `read` | Aggregation pipelines with `$out`/`$merge` are refused by the stage allowlist, so this stays a read. |
| [`storage_head`](#storage_head) | `read` | Metadata only. |
| [`jwt_verify`](#jwt_verify) | `read` | May fetch a JWKS document; the cache usually answers. |
| [`cache_write`](#cache_write) | `idempotent_write` | The same key and value land the same entry. A `ttl` restarts from the retry. |
| [`send_email`](#send_email) | `unsafe_write` | A retry sends a second message. |
| [`publish_kafka`](#publish_kafka) | `unsafe_write` | A retry publishes a second record. Consumers that need exactly-once should dedupe on a key the workflow sets. |
| [`http_call`](#http_call) | `depends_on` `method` | `GET`/`HEAD` are safe; `POST`/`PATCH` may already have been applied. This is what the connector's `retry_non_idempotent` flag is about — off by default. |
| [`db_write`](#db_write) | `depends_on` `sql` | Raw SQL: an `UPDATE … SET x = 1` is idempotent, an `INSERT` is not. |
| [`data_write`](#data_write) | `depends_on` `op` | `upsert` and `delete` are idempotent; `insert` is not; `update` depends on the expression. |
| [`mongo_write`](#mongo_write) | `depends_on` `op` | Same split: an upsert repeats safely, an insert does not. |
| [`channel_call`](#channel_call) | `depends_on` `channel` | The answer is whatever the target channel's workflow does. |

Every engine built-in (`map`, `filter`, `parse_json`, …) is `pure`: they read
and write the message and nothing else. `log` writes to this node's own
observability output, which a retry repeating is not a duplicated effect in the
sense above.

> [!NOTE]
> Wherever an input field is described as **JSONLogic**, you pass a JSONLogic
> expression that is evaluated against the data context. A plain JSON literal
> (string, number, object) is also valid JSONLogic and evaluates to itself, and
> costs nothing: it is folded once when the engine is built, and only a field
> that actually reads the message is evaluated per request.
>
> That now covers most fields, `output` included — a computed destination fans
> one task's results out by message content. The exceptions are deliberate and
> fall into three groups: **target selectors** (`connector`, a Mongo `database`
> and `collection`, `data_query.database`), because the static name is what the
> dependency list, the activation gate and the connector rename guard are built
> from; **validated enums and security switches** (`crypto.op` and its
> encodings, `jwt_sign.algorithm`, `jwt_verify.algorithms` and `jwks_url`,
> `http_call.method`, `storage_presign.method`, `mongo_write.op` and `all`,
> raw-SQL `query`, `data_*.schema`), where computing the value would move the
> check past the point an author can be told about it; and the
> **document-shaped fields** listed under
> [Connector fields](./expressions.md#connector-fields-expressions-and-documents),
> which fold `{"var": …}` and nothing else.

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

> [!WARNING]
> **A failed rule records an error; it does not stop the workflow — unless you
> say so.** The task returns a `400`, the message lands in the response's
> `errors` array, and the engine's rule is that `4xx` warns and carries on;
> `continue_on_error` governs `5xx` and handler errors only. So a `validation`
> followed by unguarded tasks proceeds exactly as if it had passed.
>
> Collecting every failure and carrying on is a legitimate shape, which is why
> it is the default. When you meant a gate, add
> [`halt_on`](./workflows.md#halting-on-failure):
>
> ```json
> { "id": "check", "name": "Check", "halt_on": "failure",
>   "function": { "name": "validation", "input": { "rules": [
>     { "logic": { "==": [1, 1] }, "message": "…" } ] } } }
> ```
>
> The task keeps its own `400` on the audit trail and in `metadata.progress`.
> Two older spellings still work and are better when you need something else:
> [`filter`](#filter) halts with no body and records `299`, and a later task
> with a `condition` on the failure plus `terminal: true` is the only form that
> can answer with a status of its own.
>
> `terminal: true` on the `validation` itself does not help — it is about
> [position, not outcome](./workflows.md#terminal-steps), so it halts whether
> the rules passed or failed. `orion-server lint` reports the unguarded shape
> as `engine.unguarded_validation`.

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
is valid), and `fields` attaches more JSONLogic-derived key/values.

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
| `connector` | string \| JSONLogic | yes | — | Name of the HTTP connector. A computed name is not yet supported |
| `method` | string | no | `"GET"` | `GET` \| `POST` \| `PUT` \| `PATCH` \| `DELETE`. The one field here that is not JSONLogic |
| `path` | string \| JSONLogic | no | — | Path appended to the connector's base URL. Accepts the pre-1.0 name `path_logic` |
| `headers` | object | no | `{}` | Extra request headers. Each **value** is JSONLogic |
| `body` | any \| JSONLogic | no | — | Request body. Accepts the pre-1.0 name `body_logic` |
| `body_format` | string \| JSONLogic | no | `"json"` | How the body becomes request bytes: `json`, `form`, or `text` — see below |
| `output` | string \| JSONLogic | no | — | Dotted path where the response body is written; omit to discard it. Accepts the pre-1.0 name `response_path` |
| `response_format` | string \| JSONLogic | no | `"json"` | How the response is captured at `output`: `json` (parsed) or `text` (a plain string) |
| `timeout_ms` | number \| JSONLogic | no | `30000` | Per-request timeout in milliseconds |

Every field above except `method` is JSONLogic, so it may be written as a plain
literal, which is what it evaluates to, or as an expression over the message.
A literal is folded once when the engine is built and costs nothing per request;
only a field that actually reads the message pays. `headers` is the one that
changes what is expressible: a value can now be computed, so a bearer token or a
correlation id no longer has to be injected by the service layer.

```json
{
  "name": "http_call",
  "input": {
    "connector": "partner-api",
    "method": "POST",
    "path": { "cat": ["/orders/", { "var": "data.order_id" }] },
    "headers": {
      "Authorization": { "cat": ["Bearer ", { "secret": "partner_token" }] },
      "X-Correlation-Id": { "var": "metadata.request_id" }
    },
    "body": { "var": "data.order" },
    "output": "data.result"
  }
}
```

```json
{
  "name": "http_call",
  "input": {
    "connector": "payment-api",
    "method": "POST",
    "path": "/charge",
    "body": { "var": "data.payment" },
    "output": "data.charge_result",
    "timeout_ms": 5000
  }
}
```

**Body formats.** `json` (the default) serializes the body as JSON. `form`
URL-encodes an object's entries as `application/x-www-form-urlencoded` pairs —
what OAuth 2.0 token endpoints and form-style APIs require: scalars encode
directly, arrays of scalars become repeated keys (`to=a&to=b`), `null` entries
are skipped (so one body shape with conditionally-null entries expresses
optional parameters), and nested values are rejected — a bracket path like
`"metadata[order_id]"` is just an ordinary key. `text` sends a string body
verbatim, which with an explicit `content-type` header covers XML, CSV, or any
other textual payload. Each format stamps its own `content-type`
(`application/json`, `application/x-www-form-urlencoded`,
`text/plain; charset=utf-8`); a `content-type` set in `headers` or on the
connector replaces the stamp — it changes the label, never the bytes.

**Response formats.** `json` (the default) parses the response and fails if it
is not valid JSON. `text` captures the body as a plain string — for gateways
that answer `text/plain` — leaving the size cap and the non-2xx error path
unchanged. Unknown values on either axis are rejected when the workflow is
created **when both are written as literals**, and a literal `body` is
shape-checked against a literal `body_format` at the same time; a computed body
or format gets the same check per request instead.

```json
{
  "name": "http_call",
  "input": {
    "connector": "webex-oauth",
    "method": "POST",
    "path": "/v1/access_token",
    "body_format": "form",
    "body_logic": {
      "grant_type": "refresh_token",
      "refresh_token": { "var": "temp_data.refresh_token" }
    },
    "output": "temp_data.token_response"
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
| `numeric_as` | string | no | `"number"` | How a `numeric`/`decimal` column is rendered: `number` or `string` — see [Decimal columns](#decimal-columns). SQL backends only |
| `binary_as` | string | no | `"auto"` | How a binary column is rendered: `auto`, `hex`, `base64` or `text` — see [Binary columns](#binary-columns). SQL backends only |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the row array is written |

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
| `numeric_as` | string | no | `"number"` | How a `numeric`/`decimal` column is rendered: `number` or `string` — see [Decimal columns](#decimal-columns). SQL backends only |
| `binary_as` | string | no | `"auto"` | How a binary column is rendered: `auto`, `hex`, `base64` or `text` — see [Binary columns](#binary-columns). SQL backends only |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the write result is written |

#### Updating array elements

Three path forms reach elements inside an array field, and **the simplest one that fits is the right one**:

| Path | Updates | Needs `array_filters` |
|---|---|---|
| `sessions.$.active` | the **first** element the `filter` matched | no |
| `sessions.$[].active` | **every** element, unconditionally | no |
| `sessions.$[s].active` | every element matching an `array_filters` entry | yes |

For "flip the one embedded entry whose `deviceId` matches", `$` is enough — atomically, in one round trip, with no `array_filters`:

```json
{ "op": "update_one",
  "filter": { "_id": {"var": "temp_data.user_id"},
              "sessions.deviceId": {"var": "data.deviceId"} },
  "update": { "$set": { "sessions.$.active": false } } }
```

`array_filters` is for what `$` and `$[]` cannot express: updating **every** element matching a predicate, reaching **nested** arrays (`$[a].items.$[b]`), and using several independent identifiers in one update.

```json
{ "op": "update_many",
  "filter": { "_id": {"var": "temp_data.user_id"} },
  "update": { "$set": { "sessions.$[s].active": false } },
  "array_filters": [ { "s.expiresAt": { "$lt": { "$date": {"var": "temp_data.now"} } } } ] }
```

Each entry constrains exactly one identifier (`$and`/`$or`/`$nor` take theirs from their branches). Orion cross-checks the two before the driver call: an identifier with no filter, a filter nothing uses, or `array_filters` with no `$[identifier]` anywhere is a `400` naming the problem — MongoDB refuses all three, but its message would reach you as an opaque `500`.

`upsert: true` is permitted; note that on the *insert* branch there is no array to match. A filter matching no element is **not** an error — the update succeeds with `matched: 1, modified: 0`, which the result envelope reports faithfully.

> [!NOTE]
> Whole-array `$set` — read the array, modify it in memory, write it back — is racy: two concurrent writers each write the full array and the second silently clobbers the first. Orion has no transaction surface to fix that, so prefer a positional path, which the server applies atomically.

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
`"partial"` with a per-item array. See
[Bulk writes](./data-dialect.md#bulk-writes).

### `db_read`

The raw-SQL escape hatch for reads — anything outside the portable dialect's
vocabulary (joins, aggregations, CTEs, database-specific SQL). Runs a `SELECT`
against a SQL connector and writes the result rows as a JSON array. Use
placeholders bound from `params` — `?` for SQLite/MySQL, `$1`, `$2`,
… for PostgreSQL.

**Reads only.** The statement must open with `SELECT`, `WITH`, `VALUES` or
`TABLE`, and a `WITH` carrying a data-modifying CTE
(`WITH gone AS (DELETE … RETURNING …) …`) is refused. `EXPLAIN` is not
admitted either — `EXPLAIN ANALYZE DELETE …` executes the delete. A statement
that writes belongs in [`db_write`](#db_write), which has its own `raw_write`
[operation gate](./data-dialect.md#connector-operation-gates); that gate is
what makes a connector delete-proof, and it only holds because `db_read`
cannot write.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the SQL connector |
| `query` | string | yes | — | `SELECT` statement with bind placeholders |
| `params` | array | no | — | Values bound to the placeholders, in order |
| `numeric_as` | string | no | `"number"` | How a `numeric`/`decimal` column is rendered: `number` or `string` — see [Decimal columns](#decimal-columns) |
| `binary_as` | string | no | `"auto"` | How a binary column is rendered: `auto`, `hex`, `base64` or `text` — see [Binary columns](#binary-columns). |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the row array is written |

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

#### Decimal columns

`numeric` (PostgreSQL) and `decimal` (MySQL) are arbitrary precision, and JSON
has no equivalent. `numeric_as` decides which way the mismatch resolves:

| Value | Result |
|---|---|
| `"number"` (default) | A JSON number — computable in JSONLogic, and **rounded** beyond 2^53 or on most decimal fractions |
| `"string"` | The exact decimal as a string; arithmetic needs an explicit cast in the workflow |

The default is the convenient one, not the safe one. For a money column use
`"string"`: a silently rounded total is a correctness bug the caller cannot
see, and the cast the string forces is the point — it makes the loss a
decision rather than an accident. `bigint` is unaffected either way, because a
64-bit integer is exact in JSON.

SQLite has no static column types — a `NUMERIC` column stores whichever storage
class the value fits, so the setting has nothing to act on there and is
ignored.

#### Binary columns

A `bytea` (PostgreSQL), `blob` (SQLite) or `binary`/`varbinary`/`blob` (MySQL)
column has no JSON form either. `binary_as` decides which way that resolves:

| Value | Result |
|---|---|
| `"auto"` (default) | The bytes as text when they are valid UTF-8, lowercase hex when they are not |
| `"hex"` | Lowercase hex, whatever the bytes are |
| `"base64"` | Standard padded base64, whatever the bytes are |
| `"text"` | The bytes as UTF-8 text, or a **`400` naming the column** when they are not |

`auto` is the default because MySQL reports `TEXT` and `JSON` columns as
`BLOB` — so text is the right answer far more often than not — and because it
is what every task written before this setting existed already reads.

It is also the one mode whose **result shape is decided by the data**: two rows
of the same column can come back as text and as hex, with nothing in the result
telling them apart, so a workflow that hex-decodes the column breaks the first
time a value happens to be valid UTF-8. For a column that is genuinely binary,
name an encoding. This is the same trade `numeric_as` makes, and it resolves
the same way: the default is the convenient one, not the safe one.

Unlike `numeric_as`, this setting **does** apply on SQLite — `BLOB` is a
storage class, so it survives the round trip a declared type does not.

#### Column types

Rows are decoded on the connector's real driver, so a `SELECT *` over an
ordinary schema works: `uuid`, `json`/`jsonb` (as the value, not a string to
re-parse), `numeric`, the date/time family (as RFC 3339 / ISO 8601 strings),
arrays, enums and domains all have JSON forms. A `json`/`jsonb` column comes
back as the document itself, so `parse_json` is not needed after a read.

`char(n)` and PostgreSQL's `citext` decode as strings, scalar and array alike.

A type with no JSON form here — `inet`, `interval`, a composite, a range, and
PostgreSQL's internal one-byte `"char"` (which is not `char(n)`) — is a
**`400` naming the column and its SQL type**, not a `500`. The remedy is in the
message: cast it in the query (`SELECT extra::text`) and use
[`parse_json`](#parse_json) if it holds a document.

#### Boolean columns

A MySQL `BOOLEAN` / `BOOL` / `TINYINT(1)` column reads back as a JSON
**boolean**, the same as a PostgreSQL `bool`. MySQL has no boolean type — all
three spellings are `TINYINT(1)`, but the width-1 declaration is the
convention every framework writes and the only one MySQL 8 still preserves, so
it is treated as the boolean it is meant to be. A `TINYINT` *without* the width
is a different column and stays a number; if you are genuinely storing a small
integer in a `TINYINT(1)`, select it as `flags + 0` to get one back.

SQLite is the exception, and it cannot be otherwise: a value there carries a
storage class rather than a declared type, so a column declared `BOOLEAN` is
indistinguishable from an integer by the time the row is read and comes back as
`1` / `0`. This is the same reason `numeric_as` has nothing to act on there.

**Parameters are the other direction, and they still need a cast.** A `params`
entry is bound by its JSON type, so a string goes out as `text`. PostgreSQL
types its parameters and has no `text = uuid` operator, so a comparison against
a `uuid`, `numeric` or `timestamptz` column is written `WHERE id = ($1)::uuid`.
Orion does not infer the SQL type from the shape of the value — a string that
looked like a UUID would then bind as one and fail against a `text` column.
SQLite and MySQL coerce, so this affects PostgreSQL only.

> Before 1.6 these queries ran through a driver-agnostic layer that could
> decode nine PostgreSQL types. Everything else failed the task with a `500` —
> and only when a row existed, so a query passed every test against an empty
> table and failed the first time production had data.

### `db_write`

The raw-SQL escape hatch for writes (multi-table statements, `UPDATE … FROM`,
SQL functions in `SET`, DDL). Runs an `INSERT`/`UPDATE`/`DELETE` against a SQL
connector and writes `{ "rows_affected": N }`. Note: the author writes
dialect-specific SQL, and a connector can disable this function entirely via
its [`raw_write` operation gate](./data-dialect.md#connector-operation-gates).

An `INSERT` also carries `last_insert_id` on MySQL and SQLite, the same key
[`data_write`](#data_write) reports. PostgreSQL does not report one — it uses
`RETURNING`, which the portable dialect supports. The key appears for an
`INSERT`/`REPLACE` only: SQLite's `last_insert_rowid` belongs to the
*connection*, so after an `UPDATE` it would report whatever an earlier insert
on that pooled connection left behind.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the SQL connector |
| `query` | string | yes | — | `INSERT`/`UPDATE`/`DELETE` statement with bind placeholders |
| `params` | array | no | — | Values bound to the placeholders, in order |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where `{ "rows_affected": N }` (plus `last_insert_id` after an insert) is written |

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
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the value is written |

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

Documents are **extended JSON**: BSON types with an extended-JSON spelling —
`{"$oid": "…"}` for an ObjectId, `{"$date": "…"}` for a typed date, and the
rest of the family — become real BSON values in the filter, and come back in
their canonical spellings in the output, so a value read from one document can
drive the next task's filter unchanged.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the MongoDB connector |
| `database` | string | yes | — | Database name |
| `collection` | string | yes | — | Collection name |
| `filter` | object | no | `{}` | MongoDB find filter document (extended JSON) |
| `projection` | object | no | all fields | MongoDB projection document, e.g. `{"name": 1, "_id": 0}` |
| `sort` | object | no | natural order | MongoDB sort document, e.g. `{"created_at": -1}` |
| `limit` | number | no | unlimited* | Maximum documents to return; must not exceed `query.max_limit` |
| `skip` | number | no | `0` | Documents to skip; must not exceed `query.max_skip` |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where matched documents are written |

\* an unlimited read is still bounded: a result larger than `query.max_limit`
is an error rather than an OOM.

```json
{
  "name": "mongo_read",
  "input": {
    "connector": "mongo",
    "database": "shop",
    "collection": "customers",
    "filter": { "tier": "vip", "since": { "$gte": { "$date": "2024-01-01T00:00:00Z" } } },
    "sort": { "since": -1 },
    "limit": 50,
    "output": "data.vips"
  }
}
```

### `mongo_write`

The write twin of [`mongo_read`](#mongo_read): inserts, updates, replaces, or
deletes documents with hand-written Mongo documents — nested arrays and
objects included, since every document field is extended JSON. For
backend-portable mutations, prefer [`data_write`](#data_write).

`op` is an open value set; each op reads a specific subset of the fields
below, and naming a field the op ignores is an authoring-time error.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the MongoDB connector |
| `database` | string | yes | — | Database name |
| `collection` | string | yes | — | Collection name |
| `op` | string | yes | — | `insert_one`, `insert_many`, `update_one`, `update_many`, `replace_one`, `delete_one`, or `delete_many` |
| `document` | object | conditional | — | The document for `insert_one` / `replace_one` (a replacement must be a plain document, no `$` operators) |
| `documents` | array | conditional | — | Documents for `insert_many`; the batch is capped by `write.max_rows` |
| `filter` | object | conditional | — | Selection filter for update/replace/delete ops (extended JSON) |
| `update` | object | conditional | — | Update document for `update_one`/`update_many`; top-level keys must be atomic operators (`$set`, `$inc`, `$push`, …). Field paths may target array elements — see [Updating array elements](#updating-array-elements) |
| `array_filters` | array | no | — | `update_one`/`update_many` only: filter documents naming the `$[identifier]` paths used in `update` |
| `upsert` | bool | no | `false` | Insert when nothing matches (update/replace ops). Gated as `upsert` on the connector when true, `update` otherwise |
| `ordered` | bool | no | `true` | `insert_many` only: stop at the first failure (`true`) or attempt every document (`false`) |
| `all` | bool | no | `false` | Acknowledge an intentionally unfiltered update/replace/delete — also requires `write.allow_unfiltered` in config |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the write result is written |

#### Updating array elements

Three path forms reach elements inside an array field, and **the simplest one that fits is the right one**:

| Path | Updates | Needs `array_filters` |
|---|---|---|
| `sessions.$.active` | the **first** element the `filter` matched | no |
| `sessions.$[].active` | **every** element, unconditionally | no |
| `sessions.$[s].active` | every element matching an `array_filters` entry | yes |

For "flip the one embedded entry whose `deviceId` matches", `$` is enough — atomically, in one round trip, with no `array_filters`:

```json
{ "op": "update_one",
  "filter": { "_id": {"var": "temp_data.user_id"},
              "sessions.deviceId": {"var": "data.deviceId"} },
  "update": { "$set": { "sessions.$.active": false } } }
```

`array_filters` is for what `$` and `$[]` cannot express: updating **every** element matching a predicate, reaching **nested** arrays (`$[a].items.$[b]`), and using several independent identifiers in one update.

```json
{ "op": "update_many",
  "filter": { "_id": {"var": "temp_data.user_id"} },
  "update": { "$set": { "sessions.$[s].active": false } },
  "array_filters": [ { "s.expiresAt": { "$lt": { "$date": {"var": "temp_data.now"} } } } ] }
```

Each entry constrains exactly one identifier (`$and`/`$or`/`$nor` take theirs from their branches). Orion cross-checks the two before the driver call: an identifier with no filter, a filter nothing uses, or `array_filters` with no `$[identifier]` anywhere is a `400` naming the problem — MongoDB refuses all three, but its message would reach you as an opaque `500`.

`upsert: true` is permitted; note that on the *insert* branch there is no array to match. A filter matching no element is **not** an error — the update succeeds with `matched: 1, modified: 0`, which the result envelope reports faithfully.

> [!NOTE]
> Whole-array `$set` — read the array, modify it in memory, write it back — is racy: two concurrent writers each write the full array and the second silently clobbers the first. Orion has no transaction surface to fix that, so prefer a positional path, which the server applies atomically.

The result mirrors `data_write`'s Mongo envelopes: inserts report
`{ "status", "inserted", "ids" }` (a partially applied `insert_many` reports
per-item outcomes and audits as **207**, exactly like `data_write`); updates
and replaces report `{ "status", "matched", "modified", "upserted_id"? }`;
deletes report `{ "status", "deleted" }`.

```json
{
  "name": "mongo_write",
  "input": {
    "connector": "mongo",
    "database": "shop",
    "collection": "meetings",
    "op": "update_one",
    "filter": { "_id": { "$oid": { "var": "data.payload.object.id" } } },
    "update": { "$set": {
      "payload": { "var": "data.payload" },
      "updated_at": { "$date": { "var": "metadata.timestamp" } },
      "deleted": false
    } },
    "upsert": true,
    "output": "temp_data.write_result"
  }
}
```

### `mongo_aggregate`

Runs an aggregation pipeline — the surface `find()` cannot reach: `$group`,
`$unwind`, `$lookup`, `$facet`, and the rest. Stages are extended JSON, and
**stage names are allowlisted**: the read-only stages are always available,
while the write stages `$out`/`$merge` run only on a connector that sets
[`aggregate_write_stages: true`](./connectors.md#db) (default
false — an aggregation must not silently write). An unknown stage is refused
by name, at authoring time for a literal pipeline and again at runtime after
`{"var": ..}` substitution, so message data cannot smuggle a stage in.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the MongoDB connector |
| `database` | string | yes | — | Database name |
| `collection` | string | yes | — | Collection name |
| `pipeline` | array | yes | — | Aggregation stages, each `{"$stage": …}` (extended JSON) |
| `allow_disk_use` | bool | no | `false` | Let the server spill large stages to disk |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where result documents are written |

Results are bounded by `query.max_limit` like `mongo_read`; a `$out`/`$merge`
pipeline returns an empty array (Mongo's own contract for those stages).

```json
{
  "name": "mongo_aggregate",
  "input": {
    "connector": "mongo",
    "database": "shop",
    "collection": "recordings",
    "pipeline": [
      { "$match": { "meetingId": { "var": "data.meeting_id" } } },
      { "$unwind": "$videos" },
      { "$group": { "_id": "$videos.quality", "count": { "$sum": 1 } } }
    ],
    "output": "temp_data.by_quality"
  }
}
```

### `publish_kafka`

Publishes a message to a Kafka topic through a Kafka connector. Requires Kafka to
be enabled in config. If `value` is omitted, the full data context is published.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string \| JSONLogic | yes | — | Name of the Kafka connector. A computed name is not yet supported |
| `topic` | string \| JSONLogic | yes | — | Target topic. Accepts an expression, so one task can route by message content |
| `key` | any \| JSONLogic | no | — | The message key. Accepts the pre-1.0 name `key_logic` |
| `value` | any \| JSONLogic | no | full `data` | The message value. Accepts the pre-1.0 name `value_logic` |

```json
{
  "name": "publish_kafka",
  "input": {
    "connector": "events",
    "topic": "order.placed",
    "key": { "var": "data.order.id" },
    "value": { "var": "data.order" }
  }
}
```

A computed `topic` is what lets one task fan a stream out by content — the
tenant, the region, the event type, where before it took one task per
destination:

```json
{
  "name": "publish_kafka",
  "input": {
    "connector": "events",
    "topic": { "cat": ["orders.", { "var": "data.region" }] },
    "value": { "var": "data.order" }
  }
}
```

---

### `send_email`

Sends transactional email through an [SMTP connector](./connectors.md#smtp).
Transport, credentials, TLS mode, and the default sender live on the
connector; the message lives here. **No automatic retries**: a timeout after
the message body is transmitted is indistinguishable from an accepted
message, and SMTP has no idempotency key — a retry would be a duplicate
email. The circuit breaker still applies.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the SMTP connector |
| `to` | string \| array | yes | — | Recipient(s); each is `addr@example.com` or `Name <addr@example.com>` |
| `cc` / `bcc` | string \| array | no | — | Same address forms |
| `subject` | string | yes | — | UTF-8 subject |
| `text` | string | one of `text`/`html` | — | Plain-text body |
| `html` | string | one of `text`/`html` | — | HTML body; with `text` too, the message is `multipart/alternative` |
| `from` | string | no | connector `from` | Honored only when the connector sets `allow_from_override` |
| `reply_to` | string | no | — | Reply-To address |
| `headers` | object | no | — | Extra headers (string values). Structured names (`From`, `To`, `Subject`, `Content-Type`, …) are rejected — this is for `List-Unsubscribe`, `Auto-Submitted`, correlation IDs |
| `output` | string \| JSONLogic | no | `"data"` | Where `{ "message_id", "response" }` is stored — the generated Message-ID (for correlation/threading) and the server's acceptance line |

A wrong address fails at workflow create when static (naming the field and
index) and at send time when resolved from the message. Rejected recipients
fail the task with the server's reply — no partial-success reporting.

Every field above except `connector`, `headers` and `output` is JSONLogic, so a
body or a subject can be composed in the task rather than in a `map` task ahead
of it:

```json
{
  "name": "send_email",
  "input": {
    "connector": "mailer",
    "to": { "var": "data.email" },
    "subject": { "cat": ["Order ", { "var": "data.order_id" }, " confirmed"] },
    "text": { "cat": ["Your OTP is ", { "var": "temp_data.otp" }] },
    "output": "temp_data.mail_result"
  }
}
```

### `storage_presign`

Computes a time-limited presigned URL for one object in a
[storage connector](./connectors.md#storage)'s bucket — **pure local
computation**: no bytes move through the runtime, and the client talks to the
object store directly. GET presigns downloads; PUT presigns direct client
uploads. Each method answers to its own connector gate (`presign_get` /
`presign_put`).

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the storage connector |
| `method` | string | no | `"GET"` | `GET` \| `PUT` — an open value set |
| `key` | string | yes | — | Object key within the connector's bucket |
| `expires_in` | number \| string | yes | — | URL lifetime: integer seconds or `"<n>s\|m\|h\|d"`; at most 7 days (S3's own ceiling) |
| `response_content_type` | string | no | — | GET only: forces the answered Content-Type; signed, so the client cannot alter it |
| `response_content_disposition` | string | no | — | GET only: forces Content-Disposition — the download-filename knob; signed |
| `content_type` | string | no | — | PUT only: the Content-Type the uploader must send — a signed header, so any other type is refused by the store |
| `output` | string \| JSONLogic | no | `"data"` | Where the presigned URL (string) is stored |

```json
{
  "name": "storage_presign",
  "input": {
    "connector": "media",
    "key": { "var": "temp_data.object_key" },
    "expires_in": "7d",
    "output": "temp_data.play_url"
  }
}
```

### `storage_head`

One SigV4-signed HEAD for object metadata. A missing object is **data, not
failure**: 404 answers `{ "exists": false }` — "is it there yet?" is the
question this function exists to ask, while auth failures, timeouts, and
other statuses fail the task. One attempt inside the circuit breaker; a
workflow can loop if it wants polling.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the storage connector |
| `key` | string | yes | — | Object key within the connector's bucket |
| `output` | string \| JSONLogic | no | `"data"` | Where `{ exists, size, etag, last_modified, content_type }` is stored |

```json
{
  "name": "storage_head",
  "input": {
    "connector": "media",
    "key": { "var": "temp_data.object_key" },
    "output": "temp_data.object_meta"
  }
}
```

## Composition functions

### `channel_call`

Invokes another channel's workflow **in-process**: no network hop. The called
channel keeps its own versioning and governance. Cycle detection and a max call
depth prevent runaway recursion.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `channel` | string \| JSONLogic | yes | — | Target channel. Accepts the pre-1.0 name `channel_logic` |
| `data` | any \| JSONLogic | no | request payload | Payload passed to the target channel. Accepts the pre-1.0 name `data_logic` |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the called channel's response is stored. Accepts the pre-1.0 name `response_path` |
| `timeout_ms` | number | no | from config | Per-call timeout in milliseconds |

`channel` and `data` are each one field, not the `channel`/`channel_logic` and
`data`/`data_logic` pairs they were before 1.5. A literal is JSONLogic for
itself, so the static spelling is unchanged and still folds once when the engine
is built; an expression in the same field is what makes the target or the
payload depend on the message. The old names remain accepted as aliases —
supplying both spellings of one field is an error, not a precedence rule.

```json
{
  "name": "channel_call",
  "input": {
    "channel": "customer-lookup",
    "data": { "var": "data.order.customer_id" },
    "output": "data.customer"
  }
}
```

A computed `channel` routes one task to a channel the message names. The
dependency endpoint reports `has_dynamic_channel_calls` for a workflow that
contains one, because the static list of targets cannot be complete:

```json
{
  "name": "channel_call",
  "input": {
    "channel": { "cat": ["notify-", { "var": "data.region" }] },
    "output": "data.notified"
  }
}
```

---

## Utility functions

### `crypto`

Digests, HMACs (compute **and** verify), and password hashing as one operation
envelope — self-contained: no connector, no egress, so dry-run and
`orion-server test` execute it for real. The `op` field selects the operation;
each op takes the subset of fields below, checked at workflow create/validate
(an op × algorithm pair outside the capability table, a missing `key`, or an
out-of-bounds cost parameter is an authoring-time error).

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `op` | string | yes | — | `hash` \| `hmac` \| `hmac_verify` \| `password_hash` \| `password_verify` |
| `algorithm` | string | no | per op | `hash`: `sha256` (default), `sha512`, plus `sha1`/`md5` for legacy interop. `hmac`/`hmac_verify`: `sha256` (default), `sha512`, `sha1`. `password_hash`: `argon2id` (default), `bcrypt`. `password_verify` auto-detects from the stored hash |
| `data` | any | for hash/hmac ops | — | Bytes to digest. A string is UTF-8 (see `input_encoding`); any other JSON value is hashed as its compact serialization, key order preserved |
| `input_encoding` | string | no | `"utf8"` | How a *string* `data` becomes bytes: `utf8`, `hex`, `base64` |
| `key` | string | for hmac ops | — | `{"secret": "name"}` reads the engine's [`[secrets]`](./configuration.md#vars-and-secrets) store; a string is a literal or a reference (`env://NAME`, `vault://…`). Never in traces or errors. Literals are fine for development; workflows are not encrypted at rest, so production wants one of the other two |
| `key_encoding` | string | no | `"utf8"` | How the resolved key becomes bytes: `utf8`, `hex`, `base64` — for APIs that issue binary signing keys |
| `signature` | string | for `hmac_verify` | — | The presented MAC; hex, base64, or base64url, auto-detected. Compared in constant time — never verify a MAC with `==` |
| `password` | string | for password ops | — | The submitted password |
| `hash` | string | for `password_verify` | — | The stored hash; scheme auto-detected from its `$argon2*$`/`$2*$` prefix, which is also the rehash-on-login discriminator |
| `encoding` | string | no | `"hex"` | Output encoding for `hash`/`hmac`: `hex`, `base64`, `base64url` (unpadded, the JWS form) |
| `params` | object | no | safe defaults | `password_hash` cost tuning, bounded: argon2id `memory_kib` (8192–131072, default 19456), `iterations` (1–10, default 2), `parallelism` (1–4, default 1); bcrypt `cost` (10–14, default 12) |
| `output` | string \| JSONLogic | no | `"data"` | Dotted result path. String for `hash`/`hmac`/`password_hash`; boolean for `hmac_verify`/`password_verify` |

Wrong password or wrong signature → `false`; a *malformed* stored hash or an
undecodable signature is a task error, so data corruption is never mistaken
for a bad credential.

```json
{
  "name": "crypto",
  "input": {
    "op": "hmac",
    "algorithm": "sha256",
    "key": "env://ZOOM_WEBHOOK_SECRET",
    "data": { "var": "data.payload.plainToken" },
    "encoding": "hex",
    "output": "temp_data.encrypted_token"
  }
}
```

```json
{
  "name": "crypto",
  "input": {
    "op": "password_verify",
    "password": { "var": "data.password" },
    "hash": { "var": "temp_data.user.password_hash" },
    "output": "temp_data.password_ok"
  }
}
```

---

### `jwt_sign`

Mints a compact JWS — login access/refresh pairs, RFC 7523 client assertions.
Self-contained like `crypto`: no connector, real execution in dry-run, and the
signing key (a literal or an `env://`/`vault://` reference) lives only inside
the call. `iat` is stamped automatically **unless the claims object supplies
one**; a token must expire deliberately — `expires_in`, or an explicit `exp`
claim.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `algorithm` | string | yes | — | `HS256/384/512`, `RS256/384/512`, `PS256/384/512`, `ES256/384`, `EdDSA` |
| `key` | string | yes | — | HS secret or RS/ES/Ed **private**-key PEM; `{"secret": "name"}`, a reference, or a literal |
| `key_encoding` | string | no | `"utf8"` | How an HS secret becomes bytes: `utf8`, `base64`, `hex` |
| `claims` | object | no | `{}` | Claim values fold `{"var": …}` nodes and nothing else — compose a computed claim in a `map` task first. (`audience`, `not_before` and `expires_in` below are full JSONLogic) |
| `expires_in` | number \| string | conditional | — | Lifetime (seconds or `"<n>s\|m\|h\|d"`) → `exp`. Required unless `claims.exp` is explicit |
| `claims.iat` | number | no | now | Issue time. Supplying one wins — there is no `issued_at` field, so nothing more specific can beat it. Back- or forward-dating is what revocation-pivot schemes need, and it is the only way a minted token can be asserted byte-for-byte offline |
| `issuer` / `audience` / `not_before` | — | no | — | Conveniences for `iss` / `aud` / `nbf` (offset from now); explicit fields win over same-named claims entries |
| `kid` | string | no | — | Key id stamped into the header, for rotation-aware verifiers |
| `output` | string \| JSONLogic | no | `"data"` | Where the token (string) is stored |

`iat` and `exp` supplied through `claims` must be **numbers**: seconds since
the Unix epoch (NumericDate, RFC 7519 §2). A string date is refused at sign
time rather than minting a token every verifier rejects later. Nothing in Orion
makes a trust decision on `iat`: neither `jwt_verify` nor the channel `jwt`
mode inspects it, so a back-dated token verifies normally.

### `jwt_verify`

Verifies a JWS mid-workflow — provider id_tokens for social login, refresh
tokens, partner assertions — against static keys and/or a JWKS (the same
process-wide cache as the channel's `jwt` mode: single-flight refresh,
stale-serve, `kid`-rotation refetch). Rejections are typed task errors
(`continue_on_error` branches on them); the reason is named, the token never
is.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `token` | string | yes | — | The compact JWS |
| `algorithms` | array | yes | — | Mandatory non-empty allowlist — `alg: none` and downgrades are unrepresentable |
| `keys` | array | one of | — | `[{algorithm, key, kid?, key_encoding?}]` — public halves for the asymmetric families. Each `key` takes `{"secret": "name"}`, a reference, or a literal |
| `jwks_url` | string | one of | — | HTTPS JWKS URL |
| `issuer` / `audience` | string \| array | no | — | Accepted `iss`/`aud` values; `{"secret": "name"}` and `env://` references resolve (OAuth client ids) |
| `leeway_secs` | number | no | `30` | Clock-skew allowance, capped at 300 |
| `require_exp` | boolean | no | `true` | RFC 8725: tokens must expire unless deliberately opted out |
| `output` | string \| JSONLogic | no | `"data"` | Where the verified claims object is stored |

```json
{
  "name": "jwt_verify",
  "input": {
    "token": { "var": "data.id_token" },
    "algorithms": ["RS256"],
    "jwks_url": "https://provider.example.com/certs",
    "issuer": "https://accounts.provider.example.com",
    "audience": "env://OAUTH_CLIENT_ID",
    "output": "temp_data.verified_claims"
  }
}
```

## Inspecting schemas at runtime

**Since:** Orion 1.2 for the complete function catalog.

`GET /api/v1/admin/functions` returns the live input schema for the connector,
composition, and utility functions (the data functions are provided by
dataflow-rs and are not cataloged there). The [Orion agent skill](../ai/skills.md) points an assistant at
the same schemas to AI assistants so generated workflows use correct field names.

**Plugin functions.** The functions of every active
[plugin](./plugins.md) appear in the same catalogue with `source: "plugin"`
and a `plugin` block naming the plugin id, version and component digest.
Their field tables come from the plugin's manifest rather than this page,
which documents only the functions built into the binary; the vocabulary is
the same (`kind`, `required`, `resolvable`, `template_at`), and workflow
validation reads it the same way.

## Related

- [Workflow Reference](./workflows.md): the workflow schema, the data context,
  and how task conditions select which tasks run.
- [Portable Data Dialect](./data-dialect.md): the full envelope, operator
  vocabulary, and backend parity rules behind `data_query`/`data_write`.
- [Connectors](./connectors.md): per-type connector fields, retries, and
  circuit-breaker behaviour.
- [Admin API](./admin-api.md): creating, validating, and activating the
  workflows these functions run in.
