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
| `format_date` | `{ "format_date": [{ "now": [] }, "yyyy-MM-dd"] }` | Format a datetime |
| `date_diff` | `{ "date_diff": [a, b, "days"] }` | Whole units between two datetimes |
| `timestamp` | `{ "timestamp": ["1d"] }` | Build a **duration** from a duration string |

Format strings use the JSONLogic vocabulary: `yyyy`, `MM`, `dd`, `HH`, `mm`,
`ss`. Orion translates it to the underlying `strftime` spec, so raw
`%Y`-style patterns also work. Prefer the `yyyy` form; it is the documented
one.

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

### Control (`ext-control`, `error-handling`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `??` | `{ "??": [{ "var": "data.nickname" }, "anonymous"] }` | Coalesce — first non-null |
| `type` | `{ "type": [{ "var": "data.price" }] }` | Type name as a string |
| `exists` | `{ "exists": ["data", "order", "id"] }` | Path presence — see [Sharp edges](#sharp-edges) |
| `switch` / `match` | see [Sharp edges](#sharp-edges) | Multi-way branch |
| `try` / `throw` | `{ "try": [expr, fallback] }` | Catch / raise an evaluation error |

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
