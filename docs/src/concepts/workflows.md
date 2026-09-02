<!-- description: A workflow is an ordered pipeline of tasks in JSON, versioned like source code and executed directly by the engine — no build step and no binary. -->
# Workflows

A **workflow** is an ordered pipeline of tasks — the business logic a channel
runs. It is a JSON document, versioned like source code, and it never becomes a
binary: the engine executes it directly.

```orion-diagram
{
  "direction": "LR",
  "groups": [ { "id": "wf", "label": "order-processing" } ],
  "nodes": [
    { "id": "p", "label": "parse_json", "sublabel": "payload → data.order", "type": "service", "group": "wf" },
    { "id": "v", "label": "validation", "sublabel": "required fields", "type": "service", "group": "wf" },
    { "id": "e", "label": "http_call", "sublabel": "enrich from CRM", "type": "service", "group": "wf" },
    { "id": "m", "label": "map", "sublabel": "compute risk", "type": "service", "group": "wf" },
    { "id": "out", "label": "response", "type": "channel" }
  ],
  "edges": [
    { "from": "p", "to": "v" }, { "from": "v", "to": "e" }, { "from": "e", "to": "m" }, { "from": "m", "to": "out" }
  ]
}
```

## Tasks

A task has an `id`, a `name`, and one `function` with its `input`. Functions are
built in — parsing, mapping, validating, filtering, logging, and the
connector-backed ones that call databases, HTTP APIs, caches and Kafka. You pick
and configure them; you do not write them.

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

Every function's exact `input` is in the [Function Reference](../reference/functions.md),
and connector-backed inputs are schema-validated when you save the workflow —
a typo is a `400` with a field path, not a surprise at 3am.

An element of `tasks` carrying its own `tasks` key is a **task group**: one
condition guarding a contiguous run of tasks, evaluated once on entry. Any step
— task or group — may set `terminal: true` to end the workflow once it has run,
which is how a workflow answers early without every later task restating the
negation of the branch above it. See
[Author Workflows](../build/workflows.md#group-tasks-and-stop-early).

## The data context

Tasks do not pass values to each other. They share one JSON document, the **data
context**, and each task reads and writes paths in it:

- **`data`** — the working document. For a sync channel, the final `data` object
  is the response body.
- **`metadata`** — stamped by the ingress: channel name, method, headers, route
  parameters.
- **`temp_data`** — scratch space that never reaches the response.

One rule catches most beginners: **the raw request payload is not in the
context**. A workflow that reads request data starts with a `parse_json` task,
which lifts the payload into `data`. Without it, conditions referencing `data.*`
evaluate against an empty object and quietly do nothing.

## Conditions

Conditions are [JSONLogic](../reference/expressions.md) expressions, compiled
once when the engine builds. They appear at two levels, and the distinction
matters:

- **Workflow-level `condition`** decides whether this workflow *matches* the
  request at all.
- **Task-level `condition`** decides whether *that task* runs inside a workflow
  that already matched.

Branching inside a pipeline is the second one. Choosing between two pipelines is
the first.

## How a channel picks a version

A channel names a workflow by id, but an id can have many versions, and several
workflows can be bound to one channel. Orion resolves this the same way on every
node:

1. **Only active workflows are considered.** Drafts and archived versions are
   invisible to traffic.
2. **Higher `priority` is evaluated first**, and the first workflow whose
   `condition` is truthy wins. A catch-all with low priority under a specific
   one with high priority is the usual shape.
3. **A rollout percentage splits versions.** Activating a new version at 25
   sends about a quarter of traffic to it and the rest to the previously active
   one. The split is a stable hash of the request, so a given caller lands
   consistently instead of flickering between versions.

## Versioned, not edited

An active workflow is **immutable**. Changing one means creating a new version,
testing it, and activating that — which is also why rolling back is putting
known-good content into a new version rather than a redeploy, and why that
content is guaranteed to be what it was when it last served.
[The Entity Lifecycle](./lifecycle.md) covers the rules;
what matters here is that "edit the running logic" is not an operation Orion
offers, on purpose.

## Errors

By default the pipeline halts on the first task that **errors** — a handler
error or a `5xx` — and the error reaches the caller in the response envelope.
Set `continue_on_error` on the workflow to collect errors and keep going
instead. A task that records a `4xx`, as a failing
[`validation`](../reference/functions.md#validation--validate) rule does, halts
nothing on its own: it is recorded and the pipeline proceeds, unless the task
carries [`halt_on`](../reference/workflows.md#halting-on-failure). For an async
channel, a task failure routes the trace to the dead letter queue for retry.

## Next steps

- [Workflow Schema](../reference/workflows.md) — every field, the data context
  in full, matching and rollout semantics.
- [Task Functions](../reference/functions.md) — what each function does and the
  exact `input` it takes.
- [Expression Language](../reference/expressions.md) — the JSONLogic operator
  catalogue, and the silent-failure edges to avoid.
- [Test & Promote a Service](../getting-started/test-and-promote.md) — run a
  workflow offline before it ever sees traffic.
