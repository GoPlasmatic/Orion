<!-- description: The Orion workflow object in full: fields, tasks, task groups, terminal steps, the shared data context, request metadata, loops, and error handling. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# Workflow definition


A workflow is a JSON-defined pipeline of tasks. A [channel](../concepts/channels.md) links to a workflow by `workflow_id`. When a request arrives, Orion matches a deployed workflow, runs its tasks in order, and returns the resulting data context.

[Workflows](../concepts/workflows.md) is the concept and [Author a workflow](../guides/author/workflows.md) the guide. Per-task `function.input` schemas are in [Task functions](./functions/index.md), and the operator catalogue in [Expression language](./expressions.md).

## Synopsis

Write this shape as a file in a definition set. [`orion-cli compile`](./cli/orion-cli/compile.md) turns it into a workflow definition, and a deployment serves it ([Deployment endpoints](./admin-api/deployments.md#definitions)).

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
| `continue_on_error` | bool | no | `false` | If `true`, a task failure (a handler error or `5xx`) that reaches the workflow ends this workflow without failing the request: the error is recorded and the next matching workflow runs. It does not run this workflow's later tasks; a task's own `on_failure: "continue"` does that — see [Error handling](#error-handling) |
`compile` moves `workflow_id` onto the deployment member, so the stored definition holds no identity. The member also supplies the revision.

A definition that would not serve is refused with `400` and a field-level error envelope. See
[Errors](./errors.md) for the format.

## Tasks

Each entry in `tasks` is a single step in the pipeline:

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `id` | string | **yes** | — | Unique within the workflow; used in tracing |
| `name` | string | no | — | Human-readable label |
| `function` | object | **yes** | — | The function to run; the `function` object table follows |
| `condition` | JSONLogic | no | — | If present and falsy, this task is skipped |
| `on_failure` | string | no | `"fail"` | What a handler error or `5xx` from **this** task does: `"fail"` passes it to the workflow, `"halt"` ends the workflow, `"continue"` records it and runs the next task — see [Error handling](#error-handling) |
| `terminal` | bool | no | `false` | End the workflow after this step runs. About **position, not outcome** — see [Terminal steps](#terminal-steps) |
| `on_rejection` | string | no | `"continue"` | What a `4xx` from **this** task does, which is what a failing `validation` rule records: `"continue"` records it and runs the next task, `"halt"` ends the workflow. About **outcome, not position** — see [Halting on a rejection](#halting-on-a-rejection). Tasks only; a group carrying it is refused |
| `for_each` | object | no | — | Run the task's function once per element of an array — see [Fan-out](#fan-out) |

The `function` object names a [built-in function](./functions/index.md) and supplies
its `input`:

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `name` | string | **yes** | — | One of the [built-in functions](./functions/index.md) |
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

An element of `tasks` carrying its own `tasks` key is a task group rather than a task. It is one condition guarding a contiguous run of steps, instead of the same condition repeated on each.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `id` | string | **yes** | — | Unique within the workflow. Groups share **one id namespace with tasks** — a group id colliding with a task id is refused at create |
| `tasks` | array | **yes** | — | The steps in the span. Must be non-empty: a condition guarding nothing is refused |
| `condition` | JSONLogic | no | `true` | Evaluated **once, on entry**. A falsy result skips the whole span without evaluating the members' own conditions |
| `terminal` | bool | no | `false` | End the workflow after the whole span runs |
| `name` | string | no | — | Human-readable label. Optional here, unlike on a task, where it is required |
| `description` | string | no | — | What the span covers |

A group has no `function`. That is exactly what distinguishes the two shapes, and the rule the parser applies is presence of a `tasks` key, nothing else. An element carrying neither `function` nor `tasks` is reported as a broken task, not an empty group.

Groups nest, up to 8 levels deep. Deeper than that is refused at create. It is a generated-JSON accident rather than an authored control-flow shape, and the engine does not build it.

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

Everything downstream of the definition sees the flattened list. A trace, an audit trail and `metadata.progress` report the member tasks, not the group. The group is a property of the definition, not a step that runs.

### Terminal steps

`terminal: true` ends the workflow after the step runs; on a group, after the whole span. With a condition, that is the guard clause: *if this, answer and stop*. Without it, every later task has to restate the negation of every earlier exit, and those conditions grow with each branch added.

It is about **position, not outcome**:

- A falsy `condition` does not halt — the step did not run.
- A skipped task does not halt.
- A task that *failed* under `on_failure: "continue"` **does** halt, because
  the author said nothing runs after this one.

**Since:** Orion 1.2. Task groups and `terminal` need dataflow-rs 3.6, which Orion 1.2.0 ships. A definition using a group fails to load on an older engine, loudly; a bare `terminal: true` is silently ignored there and every later task runs. Gate on the server version if you deploy definitions to instances you do not control.

### Halting on a rejection

`on_rejection: "halt"` ends the workflow when the task records a `4xx`. That covers a `validation` rule that did not pass and any task returning that range. A handler error or `5xx` is the task's `on_failure`, which by default fails the workflow anyway. A success falls through.

It is the other axis from `terminal`: position versus outcome. The two compose
by *or*, so `terminal` stays strictly stronger and no combination contradicts.

```json
{ "id": "check_state", "name": "Check the state token", "on_rejection": "halt",
  "function": { "name": "validation", "input": { "rules": [
    { "logic": { "==": [{ "var": "metadata.query.state" },
                        { "var": "metadata.cookies.oauth_state" }] },
      "message": "state mismatch" } ] } } }
```

Without it, a failing `validation` does not stop anything. A failed rule records status `400`, and a task's `on_rejection` is `continue` unless you say otherwise; `on_failure` governs `5xx` and handler errors only. A `validation` followed by unguarded tasks therefore records an error and proceeds exactly as if it had passed. That is how a check that reads correct ships doing nothing.

Collecting every rule failure and carrying on is a legitimate shape, and the
default keeps it. When you meant a gate, say so:

| You want | Write |
|---|---|
| Stop, keeping the task's own status | `"on_rejection": "halt"` on the task |
| Stop, and answer with a chosen status and body | a later task with a `condition` on the failure and `terminal: true` |
| Stop, with no body | a [`filter`](./functions/filter.md) task (records `299`) |

`orion-cli lint` reports a `validation` that is not acting as a gate and has
nothing guarded after it (`engine.unguarded_validation`). The silent version
does not have to be found in production.

`on_rejection` and `on_failure` belong to a task. A group has no outcome of its own, so one carrying either is refused at parse, loudly, rather than as a guard that never fires.

The 1.x spellings are no longer read. A task's `halt_on` or `continue_on_error` is refused by `lint`, `compile` and the upload. Each refusal names the key to write instead:

| 1.x | Write |
|---|---|
| `"halt_on": "failure"` | `"on_rejection": "halt"` |
| `"continue_on_error": true` | `"on_failure": "continue", "on_rejection": "continue"` |
| both | `"on_failure": "halt", "on_rejection": "halt"` |
| `"halt_on": "never"`, `"continue_on_error": false` | nothing: these are the defaults |
| `"function": { "name": "validate" }` | `"name": "validation"` |

The pre-1.0 input aliases are no longer read either: `http_call`'s `path_logic`, `body_logic` and `response_path`, and `channel_call`'s `channel_logic`, `data_logic` and `response_path` are unknown keys. Write `path`, `body`, `output`, `channel` and `data`.

### Fan-out

A task with `for_each` runs its function once per element of an array, and collects the results:

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `over` | JSONLogic | **yes** | — | Evaluated once, after the task's `condition`. Must yield an array; anything else, `null` included, fails the task. An empty array runs no call |
| `as` | string | **yes** | — | `temp_data` field holding the element in each call's view. Its index is at `temp_data.<as>_index` |
| `max_concurrency` | integer | no | `1` | Calls in flight at once. A timing knob only: the message comes out the same at every setting |
| `collect` | string | no | — | Context path each call writes its result to. Requires `into` |
| `into` | string | no | — | Context path that receives the results as an array, in element order. Requires `collect` |

```json
{
  "id": "quote",
  "name": "One quote per supplier",
  "on_failure": "continue",
  "for_each": {
    "over": { "var": "data.suppliers" },
    "as": "s",
    "max_concurrency": 8,
    "collect": "temp_data.quote",
    "into": "temp_data.quotes"
  },
  "function": {
    "name": "http_call",
    "input": {
      "connector": "supplier-api",
      "method": "GET",
      "path": { "cat": ["/quotes/", { "var": "temp_data.s.id" }] },
      "output": "temp_data.quote"
    }
  }
}
```

Every call runs against its own copy of the message, taken before the first call, so no call sees another's writes. The calls' writes, errors and audit entries are then folded back in element order. The element bindings are not left behind. An element that failed, never ran, or wrote nothing at `collect` leaves `null` at its index in `into`.

The task's `on_failure` applies per element. A call that fails the task stops new calls from starting, and the elements after it contribute nothing. `terminal` and `on_rejection` apply once, after the whole fan-out.

Any Orion function can fan out. The engine's inline built-ins (`map`, `validation`, `filter`, `log`, `parse_json`, `parse_xml`, `publish_json`, `publish_xml`) cannot: a per-element transform is JSONLogic's `map`. A key `for_each` does not define is refused at write time, because the engine would ignore it. A misspelt `max_concurency` would otherwise run one call at a time without a word.

**Since:** Orion 1.9.1, which ships dataflow-rs 3.14.

## The data context

Tasks share a single JSON document, the **data context**. Its top level holds
exactly three areas your JSONLogic can read:

| Area | Starts as | Purpose |
|------|-----------|---------|
| `data` | a copy of the payload | The working document. Tasks read and write `data.*`. For a sync channel, the final `data` object is the response body |
| `metadata` | ingress-stamped | Request context — channel name, method, headers, and params (see [Request metadata](#request-metadata)) |
| `temp_data` | `{}` | Scratch space for intermediate values. Not part of the response |

`data` starts as a copy of the request payload.
Caller fields are readable as `data.<field>` from the first condition, including the workflow's condition. The original payload lives outside the JSONLogic context, so `{"var": "payload.x"}` resolves to nothing.
Tasks do not change that original payload. Two consequences follow from the seed:

- **The response carries the caller's fields.** The final `data` is the payload plus whatever the tasks wrote, so a field the workflow should not echo has to be removed or overwritten.
- **An unmatched request answers with its own input.** When no workflow matches, nothing changes `data`, and the response is the payload.

Use `parse_json` with `source: "payload"` to place the whole payload under one `target` key, as below.
Use `parse_xml` to make an XML body readable.

```json
{
  "tasks": [
    { "id": "parse",   "name": "Parse",   "function": { "name": "parse_json", "input": { "source": "payload", "target": "order" } } },
    { "id": "process", "name": "Process", "condition": { ">": [{ "var": "data.order.total" }, 100] }, "function": { "name": "map", "input": { "mappings": [] } } }
  ]
}
```

### Output paths

Every connector function writes its result to a dotted path named `output`; a `map` task uses its mapping `path` instead. Orion creates the path inside the context if it does not exist.

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
| `cookies` | always, when the channel opts in | The cookies named by [`request.cookies_to_metadata`](./channel-config/request.md#reading-request-cookies). Absent block → the key is stripped, so a caller cannot supply it |
| `vars` | always, when the instance declares any | The [`[vars]`](./configuration/vars-and-secrets.md) config section verbatim. No `[vars]` → the key is stripped, so a caller cannot supply it |
| `auth` | only when a [`jwt`](./channel-config/auth.md) token verified | `{"claims": …}`, the verified claims. No verified token → the key is stripped, so a caller cannot supply it |

`oauth` (the [sign-in](./channel-config/oauth2_login.md) grant) and `trigger` (a [cron](./channel-config/cron.md) occurrence) are stripped from the caller's object the same way. Only their guard or the cron worker writes them.

Orion stamps nothing else on the HTTP path: no client IP, no request path, no trace id. A channel in [`request.body_mode = "payload"`](./channel-config/request.md) takes no caller `metadata` at all; the object is server-stamped keys only.

**`channel_call`.** The called workflow inherits the parent's `metadata`.
Orion overwrites `channel` with the target channel's name and adds two
tracking keys: `_orion_call_depth` (nesting depth) and `_orion_call_chain`
(the array of channel names traversed).

### The `_orion` namespace

Orion reserves these keys and no others:

| Key | Where | Meaning |
|-----|-------|---------|
| `data._orion.response` | context | Shaped channels write the response `status`, `headers`, `cookies` and `body` here; Orion drains it before replying. See [Channel Configuration](./channel-config/response.md) |
| `_orion.profile` | response envelope | Per-task timings when profiling is requested. Never in the context. See the [Data API](./data-api.md) |
| `metadata._orion_call_depth` | context | `channel_call` nesting depth |
| `metadata._orion_call_chain` | context | Channel names traversed by nested `channel_call`s |
| `metadata._orion_errors` | context | Codes of tasks that failed in this run — see [Branching on a failure](#branching-on-a-failure) |

No other key or prefix in the context is reserved. Six plain metadata keys are still platform-owned, force-stamped or stripped at every ingress. A caller cannot supply `channel`, `cookies`, `vars`, `auth`, `oauth` or `trigger` (see [Request metadata](#request-metadata)). `metadata.progress` is engine-owned too, but it belongs to dataflow-rs rather than Orion; see [Branching on a failure](#branching-on-a-failure).

### Branching on a failure

When a task fails and the run continues (`on_failure: "continue"`), the engine appends a record to `metadata._orion_errors`. A later task can answer differently depending on why the step failed:

```json
{ "task_id": "charge_payment", "workflow_id": "place-order",
  "code": "TIMEOUT_ERROR", "status": 500 }
```

`code` is a closed vocabulary. The engine's kinds are `VALIDATION_ERROR`, `WORKFLOW_ERROR`,
`TASK_ERROR`, `FUNCTION_NOT_FOUND`, `FUNCTION_ERROR`, `LOGIC_ERROR`,
`HTTP_ERROR`, `TIMEOUT_ERROR`, `IO_ERROR`, `DESERIALIZATION_ERROR` and
`UNKNOWN_ERROR`. The service kinds Orion mints are `circuit_open`,
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

A rule the schema declares, such as a unique index, a foreign key, a `NOT NULL` or a `CHECK`, is its own kind of failure. The caller can fix it, retrying does not, and it is not a fault of the operator's. Each gets a code of its own, so an endpoint can answer a duplicate submission differently from a dangling reference:

| Code | Constraint | Status if uncaught |
|---|---|---|
| `integrity_unique` | Unique index or primary key | 409 `CONFLICT` |
| `integrity_foreign_key` | Foreign key | 409 `CONFLICT` |
| `integrity_not_null` | `NOT NULL` | 400 `VALIDATION_ERROR` |
| `integrity_check` | `CHECK` | 400 `VALIDATION_ERROR` |

These come from `db_read`, `db_write`, `data_query` and `data_write` on all three SQL backends. The classification is the driver's own, so it means the same thing on SQLite, PostgreSQL and MySQL. Everything else a reached database refuses stays `FUNCTION_ERROR`: a syntax error, a missing column, a deadlock.

```json
{ "id": "already_submitted",
  "condition": { "==": [ { "var": "metadata._orion_errors.0.code" },
                         "integrity_unique" ] },
  "function": { "name": "map", "input": { "mappings": [
    { "path": "data._orion.response.status", "logic": 409 },
    { "path": "data.error", "logic": "A submission for that release already exists" }
  ] } } }
```

Branch on `code`, not `status`. `status` is the task's status, which is `500` whenever a handler returned an error, including these. The `409` in the table is what the edge sends when no task catches the failure; it never appears in the record.

The driver's own message is not exposed anywhere on this path. It names tables, columns, index names and often the value that conflicted, so it stays in the operator-only detail kept on the trace. A workflow can tell what kind of rule was violated but not which rule. Where an endpoint has to distinguish two unique indexes on one table, query for the expected case explicitly before the write. Leave the constraint as the backstop for the race.

Three properties to rely on:

- **No message, ever.** Records carry codes and task ids only. A task error's message can contain an upstream URL and response body, which [`verbose_errors`](./errors.md#message-sanitization-verbose_errors) exists to keep from anonymous callers. A workflow-visible message would route around that entirely, since `data` is returned unsanitized.
- **Not caller-supplied.** The key is cleared at every ingress, so an envelope cannot pre-seed failures. It is reset on `channel_call`, so a called channel reports its own failures, never its caller's.
- **Bounded.** Only the most recent records are kept, so a looping workflow
  with a failing body cannot grow the context without limit.

`metadata.progress` is the older, weaker signal, written by dataflow-rs after every task that ran: `{workflow_id, task_id, status_code}`, with `status_code` `500` on failure. It is a single slot overwritten by every later task and carries no reason. It distinguishes "failed" from "skipped by condition" and nothing more. Prefer `_orion_errors`.

## Conditions

Conditions are [JSONLogic](https://jsonlogic.com) expressions, compiled once
at engine build time. They appear at two levels:

- **Workflow-level `condition`**: decides whether the whole workflow matches a request. Defaults to `true`, which always matches. If multiple active workflows are bound to a channel, the first match wins (see [Matching](#matching)).
- **Task-level `condition`**: decides whether *that task* runs within a
  matched workflow. Use it for branching inside a pipeline.

The complete operator set, core JSONLogic plus the date, string, array, math and control extensions, is catalogued in [Expression language](./expressions.md). That page also documents the silent-failure edges, including misspelled operators inside `map` mappings.

## Loop

A workflow with a `loop` runs its whole task list once per sweep instead of once in total. It is how one workflow processes each element of an array, calling a connector per element, which a JSONLogic `map` cannot do.

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `max` | integer | **yes** | — | Upper bound on sweeps. Half-open: `init: 0, max: 10` yields counter values `0`–`9` |
| `counter` | string | no | — | `temp_data` field holding the count — `"i"` is `temp_data.i`, and dots nest (`"cursor.index"`). Omit to bound the loop without exposing the count |
| `init` | integer | no | `0` | First counter value |
| `increment` | integer | no | `1` | Added after each sweep. Must be at least `1`, so the counter always advances |
| `over` | JSONLogic | no | — | An array to iterate. Evaluated once, after `setup`; see [Iterating an array](#iterating-an-array) |
| `as` | string | no | — | `temp_data` field holding the current element of `over`. Requires `over` |
| `scratch` | string | no | — | `temp_data` field reset to `{}` at the start of every sweep, before the condition, so per-element state cannot leak into the next sweep |
| `setup` | array of steps | no | — | Steps run once, before the first sweep; see [Setup](#setup) |

Per sweep, in order, the engine writes the counter, checks `counter < max`, re-evaluates the workflow `condition`, and runs the task list. The sweep happens only if both the bound and the condition hold.

Stopping early is a [`filter`](./functions/filter.md) task with `on_reject:
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
          "body": { "val": ["data", "req", "recipients", { "val": ["temp_data", "i"] }] }
        }
      }
    }
  ]
}
```

> [!WARNING]
> Do not put this break in the workflow `condition`. It looks like the natural home for it and it does not work. `data.req` is written by the body's own `parse_json`, so `{"<": [{"var": "temp_data.i"}, {"var": "data.req.count"}]}` as a workflow condition is false on sweep 0, before the parse has run, and the loop never starts. Inside the body the parse has already happened, which is why the break belongs there. A workflow condition can still end a loop, but only over what is readable before the sweep's own tasks run: `metadata`, the payload's own fields (`data` [starts as a copy of it](#the-data-context), so `data.count` would do), or `data` a previous workflow populated.

The two bounds do different jobs. `max` is structural: the loop cannot outrun it, whatever the body does. The `filter` is what ends it early, once the real work is done. Reaching `max` is normal completion, not an error, because `max` is always something you wrote.

`max` is capped by [`engine.max_loop_iterations`](./configuration/index.md) (default `10000`). A workflow exceeding it is refused with `400` at upload, not when it would serve. A sweep can call a connector, so an unbounded loop is a request that holds pool connections until the channel timeout fires.

Each sweep's steps appear in the [execution trace](../operate/run/traces.md) tagged with the iteration they belong to. A trace of ten sweeps reads as ten groups rather than one flat list.

A key `loop` does not define is refused at write time, because the engine would ignore it.

### Setup

`setup` is a list of steps run once, before the first sweep, in the same grammar as `tasks`. Task groups are allowed, and a `terminal` step ends the workflow before any sweep. The workflow `condition` gates setup and the loop alike. Setup steps share the body's id namespace, so an id used in both is refused. Everything Orion checks about a step it checks about a setup step. That covers its function and input at create, and the connectors it names at activation and on a connector rename or delete. Every `lint` and `clippy` rule applies too.

### Iterating an array

With `over`, the counter indexes the array: sweep `k` holds `over[k]` at `temp_data.<as>`. The loop stops at `max` or at the array's end, whichever comes first. `init` is then a starting offset, `increment` a stride, and `init` must be at least `0`. `over` must yield an array; anything else, `null` included, is a workflow error naming `loop.over`, and an empty array runs no sweep. `max` still applies.

**Since:** Orion 1.9.1 for `setup`, `over`, `as` and `scratch`, which need dataflow-rs 3.14.

## Error handling

By default the pipeline halts on the first task that errors, and the error is returned to the caller. Set `"on_failure": "continue"` on a task to make that step non-fatal: its error is recorded and the next task runs. A workflow's `on_failure: continue` applies at the workflow boundary.
A failure ends that workflow and records its error, then the next matching workflow runs.
Later tasks in the failed workflow do not run. A run that continues records each failure's code at [`metadata._orion_errors`](#branching-on-a-failure). A later task can branch on why the step failed rather than only on whether it did.

"Errors" here means a handler failure or a status of `500` or above. A failing `validation` rule records a `4xx` and logs a warning.
The pipeline continues unless the task's `on_rejection` is `halt`. That is deliberate: a `4xx` is the task reporting on its input, not the engine failing to run it. It is also the one thing about this field worth knowing before you reach for it after a `validation`:

| To stop on | Use |
|---|---|
| A handler failure or `5xx` | `on_failure: "fail"` (the default) |
| This task's own rejection, a `4xx` | [`on_rejection: "halt"`](#halting-on-a-rejection) |
| A condition you write yourself | [`filter`](./functions/filter.md) — `on_reject: "halt"` stops the workflow, `"skip"` skips only that task |
| Reaching this point at all | [`terminal: true`](#terminal-steps) |

For async channels, a task failure routes the trace to the dead letter queue
for automatic retry.

## Serving

### Matching

When a channel resolves its workflows, Orion evaluates its deployed workflows in descending `priority` and runs the first whose `condition` is truthy. Give a catch-all workflow a low priority and specific ones a higher priority to layer behaviour.

### Rollout

A workflow definition carries no rollout. The deployment controls traffic splits between workflow revisions.
A channel member's `rollouts` assign each revision a share of 100 buckets. See [Deployments](../concepts/deployments.md#boundaries).

## Examples

### A complete workflow

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

- [Workflows](../concepts/workflows.md): the concept behind this contract.
- [Author a workflow](../guides/author/workflows.md): the guide that applies it.
- [Task functions](./functions/index.md): the `input` schema for every built-in function.
- [Expression language](./expressions.md): every operator conditions and mappings can use, with the silent-failure edges.
- [Deployment endpoints](./admin-api/deployments.md): storing a workflow definition, testing it, and deploying it.
