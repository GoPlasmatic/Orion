# Expressions (JSONLogic)

JSONLogic decides whether a workflow matches, whether a task runs, and what a
`map` mapping writes. Channels use the same language for `validation_logic` and
rate-limit `key_logic`. Every expression is compiled once, at engine build time.

The tables below are the **complete** set Orion compiles — not the whole
JSONLogic spec. An integration test asserts them against the live engine, so
they cannot drift from what runs.

## Two failure modes to internalize first

**A misspelled operator inside a `map` mapping is not an error.** JSONLogic
cannot distinguish `{"upper": [...]}` used as an operator from a data object
with one key. Mappings render through a templating path: inner expressions
resolve and the unknown object is written through as a **literal**. So
`{"uppr": [...]}` puts `{"uppr": "widget"}` at the target path — no error, no
failed task, `200` to the caller. When a mapping yields an object where you
expected a scalar, check the operator name here first.

Conditions are different: they compile and evaluate strictly, so the same typo
is a hard error.

**Connector payload fields fold `{"var": …}` and nothing else** — see the last
section. That one silently stores expression objects in your database.

## Core

| Operator | Example | Meaning |
|---|---|---|
| `var` / `val` | `{ "var": "data.order.total" }` | Read a value by dotted path |
| `val` (path chain) | `{ "val": ["data", "items", { "val": ["temp_data", "i"] }] }` | Read with **computed** segments — the only way to index an array by a value |
| `==` / `!=` | `{ "==": [{ "var": "data.type" }, "order"] }` | Loose equality |
| `===` / `!==` | `{ "===": [{ "var": "data.qty" }, 1] }` | Strict equality |
| `>` `>=` `<` `<=` | `{ ">": [{ "var": "data.total" }, 10000] }` | Comparison |
| `and` / `or` / `!` | `{ "and": [a, b] }` | Boolean logic |
| `!!` | `{ "!!": [{ "var": "data.order.id" }] }` | Truthiness ("is present") |
| `if` / `?:` | `{ "if": [cond, then, else] }` | Conditional value |
| `+` `-` `*` `/` `%` | `{ "*": [{ "var": "data.qty" }, 1.1] }` | Arithmetic |
| `max` / `min` | `{ "max": [1, 2, 3] }` | Largest / smallest |
| `cat` | `{ "cat": ["Order #", { "var": "data.id" }] }` | String concatenation |
| `substr` | `{ "substr": [{ "var": "data.code" }, 0, 3] }` | Substring |
| `in` | `{ "in": [{ "var": "data.tier" }, ["vip", "premium"]] }` | Membership (array or substring) |
| `merge` | `{ "merge": [[1, 2], [3]] }` | Flatten arrays into one |
| `map` / `filter` / `reduce` | `{ "map": [{ "var": "data.items" }, { "var": "price" }] }` | Array transforms |
| `all` / `some` / `none` | `{ "some": [{ "var": "data.items" }, { ">": [{ "var": "qty" }, 0] }] }` | Array predicates |
| `missing` / `missing_some` | `{ "missing": ["data.order.id"] }` | Report absent paths |

## Dates

| Operator | Example | Meaning |
|---|---|---|
| `now` | `{ "now": [] }` | Current instant as RFC 3339 |
| `datetime` | `{ "datetime": ["2026-07-31T00:00:00Z"] }` | Build a datetime from RFC 3339 |
| `parse_date` | `{ "parse_date": [{ "var": "data.when" }, "yyyy-MM-dd"] }` | Parse with an explicit format |
| `format_date` | `{ "format_date": [{ "now": [] }, "yyyy-MM-dd", "Asia/Kolkata"] }` | Format, optionally in an IANA zone |
| `date_diff` | `{ "date_diff": [a, b, "days"] }` | Whole units between two datetimes |
| `timestamp` | `{ "timestamp": ["1d"] }` | Build a **duration** from a duration string |

Format vocabulary: `yyyy`, `MM`, `dd`, `HH`, `mm`, `ss` (raw `%Y` strftime also
works; prefer the documented form). `format_date` and `parse_date` take an
optional trailing IANA zone — `format_date` renders the instant as that zone's
wall clock (DST-correct), `parse_date` reads a naive input as wall-clock time
*in* that zone. A misspelled literal zone fails at compile, not per request.
**Never add fixed offsets by hand**: `{"+": [ts, {"timestamp": "5h30m"}]}` is
wrong in any zone with DST.

## Strings, arrays, math, objects

| Operator | Example | Meaning |
|---|---|---|
| `length` | `{ "length": [{ "var": "data.items" }] }` | Length of a string **or** array |
| `upper` / `lower` | `{ "upper": [{ "var": "data.code" }] }` | Change case |
| `trim` | `{ "trim": [{ "var": "data.name" }] }` | Strip surrounding whitespace |
| `split` | `{ "split": [{ "var": "data.csv" }, ","] }` | Split into an array |
| `starts_with` / `ends_with` | `{ "starts_with": [{ "var": "data.sku" }, "AB-"] }` | Prefix / suffix test |
| `sort` | `{ "sort": [{ "var": "data.scores" }] }` | Sort ascending |
| `slice` | `{ "slice": [{ "var": "data.items" }, 0, 2] }` | Sub-array by start/end |
| `abs` | `{ "abs": [{ "var": "data.delta" }] }` | Absolute value |
| `ceil` / `floor` | `{ "ceil": [{ "var": "data.price" }] }` | Round up / down |
| `group_by` | `{ "group_by": [{ "var": "data.rows" }, { "var": "region" }] }` | Collapse on a computed key → array of `{key, items}`, insertion-ordered |
| `distinct` | `{ "distinct": [{ "var": "data.tags" }] }` | Deduplicate, first occurrence wins |
| `keys` / `values` | `{ "keys": [{ "var": "data.totals" }] }` | An object's keys / values as arrays |
| `entries` | `{ "entries": [{ "var": "data.totals" }] }` | `[{key, value}]` rows — iterate any object with `map` |

`group_by`'s second argument is evaluated **per element**: `var` paths are
element-relative and `{"var": ""}` is the element itself. The result is an
*array* of groups, so it composes with `map` / `filter` / `sort`.

## Control

| Operator | Example | Meaning |
|---|---|---|
| `??` | `{ "??": [{ "var": "data.nickname" }, "anonymous"] }` | Coalesce — first non-null |
| `type` | `{ "type": [{ "var": "data.price" }] }` | Type name as a string |
| `exists` | `{ "exists": ["data", "order", "id"] }` | Path presence — segments, not a dotted path |
| `switch` / `match` | see below | Multi-way branch |
| `try` / `throw` | `{ "try": [expr, fallback] }` | Catch / raise an evaluation error |

## Orion's own operators

Registered by Orion rather than gated by a feature, so they work on every
expression surface. A string encodes as its UTF-8 bytes; any other value as its
compact-JSON text; `null` is an error. Decoders are strict.

| Operator | Example | Meaning |
|---|---|---|
| `base64_encode` / `base64_decode` | `{ "base64_encode": [{ "var": "data.raw" }] }` | Standard base64, padded (decode tolerates unpadded) |
| `base64url_encode` / `base64url_decode` | `{ "base64url_encode": [{ "var": "data.claims" }] }` | URL-safe, unpadded — the JWS form |
| `hex_encode` / `hex_decode` | `{ "hex_encode": [{ "var": "data.token" }] }` | Lowercase hex |
| `url_encode` / `url_decode` | `{ "url_encode": [{ "var": "data.email" }] }` | RFC 3986 percent-encoding |
| `join` | `{ "join": [{ "var": "data.tags" }, ", "] }` | Join array elements with a separator |
| `random` | `{ "random": ["digits", 6] }` | CSPRNG generation — see below |

`random` kinds: `["uuid"]` / `["uuid", "v7"]`, `["digits", n]` (leading zeros
kept, n ≤ 64 — the OTP shape), `["int", min, max]` (inclusive), `["string",
len, alphabet?]` (len ≤ 1024; `alphanumeric` default, or `hex` / `numeric` /
`url-safe`, or a custom string of 2–256 distinct chars), `["bytes", n,
encoding?]`. Never constant-folded; no seed parameter. Values are drawn live in
dry-run and `orion-server test` too, so do not assert exact outputs in cases.

**`url_encode` is RFC 3986, not form-encoding.** A space becomes `%20`, never
`+`; a literal `+` becomes `%2B`. It is stricter than JavaScript's
`encodeURIComponent`, which leaves `!'()*` literal. `url_decode` correspondingly
does **not** treat `+` as a space.

Use it whenever a value is interpolated into an outbound query string — which
today means building `path` / `path_logic` with `cat`, since `http_call` has no
`query` field:

```json
{ "cat": ["/search?q=", { "url_encode": [{ "var": "data.term" }] }] }
```

Without it, a value containing `&` or `#` silently restructures or truncates
the query.

## `secret` — the one operator that reads outside the message

```json
{ "secret": "partner_hmac" }
```

Its argument is a **name**, not a path: it reads the `[secrets]` section the
operator declared in the config file. The value is held by the engine rather
than by the message, so it cannot reach a trace, a `map` mapping clone or a
response body — there is nothing to strip.

That is enforced, not advised. A workflow reading a secret anywhere the engine
records the result — a `map` mapping, a `log` message or field — is refused
when the engine is built, and so is one naming a secret the instance does not
declare. Both surface as a quarantined channel, not a missing value.

So read a secret in a **task condition** or in a function field that needs key
material (`crypto.key`, `jwt_sign.key`, `jwt_verify`'s `keys`), and nowhere
else. A value *derived* from a secret belongs inside a function, never in a
mapping.

Two limits: `{"secret": …}` does not compile in a channel's `validation_logic`
or `key_logic` (those run on an engine with no store, so the channel is
quarantined), and a deployment value that is not key material belongs in
`[vars]` instead — read as `{"var": "metadata.vars.name"}`, stamped into every
message, and recorded in traces on purpose.

## Sharp edges

Most of these fail *quietly*, with a plausible answer rather than an error.

- **`exists` takes path segments, not a dotted path.** It does not split on `.`
  and evaluates arguments as literals. `{"exists": ["data.order.id"]}` looks for
  one top-level key literally named `data.order.id` and returns `false`. Spell
  it out: `{"exists": ["data", "order", "id"]}`.

- **`switch` takes an array of `[case, result]` pairs**, not a flat alternating
  list. A flat list is not rejected — the second element is read as the case
  array, fails to match, and the third is returned as the default. You silently
  get one fixed branch for every input.

  ```json
  { "switch": [ { "var": "data.tier" }, [ ["gold", 20], ["silver", 10] ], 0 ] }
  ```

- **`date_diff` units are plural** — `"days"`, `"hours"`, `"minutes"`,
  `"seconds"`. An unrecognized unit, including the singular `"day"`, returns
  `0`, which reads exactly like "the dates are the same".

- **`timestamp` parses a duration, not a datetime.** `"1d"` → `"1d:0h:0m:0s"`.
  Passing a datetime is an `Invalid duration format` error.

- **`now` is evaluated per call**, so two `now` mappings can land on different
  instants. Compute it once into a field and read that field.

- **`{"cat": [arr, "|"]}` does not join with `|`** — `cat` flattens the array
  then appends the separator once, at the end. Use `join`, which requires both
  arguments (a missing separator is an error, and a non-array first argument is
  too). `{"cat": [arr]}` is the fastest spelling of `join(arr, "")`.

- **`replace(s, from, to)` is `{"join": [{"split": [s, from]}, to]}`** — literal
  substring replacement, not a regex. Splitting on an empty delimiter explodes
  into characters, so guard against that.

## Connector payloads fold `{"var": …}` and nothing else

Everything above is about places JSONLogic is **evaluated**. A connector task's
payload fields are not one of those places.

A field a handler marks *resolvable* — `mongo_write`'s `document`, `documents`,
`update`, `filter`, `array_filters`; `mongo_read`'s `filter`, `projection`,
`sort`; `cache_write`'s `value`; `jwt_sign`'s `claims`; `send_email`'s `subject`
and `html`; the `params` of the SQL and dialect functions — folds
`{"var": "some.path"}` nodes at any depth and treats **every other node as a
literal**.

```json
{ "op": "insert_one",
  "document": {
    "userId":    { "var": "data.user_id" },
    "expiresAt": { "cat": ["2026-", { "var": "data.month" }] }
  } }
```

`userId` gets the value. `expiresAt` gets the *object* `{"cat": [...]}`, stored
verbatim — and in a `filter`, a node like that matches nothing. No error at
write time. Note `{"val": …}` is folded no more than `cat` is: only `var` is.

Compute it in a `map` task first, then reference the result:

```json
{ "path": "temp_data.expires_at", "logic": { "cat": ["2026-", { "var": "data.month" }] } }
```
```json
{ "expiresAt": { "var": "temp_data.expires_at" } }
```

`orion-server lint` warns when it finds one of these, and `orion-server test`
can assert on the resolved payload with `expect_calls`.
