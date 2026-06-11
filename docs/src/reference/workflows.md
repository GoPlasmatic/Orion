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
[Admin API](../api/admin.md#error-response-format) for the `FieldError` format.

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
| `name` | string | **yes** | One of the [16 built-in functions](./functions.md) |
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

Most functions write their result to a **dotted output path** (e.g.
`response_path`, `output`, or a `map` mapping `path`), which is created inside
the context if it doesn't exist.

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

Common operators (see the [JSONLogic spec](https://jsonlogic.com/operations.html)
for the full set):

| Operator | Example | Meaning |
|----------|---------|---------|
| `var` | `{ "var": "data.order.total" }` | Read a value from the context |
| `==` / `!=` | `{ "==": [{ "var": "data.type" }, "order"] }` | Loose equality |
| `>` `>=` `<` `<=` | `{ ">": [{ "var": "data.order.total" }, 10000] }` | Comparison |
| `and` / `or` / `!` | `{ "and": [a, b] }` | Boolean logic |
| `!!` | `{ "!!": [{ "var": "data.order.id" }] }` | Truthiness (e.g. "is present") |
| `if` | `{ "if": [cond, then, else] }` | Conditional value |
| `in` | `{ "in": [{ "var": "data.tier" }, ["vip", "premium"]] }` | Membership |
| `cat` | `{ "cat": ["Order #", { "var": "data.order.id" }] }` | String concatenation |
| `+` `-` `*` `/` `%` | `{ "*": [{ "var": "data.qty" }, 1.1] }` | Arithmetic |

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

Endpoints (see the [Admin API](../api/admin.md#workflows) for full details):

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
