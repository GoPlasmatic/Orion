# Workflow Reference

A **workflow** is a versioned, JSON-defined pipeline of tasks. A
[channel](../architecture/overview.md) links to a workflow by `workflow_id`;
when a request arrives, Orion matches an active workflow, runs its tasks in
order, and returns the resulting data context.

This page is the authoritative reference for the workflow JSON shape, the data
context model, conditions, and the draft → active lifecycle. For the per-task
`function.input` schemas, see the [Function Reference](./functions.md).

## The workflow object

Send this shape to `POST /api/v1/admin/workflows` (and `PUT .../{id}` to update
the draft). Fields marked **server-managed** are set by Orion and returned in
responses — you don't send them on create.

| Field | Type | Required | Default | Notes |
|-------|------|:--------:|---------|-------|
| `workflow_id` | string | no | auto (UUID v4) | Stable identifier. ≤128 chars, alphanumeric plus `.`, `-`, `_`, must start alphanumeric |
| `name` | string | **yes** | — | Human-readable name. ≤255 chars, non-empty |
| `description` | string | no | — | ≤2048 chars |
| `priority` | integer | no | `0` | Match order — higher priority workflows are evaluated first (see [Matching](#matching)) |
| `condition` | JSONLogic | no | `true` | Whether the workflow matches a request (see [Conditions](#conditions)) |
| `tasks` | array | **yes** | — | Ordered, non-empty list of [task objects](#tasks) |
| `tags` | string[] | no | `[]` | Free-form labels for filtering |
| `continue_on_error` | bool | no | `false` | If `true`, a failing task does not halt the pipeline (see [Error handling](#error-handling)) |
| `version` | integer | server-managed | `1` | Increments per saved version of a `workflow_id` |
| `status` | string | server-managed | `draft` | `draft` \| `active` \| `archived` |
| `rollout_percentage` | integer | server-managed | `100` | Share of traffic when activated (see [Rollout](#rollout)) |
| `created_at` / `updated_at` | string | server-managed | — | RFC 3339 timestamps |

Responses wrap the resource in a `data` envelope:

```json
{ "data": { "workflow_id": "high-value-order", "version": 1, "status": "draft", "...": "..." } }
```

Validation failures return `400` with a structured error envelope — see the
[Admin API](./admin-api.md#error-response-format) for the `FieldError` format.

## Tasks

Each entry in `tasks` is a single step in the pipeline:

| Field | Type | Required | Notes |
|-------|------|:--------:|-------|
| `id` | string | **yes** | Unique within the workflow; used in tracing |
| `name` | string | **yes** | Human-readable label |
| `function` | object | **yes** | The function to run — see below |
| `condition` | JSONLogic | no | If present and falsy, this task is skipped |

The `function` object names a [built-in function](./functions.md) and supplies
its `input`:

| Field | Type | Required | Notes |
|-------|------|:--------:|-------|
| `name` | string | **yes** | One of the [built-in functions](./functions.md) |
| `input` | object | depends | Function-specific parameters. Connector functions are schema-validated on create |

```json
{
  "id": "flag",
  "name": "Flag high-value order",
  "condition": { ">": [{ "var": "data.order.total" }, 10000] },
  "function": {
    "name": "map",
    "input": { "mappings": [{ "path": "data.order.flagged", "logic": true }] }
  }
}
```

## The data context

Tasks share a single JSON document, the **data context**, with two top-level
areas your JSONLogic can read:

- `data` — the working document. The request body is parsed into it by
  `parse_json` (or `parse_xml`), tasks read and write `data.*`, and for a sync
  channel the final `data` object is what's returned to the caller.
- `metadata` — request context such as headers, query params, and path params,
  available to conditions and validation.

Every connector function writes its result to a **dotted output path** named
`output` (a `map` task uses its mapping `path` instead), which is created
inside the context if it doesn't exist. Before 1.0 `http_call` and
`channel_call` called this field `response_path`; that name is still accepted
but `output` wins when both are present.

> **The parse-then-process pattern.** A workflow that reads request data should
> start with `parse_json`; otherwise conditions referencing `data.*` see an
> empty context.

```json
{
  "tasks": [
    { "id": "parse",   "name": "Parse",   "function": { "name": "parse_json", "input": { "source": "payload", "target": "order" } } },
    { "id": "process", "name": "Process", "condition": { ">": [{ "var": "data.order.total" }, 100] }, "function": { "name": "map", "input": { "mappings": [] } } }
  ]
}
```

## Conditions

Conditions are [JSONLogic](https://jsonlogic.com) expressions, evaluated by
[datalogic-rs](https://github.com/GoPlasmatic/datalogic-rs) and compiled once at
engine build time. They appear at two levels:

- **Workflow-level `condition`** — decides whether the whole workflow *matches* a
  request. Defaults to `true` (always matches). If multiple active workflows are
  bound to a channel, the first match wins (see [Matching](#matching)).
- **Task-level `condition`** — decides whether *that task* runs within a matched
  workflow. Use it for branching inside a pipeline.

## Available operators

The table below is the **complete** set Orion compiles. It is not the whole
JSONLogic spec: datalogic-rs gates its extension operators behind Cargo
features, and Orion enables them through dataflow-rs's `all-operators` feature
(`Cargo.toml`).

> **A misspelled operator inside a `map` mapping is not an error.** JSONLogic
> cannot distinguish `{ "upper": [...] }` used as an operator from a data object
> that happens to have one key, and mappings are rendered through a templating
> path that resolves the inner expressions and writes the object through as a
> *literal*. So `{ "uppr": [...] }` puts `{"uppr": "widget"}` at the target path
> instead of a string — no error, no failed task, `200` to the caller. **When a
> mapping yields a JSON object where you expected a scalar, check the operator
> name against these tables first.**
>
> Conditions behave differently: they are compiled and evaluated strictly, so
> the same misspelling there is a hard error rather than a silent literal.

`tests/integration/jsonlogic_operators_test.rs` asserts this table against the
engine, so it cannot drift from what actually runs.

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

Format strings use the JSONLogic vocabulary — `yyyy`, `MM`, `dd`, `HH`, `mm`,
`ss` — which is translated to the underlying `strftime` spec, so raw `%Y`-style
patterns also work. Prefer the `yyyy` form; it is the documented one.

Three sharp edges here, each of which fails quietly rather than loudly:

- **`date_diff` units are plural.** `"days"`, `"hours"`, `"minutes"`,
  `"seconds"`. An unrecognised unit — including the singular `"day"` — returns
  `0` rather than an error, which reads exactly like "the dates are the same".
- **`timestamp` is not a datetime-to-epoch conversion.** It parses a *duration*
  (`"1d"` → `"1d:0h:0m:0s"`) for use in date arithmetic. Passing it a datetime
  is an `Invalid duration format` error.
- **`now` is evaluated per call**, so two `now` mappings in one workflow can
  land on different instants. Compute it once into a field and read that field
  if you need a single consistent stamp.

### Strings (`ext-string`)

| Operator | Example | Meaning |
|----------|---------|---------|
| `length` | `{ "length": [{ "var": "data.items" }] }` | Length of a string **or** array |
| `upper` / `lower` | `{ "upper": [{ "var": "data.code" }] }` | Change case |
| `trim` | `{ "trim": [{ "var": "data.name" }] }` | Strip surrounding whitespace |
| `split` | `{ "split": [{ "var": "data.csv" }, ","] }` | Split into an array |
| `starts_with` / `ends_with` | `{ "starts_with": [{ "var": "data.sku" }, "AB-"] }` | Prefix / suffix test |

### Arrays (`ext-array`) and maths (`ext-math`)

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
| `exists` | `{ "exists": ["data", "order", "id"] }` | Path presence — **see below** |
| `switch` / `match` | see below | Multi-way branch |
| `try` / `throw` | `{ "try": [expr, fallback] }` | Catch / raise an evaluation error |

Two of these take a shape that is easy to get wrong, and both fail **silently**
with a plausible answer rather than an error:

**`exists` takes path segments, not a dotted path.** Unlike `var`, it does not
split on `.`, and it evaluates its arguments as literals rather than as
expressions. `{ "exists": ["data.order.id"] }` looks for a single top-level key
literally named `data.order.id` and returns `false`; wrapping the argument in a
`var` returns `false` too, because the *value* is not a path. Spell it out:

```json
{ "exists": ["data", "order", "id"] }
```

**`switch` takes an array of `[case, result]` pairs**, not a flat alternating
list. A flat list is not rejected — the second element is read as the case
array, fails to match, and the third element is returned as the default arm, so
you silently get one fixed branch for every input:

```json
{ "switch": [
    { "var": "data.tier" },
    [ ["gold", 20], ["silver", 10] ],
    0
] }
```

## Error handling

By default the pipeline **halts** on the first task that errors, and the error is
returned to the caller. Set `continue_on_error: true` on the workflow to keep
running subsequent tasks and collect errors instead. The
[`filter`](./functions.md#filter) function offers finer control: `on_reject:
"halt"` stops the workflow, while `on_reject: "skip"` skips only the current task.

For async channels, a task failure routes the trace to the Dead Letter Queue for
automatic retry — see [Resilience](../features/resilience.md).

## Lifecycle and versioning

Each `workflow_id` has one or more **versions**, identified by the composite key
`(workflow_id, version)`. Status moves in one direction:

```orion-diagram
{
  "direction": "LR",
  "nodes": [
    { "id": "draft",    "label": "draft",    "type": "infra" },
    { "id": "active",   "label": "active",   "type": "channel" },
    { "id": "archived", "label": "archived", "type": "datastore", "shape": "rectangle" }
  ],
  "edges": [
    { "from": "draft",  "to": "active",   "label": "activate" },
    { "from": "active", "to": "archived", "label": "archive" }
  ]
}
```

- **draft** — editable; not served. Only **one draft per `workflow_id`** may exist
  at a time. Creating a workflow starts it as a draft.
- **active** — served; **immutable**. To change an active workflow, create a new
  draft version, edit it, and activate it.
- **archived** — retired; kept for history and instant rollback.

Endpoints (see the [Admin API](./admin-api.md#workflows) for full details):

| Action | Endpoint |
|--------|----------|
| Validate without saving | `POST /api/v1/admin/workflows/validate` |
| Create (as draft) | `POST /api/v1/admin/workflows` |
| New draft version of an existing id | `POST /api/v1/admin/workflows/{id}/versions` |
| Dry-run against sample data | `POST /api/v1/admin/workflows/{id}/test` |
| Change status | `PATCH /api/v1/admin/workflows/{id}/status` |
| Adjust rollout | `PATCH /api/v1/admin/workflows/{id}/rollout` |

### Matching

When a channel resolves to its workflows, Orion evaluates **active** workflows in
descending `priority`, then runs the first whose `condition` is truthy. Give a
catch-all workflow a low priority and specific ones a higher priority to layer
behavior.

### Rollout

`rollout_percentage` (1–100) enables canary releases across versions. Activating a
new version at, say, `25` directs ~25% of traffic to it and the remainder to the
previously active version; traffic is bucketed by a stable hash of the request so
a given caller is routed consistently. Promote by raising the percentage to `100`
(which archives the older active version), or roll back instantly by re-activating
a previous version.

```bash
# Activate a new version to 10% of traffic
curl -X PATCH http://localhost:8080/api/v1/admin/workflows/high-value-order/status \
  -H "Content-Type: application/json" -d '{ "status": "active", "rollout_percentage": 10 }'

# Ramp up later
curl -X PATCH http://localhost:8080/api/v1/admin/workflows/high-value-order/rollout \
  -H "Content-Type: application/json" -d '{ "rollout_percentage": 50 }'
```

## Complete example

```json
{
  "workflow_id": "high-value-order",
  "name": "High-Value Order",
  "description": "Flag orders over $10,000 for manual review",
  "priority": 10,
  "condition": { "==": [{ "var": "metadata.headers.x-source" }, "checkout"] },
  "tasks": [
    {
      "id": "parse",
      "name": "Parse payload",
      "function": { "name": "parse_json", "input": { "source": "payload", "target": "order" } }
    },
    {
      "id": "validate",
      "name": "Validate order",
      "function": {
        "name": "validation",
        "input": { "rules": [
          { "logic": { "!!": [{ "var": "data.order.id" }] }, "message": "order id is required" },
          { "logic": { ">": [{ "var": "data.order.total" }, 0] }, "message": "total must be positive" }
        ]}
      }
    },
    {
      "id": "flag",
      "name": "Flag for review",
      "condition": { ">": [{ "var": "data.order.total" }, 10000] },
      "function": {
        "name": "map",
        "input": { "mappings": [
          { "path": "data.order.flagged", "logic": true },
          { "path": "data.order.alert", "logic": { "cat": ["High-value order: $", { "var": "data.order.total" }] } }
        ]}
      }
    }
  ],
  "tags": ["orders", "risk"],
  "continue_on_error": false
}
```

See [Use Cases & Patterns](../tutorials/use-cases.md) for complete, tested
workflows, and the [Function Reference](./functions.md) for every function's
input schema.
