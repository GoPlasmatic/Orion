# Workflow reference

A workflow is a versioned, JSON-defined pipeline of tasks. A channel links to
one by `workflow_id`. When a request arrives, Orion matches an active workflow,
runs its tasks in order, and returns the resulting data context.

## The workflow object

`POST /api/v1/admin/workflows` creates it; `PUT .../{id}` replaces the draft.
Server-managed fields are set by Orion — do not send them on create.

| Field | Type | Required | Default | Notes |
|---|---|:--:|---|---|
| `workflow_id` | string | no | auto UUID v4 | ≤128 chars, alphanumeric plus `.`, `-`, `_`, must start alphanumeric |
| `name` | string | **yes** | — | ≤255 chars, non-empty |
| `description` | string | no | — | ≤2048 chars |
| `priority` | integer | no | `0` | Higher is evaluated first when several workflows could match |
| `condition` | JSONLogic | no | `true` | Whether this workflow matches the request |
| `tasks` | array | **yes** | — | Ordered, non-empty list of tasks or task groups |
| `tags` | string[] | no | `[]` | Free-form labels; filter with `--tag` |
| `loop` | object | no | — | Run the whole task list once per sweep |
| `continue_on_error` | bool | no | `false` | A failing task does not halt the pipeline |
| `version` | integer | server-managed | `1` | Increments per saved version |
| `status` | string | server-managed | `draft` | `draft` \| `active` \| `archived` |
| `rollout_percentage` | integer | server-managed | `100` | Traffic share when active |
| `created_at` / `updated_at` | string | server-managed | — | RFC 3339 |

Responses wrap the resource in a `data` envelope. Validation failures return
`400` with field-pathed errors.

## Tasks

| Field | Type | Required | Default | Notes |
|---|---|:--:|---|---|
| `id` | string | **yes** | — | Unique within the workflow; appears in traces |
| `name` | string | **yes** | — | Human-readable label |
| `function` | object | **yes** | — | `{ "name": …, "input": { … } }` |
| `condition` | JSONLogic | no | — | Falsy means this task is skipped |
| `continue_on_error` | bool | no | inherits workflow | Per-task override |
| `terminal` | bool | no | `false` | End the workflow after this step runs |

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

### Task groups

An element of `tasks` that carries its own `tasks` key is a **task group** —
one condition guarding a contiguous run of steps, instead of the same condition
repeated on every one. Presence of a `tasks` key is the *only* rule that
distinguishes the two shapes; a group has no `function`. An element with
neither is reported as a broken task, not an empty group.

| Field | Required | Notes |
|---|:--:|---|
| `id` | **yes** | Shares one id namespace with tasks — a collision is refused at create |
| `tasks` | **yes** | Non-empty; a condition guarding nothing is refused |
| `condition` | no | Evaluated **once, on entry**. Falsy skips the whole span without evaluating members' own conditions |
| `terminal` | no | End the workflow after the whole span |
| `name` / `description` | no | Optional here, unlike on a task where `name` is required |

Groups nest up to 8 levels. Everything downstream — traces, audit trails,
`metadata.progress` — sees the **flattened** list: the group is a property of
the definition, not a step that runs.

```json
{ "id": "not_found",
  "condition": { "==": [{ "var": "data.user" }, null] },
  "terminal": true,
  "tasks": [
    { "id": "body", "name": "404 body", "function": { "name": "map", "input": { "mappings": [
        { "path": "data.out", "logic": { "error": "User Not Found" } } ] } } },
    { "id": "status", "name": "404 status", "function": { "name": "map", "input": { "mappings": [
        { "path": "data._orion.response", "logic": { "status": 404, "body_path": "data.out" } } ] } } }
  ] }
```

### Terminal steps

`terminal: true` is the guard clause: *if this, answer and stop*. It is about
**position, not outcome**:

- A falsy `condition` does not halt — the step did not run.
- A skipped task does not halt.
- A task that *failed* under `continue_on_error: true` **does** halt, because
  the author said nothing runs after this one.

Task groups and `terminal` need dataflow-rs 3.6 (Orion 1.2.0+). On an older
engine a group **fails to load** loudly, but a bare `terminal: true` is
silently ignored and every later task runs — gate on the server version when
deploying to instances you do not control.

## The data context

Tasks share one JSON document. Its top level holds exactly three readable
areas:

| Area | Starts as | Purpose |
|---|---|---|
| `data` | `{}` | The working document. For a sync channel the final `data` is the response body |
| `metadata` | ingress-stamped | Request context — channel, method, headers, params |
| `temp_data` | `{}` | Scratch space. Not part of the response |

**`payload` is not in the context.** The raw ingress payload lives outside the
JSONLogic context, so `{"var": "payload.x"}` resolves to nothing. `parse_json`
or `parse_xml` with `source: "payload"` is the only way to reach it — which is
why a workflow that reads request data must start with one.

### Output paths

Every connector function writes its result to a dotted path named `output`; a
`map` task uses its mapping `path` instead. Orion creates the path if it does
not exist. `http_call` and `channel_call` also accept the pre-1.0 spelling
`response_path` — supplying **both** is a duplicate-field error, not a
precedence rule.

### Request metadata

**HTTP** — Orion copies a caller-supplied `metadata` object first, then stamps
its own keys on top:

| Key | Stamped | Value |
|---|---|---|
| `channel` | always | Resolved channel name; always overrides a caller value |
| `http_method` | always | `POST`, `GET`, … |
| `params` | when non-empty | Path parameters from the REST route pattern |
| `query` | when non-empty | Query-string parameters |
| `headers` | always | All headers, names lowercased; `authorization`, `cookie`, `proxy-authorization` and `x-api-key` values masked |
| `cookies` | when the channel opts in | Only those named by `request.cookies_to_metadata` |

Nothing else on the HTTP path — no client IP, no request path, no trace id. A
channel with `request.body_mode = "payload"` takes no caller `metadata` at all.

**Kafka** — `channel`, `kafka_topic`, `kafka_partition`, `kafka_offset`, and
`kafka_key` (only when valid UTF-8). Record headers are not copied.

**`channel_call`** — the called workflow inherits the parent's `metadata`, with
`channel` overwritten and two tracking keys added: `_orion_call_depth` and
`_orion_call_chain`.

### The `_orion` namespace

Orion reserves these keys and no others:

| Key | Meaning |
|---|---|
| `data._orion.response` | Shaped channels write `status`, `headers`, `body` here; Orion drains it before replying |
| `_orion.profile` | Per-task timings in the response envelope when profiling is requested — never in the context |
| `metadata._orion_call_depth` | `channel_call` nesting depth |
| `metadata._orion_call_chain` | Channel names traversed |
| `metadata._orion_errors` | Codes of tasks that failed in this run |

## Error handling

By default the pipeline **halts** on the first task that errors and returns the
error to the caller. `continue_on_error: true` on the workflow keeps running
and collects errors; on a single task it makes just that step non-fatal.
`filter` gives finer control: `on_reject: "halt"` stops the workflow,
`on_reject: "skip"` skips only the current task.

For async channels, a task failure routes the trace to the dead-letter queue
for automatic retry.

### Branching on a failure

A continued run appends a record to `metadata._orion_errors`:

```json
{ "task_id": "charge_payment", "workflow_id": "place-order",
  "code": "TIMEOUT_ERROR", "status": 500 }
```

`code` is closed: `VALIDATION_ERROR`, `WORKFLOW_ERROR`, `TASK_ERROR`,
`FUNCTION_NOT_FOUND`, `FUNCTION_ERROR`, `LOGIC_ERROR`, `HTTP_ERROR`,
`TIMEOUT_ERROR`, `IO_ERROR`, `DESERIALIZATION_ERROR`, `UNKNOWN_ERROR`, plus
service kinds Orion mints such as `circuit_open`. `status` separates a handler
that errored (always `500`) from a task that returned a 5xx *outcome*.

```json
{ "id": "retry_later",
  "condition": { "in": [ { "var": "metadata._orion_errors.0.code" },
                         ["TIMEOUT_ERROR", "IO_ERROR"] ] } }
```

Three properties: **no message ever** (codes and task ids only, so an upstream
URL or body cannot leak past `verbose_errors`), **not caller-supplied** (the
key is cleared at every ingress and reset on `channel_call`), and **bounded**
(only the most recent records are kept).

`metadata.progress` is the older, weaker signal — a single slot overwritten by
every task, carrying no reason. Prefer `_orion_errors`.

## Loop

A workflow with a `loop` runs its **whole task list once per sweep**. It is how
one workflow calls a connector per array element, which a JSONLogic `map`
cannot do.

| Field | Required | Default | Notes |
|---|:--:|---|---|
| `max` | **yes** | — | Upper bound on sweeps. Half-open: `init: 0, max: 10` yields `0`–`9` |
| `counter` | no | — | `temp_data` field holding the count — `"i"` is `temp_data.i`; dots nest |
| `init` | no | `0` | First counter value |
| `increment` | no | `1` | At least `1`, so the counter always advances |

Per sweep, in order: write the counter, check `counter < max`, re-evaluate the
**workflow** condition, run the task list.

**Do not put the break in the workflow `condition`.** `data` starts empty, so a
condition reading `data.*` is false on sweep 0 — before any `parse_json` has
run — and the loop never starts. Put the break in the body as a `filter` task
with `on_reject: "halt"`, which ends the whole loop rather than the sweep.

`max` is structural — the loop cannot outrun it. Reaching it is normal
completion, not an error. It is capped by `engine.max_loop_iterations`
(default `10000`); exceeding that is refused with `400` at write time.

## Matching and rollout

If several active workflows are bound to a channel, they are evaluated in
`priority` order (higher first) and the first whose `condition` matches wins.

`rollout_percentage` (`1`–`100`) is the traffic share an active version takes.
`0` is refused — a version serving no traffic is an archived version.

## Lifecycle

```
draft --activate--> active --archive--> archived
```

- **draft** — editable, not served. Only **one draft per `workflow_id`** at a
  time. Create starts here.
- **active** — served, **immutable**.
- **archived** — retired and kept. It is a rollback *source*: copy its content
  into a new draft. Nothing reactivates it in place.
