# Workflows

Read this reference when creating or changing workflow JSON.

## Object and lifecycle

A workflow requires `workflow_id`, `name`, `condition`, and `tasks`.
The server owns version, status, rollout, content hash, and timestamps. Optional
authoring fields include priority, tags, loop configuration, and
`continue_on_error`.

```json
{
  "workflow_id": "order-processing",
  "name": "Order processing",
  "condition": true,
  "tasks": []
}
```

Create produces a draft. Only drafts are editable, and only one draft exists
per workflow ID. Activation makes a version immutable. Archive retires active
versions; it does not reveal or reactivate a predecessor.

## Steps, groups, and terminal behavior

A task step contains an ID, display name, optional condition/error behavior, and
one function. IDs must be unique across the authored tree.

A task group is a step whose `tasks` array contains nested steps. Its condition
gates the group. Group error behavior applies to its subtree. Groups may nest
only to the engine's supported maximum depth; lint reports violations.

```json
{
  "id": "premium-path",
  "name": "Premium path",
  "condition": { "==": [{ "var": "data.customer.tier" }, "premium"] },
  "tasks": [
    {
      "id": "price",
      "name": "Calculate price",
      "function": { "name": "map", "input": { "mappings": [] } }
    }
  ]
}
```

`terminal: true` on any step ends the workflow after that step or group
completes. Use it for explicit successful exits. Use a halting `filter` when
termination depends on a predicate evaluated as a task.

`halt_on: "failure"` is the outcome axis to `terminal`'s position axis: it ends
the workflow when *that step failed* — a status of 400 or above, which includes
a `validation` rule that did not pass. The two compose by `or`, and a step that
halts this way keeps its own status on the audit trail rather than the 299 a
halting `filter` records. Reach for it when a check must actually stop the
pipeline; a bare `validation` collects its messages and carries on by design.

## Fragments and shared values

Definition directories may contain shared values referenced by `$from` and
parameterized task fragments invoked with `use` plus `with`.

```json
{ "id": "session", "use": "require-session",
  "with": { "deny_message": "Please sign in" } }
```

Compilation expands fragments and prefixes every contributed ID, including
nested groups. A fragment cannot recursively use another fragment. Single-file
admin endpoints cannot resolve source forms and return `UNCOMPILED_SOURCE`.
Always lint and compile the definition set.

## Context

Every run has three readable roots:

| Root | Purpose |
|---|---|
| `data` | Working document and default sync response data |
| `metadata` | Trusted ingress/channel context and declared vars |
| `temp_data` | Scratch state excluded from the response |

The raw request body is outside that JSONLogic document. Parsing functions read
it as `source: "payload"` and write beneath `data`.

HTTP metadata may include channel, method, lower-cased/masked headers, route
params, query values, and explicitly opted-in cookies. Kafka metadata includes
topic, partition, offset, and a UTF-8 key. `channel_call` inherits metadata
while overwriting the channel identity and adding call-chain fields.

Do not trust caller-provided metadata for authorization. Orion overwrites its
reserved ingress keys, but business metadata remains user input unless a guard
establishes otherwise.

Reserved state includes:

- `data._orion.response` for shaped responses;
- `metadata._orion_errors` for continued task failures;
- `metadata._orion_call_depth` and `metadata._orion_call_chain`;
- response-only profiling data under `_orion.profile`.

Avoid creating your own values in the `_orion` namespace.

## Failure behavior

Without an override, a task error halts the pipeline. A task-level
`continue_on_error: true` makes that step non-fatal; workflow/group settings
apply more broadly.

Continued failures append bounded, sanitized records to
`metadata._orion_errors`, including task ID, workflow ID, stable code, and
status—not backend error messages. Branch on the code. Connector and integrity
codes may be more specific than the generic engine codes.

```json
{
  "id": "recover",
  "name": "Recover from timeout",
  "condition": {
    "in": [
      { "var": "metadata._orion_errors.0.code" },
      ["TIMEOUT_ERROR", "IO_ERROR"]
    ]
  },
  "function": { "name": "map", "input": { "mappings": [] } }
}
```

For async HTTP or Kafka work, an unhandled failure follows trace/DLQ policy.
Inspect the trace rather than assuming the acknowledgement means completion.

## Loops

A workflow loop repeats the whole task list. `max` is required and bounded by
server configuration; `init` defaults to zero and `increment` must advance.
An optional `counter` writes beneath `temp_data`.

Do not make a workflow condition depend on data first produced by the loop
body: the condition is checked before the first sweep. Put an early exit inside
the body with a `filter` using `on_reject: "halt"`. Reaching `max` is normal
completion.

## Matching and version selection

A channel binds to a workflow ID. Among eligible active definitions, higher
priority matches first after conditions and rollout admission are considered.

Canary activation assigns a percentage to the new active version and leaves the
remainder on existing active versions. The active percentages for an ID must
form the partition required by the engine; otherwise bound channels are
quarantined. Use activation `--dry-run` before changing traffic.

HTTP rollout can be sticky using the configured identity header, otherwise the
forwarded client IP, with random-per-request fallback when neither is present.
`channel_call` preserves the parent rollout decision. Kafka is admitted
without an HTTP caller bucket.

Promoting a version fully archives the replaced active version. For rollback,
copy known-good archived content into a new draft, test, and activate it—or
reapply the previous package artifact.

## Testing checklist

Test at least:

- normal and malformed payloads;
- every meaningful condition branch;
- missing/null/empty values;
- continued and unhandled function failures;
- loop zero, one, and maximum-boundary behavior;
- shaped response control data when the channel uses it;
- dependency stubs and their output paths.

Inspect task states and intermediate context in the trace, not only the final
response.
