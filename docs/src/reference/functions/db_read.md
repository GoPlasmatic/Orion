<!-- description: The db_read task function: run a raw SELECT with bound parameters against a SQL connector, with the column decoding and parameter typing rules. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `db_read`

The raw-SQL escape hatch for reads — anything outside the portable dialect's
vocabulary (joins, aggregations, CTEs, database-specific SQL). Runs a `SELECT`
against a SQL connector and writes the result rows as a JSON array. Use
placeholders bound from `params` — `?` for SQLite/MySQL, `$1`, `$2`,
… for PostgreSQL.

## Synopsis

```json
{
  "name": "db_read",
  "input": {
    "connector": "primary-db",
    "query": "SELECT id, name, tier FROM customers WHERE id = ?",
    "params": [
      {
        "var": "data.order.customer_id"
      }
    ],
    "numeric_as": "number",
    "binary_as": "auto",
    "output": "data.customer"
  }
}
```

## Description

`db_read` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `read`. See [Retry safety](./retry-safety.md) for what the answer costs.

**Reads only.** The statement must open with `SELECT`, `WITH`, `VALUES` or `TABLE`. A `WITH` carrying a data-modifying CTE (`WITH gone AS (DELETE … RETURNING …) …`) is refused. `EXPLAIN` is not admitted either, because `EXPLAIN ANALYZE DELETE …` executes the delete. A statement that writes belongs in [`db_write`](./db_write.md), which has its own `raw_write` [operation gate](../data-dialect.md#connector-operation-gates). That gate is what makes a connector delete-proof, and it only holds because `db_read` cannot write.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the SQL connector |
| `query` | string | yes | — | `SELECT` statement with bind placeholders |
| `params` | array | no | — | Values bound to the placeholders, in order. Each element folds `{"var": …}` and nothing else — an operator written inline binds as a literal object, which `lint` reports as `logic.unresolvable`; compute it in a `map` task first |
| `numeric_as` | string | no | `"number"` | How a `numeric`/`decimal` column is rendered: `number` or `string` — see [Decimal columns](#decimal-columns) |
| `binary_as` | string | no | `"auto"` | How a binary column is rendered: `auto`, `hex`, `base64` or `text` — see [Binary columns](#binary-columns). |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the row array is written |

### Parameter types

**Parameters are the other direction, and the query decides their type.** On
PostgreSQL a `params` entry is bound to the type the *server* declares for that
placeholder, not to the shape of the JSON value. `WHERE id = $1` against a `uuid` column binds a `uuid`. `LIMIT ($1)::int` binds an integer whether the caller sent `5` or `"5"`. The cast forms still work, and remain the way to write a type outside the list below. They are no longer the only way.

This matters beyond convenience. Until 1.7 the type came from the value, and PostgreSQL fixes a prepared statement's parameter types on first use. The first call through a query therefore decided them for every later one. A read bound with a number and then called with `?limit=5` failed, because a query-string value is always a string. Where the byte lengths happened to line up it did something worse. An eight-character string bound where `int8` had been declared came back as a plausible, wrong number.

The types a parameter can be bound to:

| Declared type | Accepted JSON |
|---|---|
| `bool` | boolean, or `"true"` / `"false"` |
| `int2` `int4` `int8` | number, or a numeric string; out of range is a `400` |
| `float4` `float8` | number, or a numeric string |
| `numeric` | string (exact) or number — the string is the lossless one, as with [`numeric_as`](#decimal-columns) |
| `text` `varchar` `char(n)` `name` `citext` | any scalar |
| `uuid` | the hyphenated string form |
| `json` `jsonb` | a document, or JSON text — text is parsed, matching PostgreSQL's own `text` → `json` cast |
| `timestamptz` | RFC 3339 **with an offset**: `"2026-09-02T05:00:00Z"` |
| `timestamp` | `"2026-09-02 05:00:00"`, the form a read returns, or RFC 3339 |
| `date` / `time` | `"2026-09-02"` / `"05:00:00"` |
| an enum | the label |
| an array of any of these | a JSON array — which is what makes `WHERE id = ANY($1)` work |

A value that does not convert is a **`400` naming the placeholder and the type the query declared for it**. The value itself is never named, because it may be private.

A `timestamptz` needs its offset stated. Reading one without would mean choosing a timezone on your behalf, and a silently shifted timestamp is the kind of wrong answer nobody notices. Cast the placeholder (`($1)::timestamptz`) if you want the server to interpret it.

**A type outside that list keeps the old behaviour**: `bytea`, `inet`,
`interval`, composites and ranges are bound by the value's JSON shape, as
before. They are not refused, because an author can define a cast Orion cannot know about. Nothing is inferred for them either, so write the cast (`decode($1, 'base64')`, `($1)::inet`) as you would have before 1.7.

SQLite has no static parameter types and MySQL re-sends its types on every
execute, so neither was ever affected and neither changes.

## Returns

### Column types

Rows are decoded on the connector's real driver, so a `SELECT *` over an ordinary schema works. `uuid`, `json`/`jsonb`, `numeric`, the date/time family, arrays, enums and domains all have JSON forms. Dates and times arrive as RFC 3339 / ISO 8601 strings. A `json`/`jsonb` column comes back as the document itself, so `parse_json` is not needed after a read.

`char(n)` and PostgreSQL's `citext` decode as strings, scalar and array alike.

A type with no JSON form here is a **`400` naming the column and its SQL type**, not a `500`. That covers `inet`, `interval`, a composite, a range, and PostgreSQL's internal one-byte `"char"`, which is not `char(n)`. The remedy is in the message: cast it in the query (`SELECT extra::text`) and use
[`parse_json`](./parse_json.md) if it holds a document.

### Decimal columns

`numeric` (PostgreSQL) and `decimal` (MySQL) are arbitrary precision, and JSON
has no equivalent. `numeric_as` decides which way the mismatch resolves:

| Value | Result |
|---|---|
| `"number"` (default) | A JSON number — computable in JSONLogic, and **rounded** beyond 2^53 or on most decimal fractions |
| `"string"` | The exact decimal as a string; arithmetic needs an explicit cast in the workflow |

The default is the convenient one, not the safe one. For a money column use `"string"`: a silently rounded total is a correctness bug the caller cannot see. The cast the string forces is the point, because it makes the loss a decision rather than an accident. `bigint` is unaffected either way, because a
64-bit integer is exact in JSON.

SQLite has no static column types. A `NUMERIC` column stores whichever storage class the value fits, so the setting has nothing to act on there and is ignored.

### Binary columns

A `bytea` (PostgreSQL), `blob` (SQLite) or `binary`/`varbinary`/`blob` (MySQL)
column has no JSON form either. `binary_as` decides which way that resolves:

| Value | Result |
|---|---|
| `"auto"` (default) | The bytes as text when they are valid UTF-8, lowercase hex when they are not |
| `"hex"` | Lowercase hex, whatever the bytes are |
| `"base64"` | Standard padded base64, whatever the bytes are |
| `"text"` | The bytes as UTF-8 text, or a **`400` naming the column** when they are not |

`auto` is the default for two reasons. MySQL reports `TEXT` and `JSON` columns as `BLOB`, so text is the right answer far more often than not. And it is what every task written before this setting existed already reads.

It is also the one mode whose **result shape is decided by the data**. Two rows of the same column can come back as text and as hex, with nothing in the result telling them apart. A workflow that hex-decodes the column then breaks the first time a value happens to be valid UTF-8. For a column that is binary, name an encoding. This is the same trade `numeric_as` makes, and it resolves
the same way: the default is the convenient one, not the safe one.

Unlike `numeric_as`, this setting **does** apply on SQLite — `BLOB` is a
storage class, so it survives the round trip a declared type does not.

### Boolean columns

A MySQL `BOOLEAN` / `BOOL` / `TINYINT(1)` column reads back as a JSON
**boolean**, the same as a PostgreSQL `bool`. MySQL has no boolean type; all three spellings are `TINYINT(1)`. The width-1 declaration is the convention every framework writes and the only one MySQL 8 still preserves. It is therefore treated as the boolean it is meant to be. A `TINYINT` *without* the width is a different column and stays a number. If you are storing a small integer in a `TINYINT(1)`, select it as `flags + 0` to get one back.

SQLite is the exception, and it cannot be otherwise. A value there carries a storage class rather than a declared type. A column declared `BOOLEAN` is therefore indistinguishable from an integer by the time the row is read, and comes back as `1` / `0`. This is the same reason `numeric_as` has nothing to act on there.

## Examples

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

## Caveats

### Paging a raw read

`db_read` has **no pagination surface**: no `limit`, `skip`, `after` or `cursor` field. Orion does not parse the statement, so it cannot inject a bound into it. A field this table does not declare is *silently ignored*, so a `"limit": 10` written beside `query` does nothing at all.

What still applies is `query.max_limit`, as a tripwire rather than a page. The rows are counted as they stream, and a result exceeding the cap fails the task
with a `400` rather than coming back truncated. Paging is part of the SQL you
write.

Raw SQL is also where a keyset cursor fits best, because the statement can branch on an absent cursor. The [portable dialect's `after`](../data-dialect.md#paging) handles that for you; a hand-written filter cannot:

```json
{
  "name": "db_read",
  "input": {
    "connector": "arena-db",
    "query": "SELECT m.model_id, s.conservative FROM ratings s JOIN models m ON m.id = s.model_id WHERE $1::float8 IS NULL OR s.conservative < $1 OR (s.conservative = $1 AND m.model_id > $2) ORDER BY s.conservative DESC, m.model_id ASC LIMIT 20",
    "params": [
      { "var": "data.req.cursor.conservative" },
      { "var": ["data.req.cursor.model_id", ""] }
    ],
    "output": "data.page"
  }
}
```

Two things to get right. The comparison **mirrors the sort key by key**: a `desc` key compares `<`, an `asc` key compares `>`. Each later key is reached only under equality of every key before it. Spell it out as clauses rather than a row-value `(a, b) < ($1, $2)`. The row form needs every key to share a direction, which a leaderboard does not. MySQL also does not turn it into an index range scan. And `params` is **positional**. On SQLite and MySQL, which spell placeholders `?` rather than `$n`, a cursor value used twice must be passed twice.

If a sort key is nullable, add the null arm the dialect adds for you. Nulls sort last on `desc`, and `s.conservative < $1` is unknown for them. Without `OR s.conservative IS NULL` the final page drops every null-valued row.

## Compatibility

**Since:** Orion 1.6, rows decode on the connector's real driver. Before that, a driver-agnostic layer decoded nine PostgreSQL types and everything else failed the task with a `500`. The failure came only when a row existed. A query passed every test against an empty table and failed the first time production had data.

**Since:** Orion 1.7, a parameter is bound to the type the query declares for its placeholder. Before that, the type came from the JSON value, and PostgreSQL fixed it on the first call.

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Portable data dialect › Connector operation gates](../data-dialect.md#connector-operation-gates): the gates that bound raw SQL.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
