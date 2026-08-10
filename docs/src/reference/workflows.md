# Workflow Schema

A **workflow** is a versioned, JSON-defined pipeline of tasks. A
[channel](../concepts/channels.md) links to a workflow by `workflow_id`.
When a request arrives, Orion matches an active workflow, runs its tasks in
order, and returns the resulting data context.

Per-task `function.input` schemas live in the
[Function Reference](./functions.md). The operator catalog for conditions and
mappings lives in the [Expression Reference](./expressions.md).

## The workflow object

Send this shape to `POST /api/v1/admin/workflows`; `PUT .../{id}` updates the
draft. Fields marked *server-managed* are set by Orion and returned in
responses — you do not send them on create.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `workflow_id` | string | no | auto (UUID v4) | Stable identifier. ≤128 chars, alphanumeric plus `.`, `-`, `_`, must start alphanumeric |
| `name` | string | **yes** | — | Human-readable name. ≤255 chars, non-empty |
| `description` | string | no | — | ≤2048 chars |
| `priority` | integer | no | `0` | Match order. Higher-priority workflows are evaluated first (see [Matching](#matching)) |
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

Validation failures return `400` with a field-level error envelope — see
[Errors](./errors.md) for the format.

## Tasks

Each entry in `tasks` is a single step in the pipeline:

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `id` | string | **yes** | — | Unique within the workflow; used in tracing |
| `name` | string | **yes** | — | Human-readable label |
| `function` | object | **yes** | — | The function to run — see below |
| `condition` | JSONLogic | no | — | If present and falsy, this task is skipped |

The `function` object names a [built-in function](./functions.md) and supplies
its `input`:

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `name` | string | **yes** | — | One of the [built-in functions](./functions.md) |
| `input` | object | depends | — | Function-specific parameters. Connector functions are schema-validated on create |

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

Tasks share a single JSON document, the **data context**. Its top level holds
exactly three areas your JSONLogic can read:

| Area | Starts as | Purpose |
|------|-----------|---------|
| `data` | `{}` | The working document. Tasks read and write `data.*`. For a sync channel, the final `data` object is the response body |
| `metadata` | ingress-stamped | Request context — channel name, method, headers, and params (see [Request metadata](#request-metadata)) |
| `temp_data` | `{}` | Scratch space for intermediate values. Not part of the response |

> [!WARNING]
> **`payload` is not in the context.** The raw ingress payload lives in a
> sibling field outside the JSONLogic context, so `{"var": "payload.x"}`
> resolves to nothing. The only way to reach it is `parse_json` or `parse_xml`
> with `source: "payload"`, which parses it into `data`.

> [!TIP]
> **The parse-then-process pattern.** A workflow that reads request data must
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

### Output paths

Every connector function writes its result to a dotted path named `output`; a
`map` task uses its mapping `path` instead. Orion creates the path inside the
context if it does not exist. `http_call` and `channel_call` also accept the
pre-1.0 spelling `response_path`; `output` wins when both are present.

### Request metadata

The ingress that receives a request stamps `metadata`. Each ingress stamps a
different set of keys.

**HTTP.** Orion copies the caller's `metadata` object first, when the request
supplies one, then stamps its own keys on top:

| Key | Stamped | Value |
|-----|---------|-------|
| `channel` | always | The resolved channel name. Always overrides a caller-supplied value |
| `http_method` | always | The request method (`POST`, `GET`, …) |
| `params` | only when non-empty | Path parameters extracted by the REST route pattern |
| `query` | only when non-empty | Query-string parameters |
| `headers` | always | Every request header, names lowercased. Values of `authorization`, `cookie`, `proxy-authorization`, and `x-api-key` are masked |

Orion stamps nothing else on the HTTP path: no client IP, no request path, no
trace ID.

**Kafka.** A consumed record stamps:

| Key | Value |
|-----|-------|
| `channel` | The channel bound to the topic |
| `kafka_topic` | Source topic |
| `kafka_partition` | Partition number |
| `kafka_offset` | Record offset |
| `kafka_key` | The record key — stamped only when it is valid UTF-8 |

Kafka record headers are not copied into `metadata`.

**`channel_call`.** The called workflow inherits the parent's `metadata`.
Orion overwrites `channel` with the target channel's name and adds two
tracking keys: `_orion_call_depth` (nesting depth) and `_orion_call_chain`
(the array of channel names traversed).

### The `_orion` namespace

Orion reserves these keys and no others:

| Key | Where | Meaning |
|-----|-------|---------|
| `data._orion.response` | context | Shaped channels write the response `status`, `headers`, and `body` here; Orion drains it before replying. See [Channel Configuration](./channel-config.md) |
| `_orion.profile` | response envelope | Per-task timings when profiling is requested. Never in the context. See the [Data API](./data-api.md) |
| `metadata._orion_call_depth` | context | `channel_call` nesting depth |
| `metadata._orion_call_chain` | context | Channel names traversed by nested `channel_call`s |

No other key or prefix in the context is reserved.

## Conditions

Conditions are [JSONLogic](https://jsonlogic.com) expressions, compiled once
at engine build time. They appear at two levels:

- **Workflow-level `condition`** — decides whether the whole workflow
  *matches* a request. Defaults to `true` (always matches). If multiple active
  workflows are bound to a channel, the first match wins (see
  [Matching](#matching)).
- **Task-level `condition`** — decides whether *that task* runs within a
  matched workflow. Use it for branching inside a pipeline.

The complete operator set — core JSONLogic plus the date, string, array, math,
and control extensions — is cataloged in the
[Expression Reference](./expressions.md). That page also documents the
silent-failure edges, including misspelled operators inside `map` mappings.

## Error handling

By default the pipeline **halts** on the first task that errors, and the error
is returned to the caller. Set `continue_on_error: true` on the workflow to
keep running subsequent tasks and collect errors instead. The
[`filter`](./functions.md#filter) function offers finer control: `on_reject:
"halt"` stops the workflow, while `on_reject: "skip"` skips only the current
task.

For async channels, a task failure routes the trace to the dead letter queue
for automatic retry.

## Lifecycle and versioning

Each `workflow_id` has one or more **versions**, identified by the composite
key `(workflow_id, version)`. Status moves in one direction:

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

- **draft** — editable; not served. Only **one draft per `workflow_id`** may
  exist at a time. Creating a workflow starts it as a draft.
- **active** — served; **immutable**. To change an active workflow, create a
  new draft version, edit it, and activate it.
- **archived** — retired; kept for history and instant rollback.

| Action | Endpoint |
|--------|----------|
| Validate without saving | `POST /api/v1/admin/workflows/validate` |
| Create (as draft) | `POST /api/v1/admin/workflows` |
| New draft version of an existing id | `POST /api/v1/admin/workflows/{id}/versions` |
| Dry-run against sample data | `POST /api/v1/admin/workflows/{id}/test` |
| Change status | `PATCH /api/v1/admin/workflows/{id}/status` |
| Adjust rollout | `PATCH /api/v1/admin/workflows/{id}/rollout` |

The Admin API owns endpoint semantics: see
[Lifecycle](./admin-api.md#lifecycle) for the first four rows and
[Status changes](./admin-api.md#status-changes) for status and rollout.

### Matching

When a channel resolves its workflows, Orion evaluates **active** workflows in
descending `priority` and runs the first whose `condition` is truthy. Give a
catch-all workflow a low priority and specific ones a higher priority to layer
behavior.

### Rollout

`rollout_percentage` (1–100) enables canary releases across versions.
Activating a new version at `25` directs about 25% of traffic to it; the
remainder goes to the previously active version. Traffic is bucketed by a
stable hash of the request, so a given caller routes consistently. Promote by
raising the percentage to `100`, which archives the older active version. Roll
back instantly by re-activating a previous version. Both moves go through the
status and rollout endpoints — see
[Status changes](./admin-api.md#status-changes).

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

## Related

- [Function Reference](./functions.md) — the `input` schema for every built-in
  function.
- [Expression Reference](./expressions.md) — every operator conditions and
  mappings can use, with the silent-failure edges.
- [Channel Configuration](./channel-config.md) — how a channel binds to a
  workflow and shapes its response.
- [Admin API](./admin-api.md#lifecycle) — the endpoints that create, version,
  and activate workflows.
