# Expression Language (JSONLogic)

[JSONLogic](https://jsonlogic.com) is Orion's expression language. Expressions
decide whether a workflow matches, whether a task runs, and what a `map`
mapping writes — see the [Workflow Reference](./workflows.md). Channels use the
same language for `validation_logic` and rate-limit `key_logic` — see
[Channel Configuration](./channel-config.md). Every expression is evaluated by
[datalogic-rs](https://github.com/GoPlasmatic/datalogic-rs) and compiled once,
at engine build time.

> [!WARNING]
> **A misspelled operator inside a `map` mapping is not an error.** JSONLogic
> cannot distinguish `{ "upper": [...] }` used as an operator from a data
> object that happens to have one key. Mappings render through a templating
> path: inner expressions resolve, and the unknown object is written through as
> a *literal*. So `{ "uppr": [...] }` puts `{"uppr": "widget"}` at the target
> path — no error, no failed task, `200` to the caller. When a mapping yields a
> JSON object where you expected a scalar, check the operator name against the
> tables below first.
>
> Conditions behave differently. They are compiled and evaluated strictly, so
> the same misspelling there is a hard error rather than a silent literal.

> [!NOTE]
> What an expression can read depends on where it appears. Workflow and task
> expressions read the [data context](./workflows.md); `validation_logic` and
> `key_logic` each see a smaller, dedicated context —
> [Channel Configuration](./channel-config.md) documents both.

## Available operators

The tables below are the **complete** set Orion compiles. They are not the
whole JSONLogic spec — see [Feature boundary](#feature-boundary).
`crates/orion-server/tests/integration/jsonlogic_operators_test.rs` asserts
this table against the engine, so it cannot drift from what actually runs.

### Core

| Operator | Example | Meaning |
|----------|---------|---------|
| `var` / `val` | `{ "var": "data.order.total" }` | Read a value from the context (dotted path) |
| `val` (path chain) | `{ "val": ["data", "items", { "val": ["temp_data", "i"] }] }` | Read with **computed** segments — the only way to index an array by a value |
| `==` / `!=` | `{ "==": [{ "var": "data.type" }, "order"] }` | Loose equality |
| `===` / `!==` | `{ "===": [{ "var": "data.qty" }, 1] }` | Strict equality (no type coercion) |
| `>` `>=` `<` `<=` | `{ ">": [{ "var": "data.order.total" }, 10000] }` | Comparison |
| `and` / `or` / `!` | `{ "and": [a, b] }` | Boolean logic |
| `!!` | `{ "!!": [{ "var": "data.order.id" }] }` | Truthiness (e.g. "is present") |
| `if` / `?:` | `{ "if": [cond, then, else] }` | Conditional value |
| `+` `-` `*` `/` `%` | `{ "*": [{ "var": "data.qty" }, 1.1] }` | Arithmetic |
| `max` / `min` | `{ "max": [1, 2, 3] }` | Largest / smallest |
| `cat` | `{ "cat": ["Order #", { "var": "data.order.id" }] }` | String concatenation |
| `substr` | `{ "substr": [{ "var": "data.code" }, 0, 3] }` | Substring |
| `in` | `{ "in": [{ "var": "data.tier" }, ["vip", "premium"]] }` | Membership (array or substring) |
| `merge` | `{ "merge": [[1, 2], [3]] }` | Flatten arrays into one |
| `map` / `filter` / `reduce` | `{ "map": [{ "var": "data.items" }, { "var": "price" }] }` | Array transforms |
| `all` / `some` / `none` | `{ "some": [{ "var": "data.items" }, { ">": [{ "var": "qty" }, 0] }] }` | Array predicates |
| `missing` / `missing_some` | `{ "missing": ["data.order.id"] }` | Report absent paths |

### Dates (`datetime`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `now` | `{ "now": [] }` | Current instant, as an RFC 3339 string |
| `datetime` | `{ "datetime": ["2026-07-31T00:00:00Z"] }` | Build a datetime from an RFC 3339 string |
| `parse_date` | `{ "parse_date": [{ "var": "data.when" }, "yyyy-MM-dd"] }` | Parse with an explicit format |
| `format_date` | `{ "format_date": [{ "now": [] }, "yyyy-MM-dd", "Asia/Kolkata"] }` | Format a datetime, optionally in an IANA timezone |
| `date_diff` | `{ "date_diff": [a, b, "days"] }` | Whole units between two datetimes |
| `timestamp` | `{ "timestamp": ["1d"] }` | Build a **duration** from a duration string |

Format strings use the JSONLogic vocabulary: `yyyy`, `MM`, `dd`, `HH`, `mm`,
`ss`. Orion translates it to the underlying `strftime` spec, so raw
`%Y`-style patterns also work. Prefer the `yyyy` form; it is the documented
one.

`format_date` and `parse_date` accept an optional trailing IANA zone name
(`"Asia/Kolkata"`, `"America/New_York"`): `format_date` renders the instant as
that zone's wall-clock (DST-correct), and `parse_date` reads a naive input as
wall-clock time *in* that zone. A misspelled literal zone fails when the
expression is compiled, not per request. Do not add fixed offsets by hand —
`{ "+": [ts, { "timestamp": "5h30m" }] }` is wrong in any zone with DST.

### Strings (`ext-string`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `length` | `{ "length": [{ "var": "data.items" }] }` | Length of a string **or** array |
| `upper` / `lower` | `{ "upper": [{ "var": "data.code" }] }` | Change case |
| `trim` | `{ "trim": [{ "var": "data.name" }] }` | Strip surrounding whitespace |
| `split` | `{ "split": [{ "var": "data.csv" }, ","] }` | Split into an array |
| `starts_with` / `ends_with` | `{ "starts_with": [{ "var": "data.sku" }, "AB-"] }` | Prefix / suffix test |

### Arrays (`ext-array`) and math (`ext-math`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `sort` | `{ "sort": [{ "var": "data.scores" }] }` | Sort ascending |
| `slice` | `{ "slice": [{ "var": "data.items" }, 0, 2] }` | Sub-array by start/end |
| `abs` | `{ "abs": [{ "var": "data.delta" }] }` | Absolute value |
| `ceil` / `floor` | `{ "ceil": [{ "var": "data.price" }] }` | Round up / down |
| `group_by` | `{ "group_by": [{ "var": "data.meetings" }, { "format_date": [{ "var": "start" }, "dd MMM yyyy", "Asia/Kolkata" ] }] }` | Collapse on a computed key → array of `{key, items}` rows, insertion-ordered |
| `distinct` | `{ "distinct": [{ "var": "data.tags" }] }` | Deduplicate, first occurrence wins; add a key expression to dedupe by computed key |

`group_by`'s second argument is evaluated **per element** — inside it, `var`
paths are element-relative and `{ "var": "" }` is the element itself. The
result is an *array* of `{key, items}` groups (not an object), so it composes
directly with `map` / `filter` / `sort`.

### Objects (`ext-object`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `keys` / `values` | `{ "keys": [{ "var": "data.totals" }] }` | An object's keys / values as arrays |
| `entries` | `{ "entries": [{ "var": "data.totals" }] }` | `[{key, value}]` rows — iterate any object with `map` |

### Control (`ext-control`, `error-handling`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `??` | `{ "??": [{ "var": "data.nickname" }, "anonymous"] }` | Coalesce — first non-null |
| `type` | `{ "type": [{ "var": "data.price" }] }` | Type name as a string |
| `exists` | `{ "exists": ["data", "order", "id"] }` | Path presence — see [Sharp edges](#sharp-edges) |
| `switch` / `match` | see [Sharp edges](#sharp-edges) | Multi-way branch |
| `try` / `throw` | `{ "try": [expr, fallback] }` | Catch / raise an evaluation error |

### Encoding (Orion operators)

Registered by Orion itself rather than gated by a cargo feature — available on
every expression surface (conditions, `map` logic, `body_logic`, channel
guards). A string encodes as its UTF-8 bytes; any other value encodes as its
compact-JSON text (key order preserved); `null` is an error. Decoders are
strict: the input must be valid for the alphabet — base64 accepts padded and
unpadded input — and the decoded bytes must be valid UTF-8. Binary payloads
belong to the `crypto` function's `input_encoding` instead.

| Operator | Example | Meaning |
|----------|---------|---------|
| `base64_encode` / `base64_decode` | `{ "base64_encode": [{ "var": "data.raw" }] }` | Standard base64 (RFC 4648 §4, padded; decode tolerates unpadded) |
| `base64url_encode` / `base64url_decode` | `{ "base64url_encode": [{ "var": "data.claims" }] }` | URL-safe base64, unpadded — the JWS form |
| `hex_encode` / `hex_decode` | `{ "hex_encode": [{ "var": "data.token" }] }` | Lowercase hex |

### Randomness (Orion operators)

CSPRNG-backed value generation — never constant-folded, so every evaluation
draws fresh; there is deliberately no seed parameter, and range/alphabet
sampling is uniform (no modulo bias). The first argument selects the
generator kind; a new kind is a table row, never a new operator. Bad
arguments (unknown kind, inverted bounds, out-of-range length, an alphabet
with duplicate characters) are evaluation-time errors naming what is
allowed. Values are drawn live in dry-run and `orion-server test` too — like
`now`, avoid asserting exact outputs in regression cases.

| Operator | Example | Meaning |
|----------|---------|---------|
| `random` | `{ "random": ["digits", 6] }` | Kind-selected generation: `["uuid"]` / `["uuid", "v7"]` (canonical UUID; v7 is time-sortable), `["digits", n]` (exactly-n digits, leading zeros kept — the OTP shape, n ≤ 64), `["int", min, max]` (inclusive, within ±2⁵³−1), `["string", len, alphabet?]` (len ≤ 1024; named sets `alphanumeric` (default) / `hex` / `numeric` / `url-safe`, or a custom string of 2–256 distinct characters), `["bytes", n, encoding?]` (n ≤ 1024; `hex` default / `base64` / `base64url` per the encoding table above) |

## Sharp edges

Five operators take a shape or a value that is easy to get wrong. Each bullet
states the failure mode; most fail quietly, with a plausible answer rather
than an error.

- **`exists` takes path segments, not a dotted path.** Unlike `var`, it does
  not split on `.`, and it evaluates its arguments as literals rather than as
  expressions. `{ "exists": ["data.order.id"] }` looks for a single top-level
  key literally named `data.order.id` and returns `false`. Wrapping the
  argument in a `var` returns `false` too, because the *value* is not a path.
  Spell it out:

  ```json
  { "exists": ["data", "order", "id"] }
  ```

- **`switch` takes an array of `[case, result]` pairs**, not a flat
  alternating list. A flat list is not rejected: the second element is read as
  the case array, fails to match, and the third element is returned as the
  default arm. You silently get one fixed branch for every input.

  ```json
  { "switch": [
      { "var": "data.tier" },
      [ ["gold", 20], ["silver", 10] ],
      0
  ] }
  ```

- **`date_diff` units are plural.** `"days"`, `"hours"`, `"minutes"`,
  `"seconds"`. An unrecognized unit — including the singular `"day"` — returns
  `0` rather than an error, which reads exactly like "the dates are the same".

- **`timestamp` is not a datetime-to-epoch conversion.** It parses a
  *duration* (`"1d"` → `"1d:0h:0m:0s"`) for use in date arithmetic. Passing it
  a datetime is an `Invalid duration format` error.

- **`now` is evaluated per call**, so two `now` mappings in one workflow can
  land on different instants. Compute it once into a field and read that field
  if you need a single consistent stamp.

## Feature boundary

The extension categories — `datetime`, `ext-string`, `ext-array`, `ext-math`,
`ext-control`, `error-handling` — are Cargo features of datalogic-rs.

> [!NOTE]
> Orion reaches datalogic-rs through dataflow-rs and cannot enable a datalogic
> feature on its own. The extension operators are available only as dataflow-rs
> enables them; Orion turns them all on through dataflow-rs's `all-operators`
> feature. A build without that feature compiles only the [Core](#core) set.

## Related

- [Workflow Reference](./workflows.md) — the workflow object and the data
  context that conditions and mappings read.
- [Channel Configuration](./channel-config.md) — `validation_logic` and
  `key_logic`, and the dedicated context each one sees.
- [Function Reference](./functions.md) — every function's `input` schema,
  including the fields that accept logic values.
