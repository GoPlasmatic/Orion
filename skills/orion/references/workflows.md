# Workflows

Read this reference when creating or changing workflow JSON.

## Object and identity

A workflow file requires `workflow_id`, `name`, `condition`, and `tasks`.
Optional authoring fields include `priority`, `tags`, `loop` and
`continue_on_error`. A workflow carries no version, status or rollout.

```json
{
  "workflow_id": "order-processing",
  "name": "Order processing",
  "condition": true,
  "tasks": []
}
```

The file lives in the definition set. `compile` strips `workflow_id` into the
manifest member and stores the rest as a definition named by its digest. A
change to the file is a new digest, so the next apply deploys it as the next
revision of that workflow id. A deployed revision is never edited.

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

`on_rejection: "halt"` is the outcome axis to `terminal`'s position axis: it
ends the workflow when *that step was rejected* — a `4xx`, which includes a
`validation` rule that did not pass. The two compose by `or`, and a step that
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

HTTP metadata may include channel, method, lower-cased headers (credential
headers masked), route
params, query values, and explicitly opted-in cookies. A cron run carries
`metadata.trigger`, written only by the cron worker. `channel_call` inherits metadata
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

Without an override, a task error halts the pipeline. A task's
`on_failure: "continue"` makes that step non-fatal (`"halt"` ends the workflow
instead). The workflow's `continue_on_error: true` ends the failing workflow
without failing the request; its later tasks do not run. The 1.x task keys
`halt_on` and `continue_on_error` are refused.

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

For async HTTP work, an unhandled failure follows trace/DLQ policy.
Inspect the trace rather than assuming the acknowledgement means completion.

## Loops

A workflow loop repeats the whole task list. `max` is required and bounded by
server configuration; `init` defaults to zero and `increment` must advance.
An optional `counter` writes beneath `temp_data`.

Do not make a workflow condition depend on data first produced by the loop
body: the condition is checked before the first sweep. Put an early exit inside
the body with a `filter` using `on_reject: "halt"`. Reaching `max` is normal
completion.

## Matching and revision selection

A channel binds to one workflow id through its member's `requires`. Among the
deployed workflows a channel resolves, higher `priority` matches first, then
the `condition` must be truthy.

A deployment normally carries one revision per workflow. A traffic split is a
hand-written manifest: both workflow revisions are members, and the channel
member's `rollouts` intervals partition buckets `0..100` between them. See
the traffic-split section of [cli.md](cli.md). `compile` never produces one.

HTTP bucketing can be sticky on the header named by
`engine.rollout_sticky_header`, otherwise the forwarded client IP, with a
random bucket per request when neither is present. `channel_call` preserves
the parent's bucket.

Rollback rolls forward. `orion-cli deployments rollback` commits an earlier
revision's manifest as a new revision. Applying an older commit of the
definition set has the same effect.

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
