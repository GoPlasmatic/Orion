<!-- description: The Orion workflow object in full: fields, tasks, task groups, terminal steps, the shared data context, request metadata, loops, error handling and rollout. -->
# Workflow JSON Schema

**Page type:** Reference · **Audience:** Workflow authors and tooling developers

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
| `loop` | object | no | — | Run the task list once per sweep instead of once in total (see [Loop](#loop)) |
| `continue_on_error` | bool | no | `false` | If `true`, a task that errors or records `5xx` does not halt the pipeline. A `4xx` never halts either way — see [Error handling](#error-handling) |
| `version` | integer | server-managed | `1` | Increments per saved version of a `workflow_id` |
| `status` | string | server-managed | `draft` | `draft` \| `active` \| `archived` |
| `rollout_percentage` | integer | server-managed | `100` | Share of traffic when activated (see [Rollout](#rollout)) |
| `created_at` / `updated_at` | string | server-managed | — | RFC 3339 timestamps |

Responses wrap the resource in a `data` envelope:

```json
{ "data": { "workflow_id": "high-value-order", "version": 1, "status": "draft", "...": "..." } }
```

Validation failures return `400` with a field-level error envelope. See
[Errors](./errors.md) for the format.

## Tasks

Each entry in `tasks` is a single step in the pipeline:

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `id` | string | **yes** | — | Unique within the workflow; used in tracing |
| `name` | string | **yes** | — | Human-readable label |
| `function` | object | **yes** | — | The function to run — see below |
| `condition` | JSONLogic | no | — | If present and falsy, this task is skipped |
| `continue_on_error` | bool | no | inherits the workflow | Per-task override: `true` lets the pipeline continue past **this** task's error or `5xx`. To stop on a `4xx`, use `halt_on` |
| `terminal` | bool | no | `false` | End the workflow after this step runs. About **position, not outcome** — see [Terminal steps](#terminal-steps) |
| `halt_on` | string | no | `"never"` | `"failure"` ends the workflow when *this task* failed. About **outcome, not position** — see [Halting on failure](#halting-on-failure). Tasks only; a group carrying it is refused |

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

### Task groups

**Since:** Orion 1.2

An element of `tasks` carrying its own `tasks` key is a **task group** rather
than a task: one condition guarding a contiguous run of steps, instead of the
same condition repeated on each.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `id` | string | **yes** | — | Unique within the workflow. Groups share **one id namespace with tasks** — a group id colliding with a task id is refused at create |
| `tasks` | array | **yes** | — | The steps in the span. Must be non-empty: a condition guarding nothing is refused |
| `condition` | JSONLogic | no | `true` | Evaluated **once, on entry**. A falsy result skips the whole span without evaluating the members' own conditions |
| `terminal` | bool | no | `false` | End the workflow after the whole span runs |
| `name` | string | no | — | Human-readable label. Optional here, unlike on a task, where it is required |
| `description` | string | no | — | What the span covers |

A group has no `function` — that is exactly what distinguishes the two shapes,
and the rule the parser applies is **presence of a `tasks` key**, nothing else.
An element carrying neither `function` nor `tasks` is reported as a broken
task, not an empty group.

Groups nest, up to 8 levels deep. Deeper than that is refused at create: it is
a generated-JSON accident rather than an authored control-flow shape, and the
engine will not build it.

```json
{ "id": "not_found",
  "condition": { "==": [{ "var": "data.user" }, null] },
  "terminal": true,
  "tasks": [
    { "id": "body",   "name": "404 body",   "function": { "name": "map", "input": { "mappings": [
        { "path": "data.out", "logic": { "error": "User Not Found" } } ] } } },
    { "id": "status", "name": "404 status", "function": { "name": "map", "input": { "mappings": [
        { "path": "data._orion.response", "logic": { "status": 404, "body_path": "data.out" } } ] } } }
  ] }
```

Everything downstream of the definition sees the **flattened** list — a trace,
an audit trail and `metadata.progress` report the member tasks, not the group.
The group is a property of the definition, not a step that runs.

### Terminal steps

`terminal: true` ends the workflow after the step runs — on a group, after the
whole span. With a condition, that is the guard clause: *if this, answer and
stop*. Without it, every later task has to restate the negation of every
earlier exit, and those conditions grow with each branch added.

It is about **position, not outcome**:

- A falsy `condition` does not halt — the step did not run.
- A skipped task does not halt.
- A task that *failed* under `continue_on_error: true` **does** halt, because
  the author said nothing runs after this one.

> [!NOTE]
> Task groups and `terminal` need dataflow-rs 3.6, which Orion 1.2.0 ships. A
> definition using a group **fails to load** on an older engine, loudly; a bare
> `terminal: true` is silently ignored there and every later task runs. Gate on
> the server version if you deploy definitions to instances you do not control.

### Halting on failure

`halt_on: "failure"` ends the workflow when the task **failed**: a recorded
status of `400` or above, which covers a `validation` rule that did not pass,
any task returning that range, and a handler error. A success falls through.

It is the other axis from `terminal`: position versus outcome. The two compose
by *or*, so `terminal` stays strictly stronger and no combination contradicts.

```json
{ "id": "check_state", "name": "Check the state token", "halt_on": "failure",
  "function": { "name": "validation", "input": { "rules": [
    { "logic": { "==": [{ "var": "metadata.query.state" },
                        { "var": "metadata.cookies.oauth_state" }] },
      "message": "state mismatch" } ] } } }
```

**Without it, a failing `validation` does not stop anything.** A failed rule
records status `400`, and the engine's rule is that `4xx` warns and carries on;
`continue_on_error` governs `5xx` and handler errors only. So a `validation`
followed by unguarded tasks records an error and proceeds exactly as if it had
passed, which is how a check that reads correct ships doing nothing.

Collecting every rule failure and carrying on is a legitimate shape, and the
default keeps it. When you meant a gate, say so:

| You want | Write |
|---|---|
| Stop, keeping the task's own status | `"halt_on": "failure"` on the task |
| Stop, and answer with a chosen status and body | a later task with a `condition` on the failure and `terminal: true` |
| Stop, with no body | a [`filter`](./functions.md#filter) task (records `299`) |

`orion-server lint` reports a `validation` that is not acting as a gate and has
nothing guarded after it (`engine.unguarded_validation`), so the silent version
does not have to be found in production.

`halt_on` belongs to a **task**. A group has no outcome of its own, so one
carrying `halt_on` is refused at parse — loudly, rather than as a guard that
never fires. `continue_on_error` on a group is the same mistake resolved the
other way: it parses, the engine drops it, and `lint` reports
`engine.group_continue_on_error`.

> [!NOTE]
> `halt_on` needs dataflow-rs 3.10, which Orion 1.6.0 ships.

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
pre-1.0 spelling `response_path`. Supplying **both** in one input is a
duplicate-field error, not a precedence rule. See
[Support & Compatibility](./support.md#accepted-alternate-spellings).

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
| `cookies` | always, when the channel opts in | The cookies named by [`request.cookies_to_metadata`](./channel-config.md#reading-request-cookies). Absent block → the key is stripped, so a caller cannot supply it |
| `vars` | always, when the instance declares any | The [`[vars]`](./configuration.md#vars-and-secrets) config section verbatim. No `[vars]` → the key is stripped, so a caller cannot supply it |

Orion stamps nothing else on the HTTP path: no client IP, no request path, no
trace ID. A channel in
[`request.body_mode = "payload"`](./channel-config.md#request-body) takes no
caller `metadata` at all — the object is server-stamped keys only.

**Kafka.** A consumed record stamps:

| Key | Value |
|-----|-------|
| `channel` | The channel bound to the topic |
| `kafka_topic` | Source topic |
| `kafka_partition` | Partition number |
| `kafka_offset` | Record offset |
| `kafka_key` | The record key — stamped only when it is valid UTF-8 |
| `vars` | The `[vars]` config section, as on the HTTP path — a workflow reads the same deployment values whichever transport reached it |

Kafka record headers are not copied into `metadata`.

**`channel_call`.** The called workflow inherits the parent's `metadata`.
Orion overwrites `channel` with the target channel's name and adds two
tracking keys: `_orion_call_depth` (nesting depth) and `_orion_call_chain`
(the array of channel names traversed).

### The `_orion` namespace

Orion reserves these keys and no others:

| Key | Where | Meaning |
|-----|-------|---------|
| `data._orion.response` | context | Shaped channels write the response `status`, `headers`, `cookies` and `body` here; Orion drains it before replying. See [Channel Configuration](./channel-config.md#response-shaping) |
| `_orion.profile` | response envelope | Per-task timings when profiling is requested. Never in the context. See the [Data API](./data-api.md) |
| `metadata._orion_call_depth` | context | `channel_call` nesting depth |
| `metadata._orion_call_chain` | context | Channel names traversed by nested `channel_call`s |
| `metadata._orion_errors` | context | Codes of tasks that failed in this run — see [Branching on a failure](#branching-on-a-failure) |

No other key or *prefix* in the context is reserved, but three plain metadata
keys are still platform-owned and force-stamped at every ingress, so a caller
cannot supply them: `channel`, `cookies` and `vars` (see [Request
metadata](#request-metadata)). `metadata.progress` is engine-owned too, but it
belongs to dataflow-rs rather than Orion. See below.

### Branching on a failure

When a task fails and the run continues (`continue_on_error`), the engine
appends a record to `metadata._orion_errors`, so a later task can answer
differently depending on **why** the step failed:

```json
{ "task_id": "charge_payment", "workflow_id": "place-order",
  "code": "TIMEOUT_ERROR", "status": 500 }
```

`code` is a closed vocabulary — `VALIDATION_ERROR`, `WORKFLOW_ERROR`,
`TASK_ERROR`, `FUNCTION_NOT_FOUND`, `FUNCTION_ERROR`, `LOGIC_ERROR`,
`HTTP_ERROR`, `TIMEOUT_ERROR`, `IO_ERROR`, `DESERIALIZATION_ERROR`,
`UNKNOWN_ERROR`, plus service kinds Orion mints: `circuit_open`,
`connector_detail`, `channel_rate_limited`, `channel_forbidden`,
`channel_conflict`, `channel_unavailable`, and the four
[integrity codes](#integrity-violations) below.
`status` separates a handler that returned an error (always `500`) from a task
that returned a 5xx *outcome*.

```json
{ "id": "retry_later",
  "condition": { "in": [ { "var": "metadata._orion_errors.0.code" },
                         ["TIMEOUT_ERROR", "IO_ERROR"] ] } }
```

#### Integrity violations

A rule the schema declares — a unique index, a foreign key, a `NOT NULL`, a
`CHECK` — is its own kind of failure: the caller can fix it, retrying will not,
and it is not a fault of the operator's. Each gets a code of its own, so an
endpoint can answer a duplicate submission differently from a dangling
reference:

| Code | Constraint | Status if uncaught |
|---|---|---|
| `integrity_unique` | Unique index or primary key | 409 `CONFLICT` |
| `integrity_foreign_key` | Foreign key | 409 `CONFLICT` |
| `integrity_not_null` | `NOT NULL` | 400 `VALIDATION_ERROR` |
| `integrity_check` | `CHECK` | 400 `VALIDATION_ERROR` |

These come from `db_read`, `db_write`, `data_query` and `data_write` on all
three SQL backends — the classification is the driver's own, so it means the
same thing on SQLite, PostgreSQL and MySQL. Everything else a reached database
refuses stays `FUNCTION_ERROR`: a syntax error, a missing column, a deadlock.

```json
{ "id": "already_submitted",
  "condition": { "==": [ { "var": "metadata._orion_errors.0.code" },
                         "integrity_unique" ] },
  "function": { "name": "map", "input": { "mappings": [
    { "path": "data._orion.response.status", "logic": 409 },
    { "path": "data.error", "logic": "A submission for that release already exists" }
  ] } } }
```

> **Branch on `code`, not `status`.** `status` is the *task's* status, which is
> `500` whenever a handler returned an error — including these. The `409` in
> the table above is what the edge sends when no task catches the failure; it
> never appears in the record.

The driver's own message is not exposed anywhere on this path. It names tables,
columns, index names and often the value that conflicted, so it stays in the
operator-only detail kept on the trace, which means a workflow can tell *what
kind* of rule was violated but not *which* rule. Where an endpoint has to
distinguish two unique indexes on one table, query for the expected case
explicitly before the write and leave the constraint as the backstop for the
race.

Three properties to rely on:

- **No message, ever.** Records carry codes and task ids only. A task error's
  message can contain an upstream URL and response body, which
  [`verbose_errors`](./errors.md#message-sanitization-verbose_errors) exists to
  keep from anonymous callers — a workflow-visible message would route around
  that entirely, since `data` is returned unsanitized.
- **Not caller-supplied.** The key is cleared at every ingress, so an envelope
  cannot pre-seed failures, and it is reset on `channel_call` — a called
  channel reports its own failures, never its caller's.
- **Bounded.** Only the most recent records are kept, so a looping workflow
  with a failing body cannot grow the context without limit.

**`metadata.progress`** is the older, weaker signal, written by dataflow-rs
after every task that *ran* — `{workflow_id, task_id, status_code}`, with
`status_code` `500` on failure. It is a single slot overwritten by every
later task and carries no reason, so it distinguishes "failed" from
"skipped by condition" and nothing more. Prefer `_orion_errors`.

## Conditions

Conditions are [JSONLogic](https://jsonlogic.com) expressions, compiled once
at engine build time. They appear at two levels:

- **Workflow-level `condition`**: decides whether the whole workflow
  *matches* a request. Defaults to `true` (always matches). If multiple active
  workflows are bound to a channel, the first match wins (see
  [Matching](#matching)).
- **Task-level `condition`**: decides whether *that task* runs within a
  matched workflow. Use it for branching inside a pipeline.

The complete operator set — core JSONLogic plus the date, string, array, math,
and control extensions — is cataloged in the
[Expression Reference](./expressions.md). That page also documents the
silent-failure edges, including misspelled operators inside `map` mappings.

## Loop

A workflow with a `loop` runs its **whole task list once per sweep** instead of
once in total. It is how one workflow processes each element of an array —
calling a connector per element, which a JSONLogic `map` cannot do.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `max` | integer | **yes** | — | Upper bound on sweeps. Half-open: `init: 0, max: 10` yields counter values `0`–`9` |
| `counter` | string | no | — | `temp_data` field holding the count — `"i"` is `temp_data.i`, and dots nest (`"cursor.index"`). Omit to bound the loop without exposing the count |
| `init` | integer | no | `0` | First counter value |
| `increment` | integer | no | `1` | Added after each sweep. Must be at least `1`, so the counter always advances |

Per sweep, in order: the engine writes the counter, checks `counter < max`,
re-evaluates the **workflow** `condition`, and runs the task list. The sweep
happens only if both the bound and the condition hold.

Stopping early is a [`filter`](./functions.md#filter) task with `on_reject:
"halt"`, which ends the whole loop rather than the current sweep:

```json
{
  "id": "notify-each-recipient",
  "name": "Notify each recipient",
  "condition": true,
  "loop": { "counter": "i", "max": 500 },
  "tasks": [
    {
      "id": "parse",
      "name": "Parse",
      "function": { "name": "parse_json", "input": { "source": "payload", "target": "req" } }
    },
    {
      "id": "more",
      "name": "Stop once every recipient is done",
      "function": {
        "name": "filter",
        "input": {
          "condition": { "<": [{ "var": "temp_data.i" }, { "var": "data.req.count" }] },
          "on_reject": "halt"
        }
      }
    },
    {
      "id": "send",
      "name": "Send one notification",
      "function": {
        "name": "http_call",
        "input": {
          "connector": "notifier",
          "method": "POST",
          "path": "/send",
          "body_logic": { "val": ["data", "req", "recipients", { "val": ["temp_data", "i"] }] }
        }
      }
    }
  ]
}
```

> [!WARNING]
> **Do not put the break in the workflow `condition`.** It looks like the
> natural home for it and it does not work: `data` [starts empty](#the-data-context),
> so `{"<": [{"var": "temp_data.i"}, {"var": "data.req.count"}]}` as a workflow
> condition is false on sweep 0 — before any `parse_json` has run, and the
> loop never starts at all. Inside the body the parse has already happened,
> which is why the break belongs there.
>
> A workflow condition can still end a loop, but only over what is readable
> *before* the sweep's own tasks run: `metadata`, or `data` a previous workflow
> populated.

So the two bounds do different jobs. `max` is structural — the loop cannot
outrun it, whatever the body does. The `filter` is what ends it early, once the
real work is done. Reaching `max` is normal completion, not an error: `max` is
always something you wrote, so hitting it is the bound you asked for.

> [!NOTE]
> `max` is capped by [`engine.max_loop_iterations`](./configuration.md)
> (default `10000`), and a workflow exceeding it is refused with `400` at write
> time rather than at activation. A sweep can call a connector, so an
> unbounded loop is a request that holds pool connections until the channel
> timeout fires.

Each sweep's steps appear in the [execution trace](../operate/traces.md)
tagged with the iteration they belong to, so a trace of ten sweeps reads as ten
groups rather than one flat list.

## Error handling

By default the pipeline **halts** on the first task that errors, and the error
is returned to the caller. Set `continue_on_error: true` on the workflow to
keep running later tasks and collect errors instead, or on a single task
to make just that step non-fatal. A run that continues records each failure's
code at [`metadata._orion_errors`](#branching-on-a-failure), so a later task
can branch on *why* the step failed rather than only on whether it did.

**"Errors" here means a handler failure or a status of `500` or above.** A task
that records a `4xx`, which is what a failing `validation` rule does — logs a
warning and the pipeline carries on, whatever `continue_on_error` says. That is
deliberate: a `4xx` is the task reporting on its input, not the engine failing
to run it. It is also the one thing about this field worth knowing before you
reach for it after a `validation`, so:

| To stop on | Use |
|---|---|
| A handler failure or `5xx` | `continue_on_error: false` (the default) |
| This task's own failure, `4xx` included | [`halt_on: "failure"`](#halting-on-failure) |
| A condition you write yourself | [`filter`](./functions.md#filter) — `on_reject: "halt"` stops the workflow, `"skip"` skips only that task |
| Reaching this point at all | [`terminal: true`](#terminal-steps) |

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

- **draft**: editable; not served. Only **one draft per `workflow_id`** may
  exist at a time. Creating a workflow starts it as a draft.
- **active**: served; **immutable**. To change an active workflow, create a
  new draft version, edit it, and activate it.
- **archived**: retired, and kept. An archived version is a rollback *source*:
  its content is what you copy into a new draft and activate. Nothing
  reactivates an archived version in place. See
  [Version & Roll Out Changes › Roll back](../build/versioning.md#roll-back).

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
behaviour.

### Rollout

`rollout_percentage` (1–100) enables canary releases across versions.
Activating a new version at `25` directs about 25% of traffic to it; the
remainder goes to the previously active version. Traffic is bucketed by a
stable hash of the request, so a given caller routes consistently. Promote by
raising the percentage to `100`, which archives the older active version. Roll
back instantly by re-activating a previous version. Both moves go through the
status and rollout endpoints. See
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

- [Function Reference](./functions.md): the `input` schema for every built-in
  function.
- [Expression Reference](./expressions.md): every operator conditions and
  mappings can use, with the silent-failure edges.
- [Channel Configuration](./channel-config.md): how a channel binds to a
  workflow and shapes its response.
- [Admin API](./admin-api.md#lifecycle): the endpoints that create, version,
  and activate workflows.
