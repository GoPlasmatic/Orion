<!-- description: A workflow is an ordered pipeline of tasks in JSON, versioned like source code and executed directly by the engine — no build step and no binary. -->
<!-- type: concept -->
<!-- last_verified: 2026-09-14 -->

# Workflows

A *workflow* is an ordered pipeline of tasks: the business logic a channel runs. It is a JSON document, versioned like source code, and it never becomes a binary. The engine executes it directly.

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

A task has an `id`, a `name`, and one `function` with its `input`. Functions are built in: parsing, mapping, validating, filtering, logging, and the connector-backed ones that call databases, HTTP APIs, caches and Kafka. You pick and configure them; you do not write them.

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

Every function's exact `input` is in [Task functions](../reference/functions/index.md). Connector-backed inputs are schema-validated when you save the workflow, so an invalid field answers `400` with its field path before the workflow reaches production.

An element of `tasks` carrying its own `tasks` key is a *task group*: one condition guarding a contiguous run of tasks, evaluated once on entry. Any step, task or group, may set `terminal: true` to end the workflow once it has run. That is how a workflow answers early without every later task restating the negation of the branch above it. See [Group tasks and stop early](../guides/author/workflows.md#group-tasks-and-stop-early).

## The data context

Tasks do not pass values to each other. They share one JSON document, the *data context*, and each task reads and writes paths in it:

- **`data`**: the working document. For a sync channel, the final `data` object is the response body.
- **`metadata`**: stamped by the ingress: channel name, method, headers, route parameters.
- **`temp_data`**: scratch space that never reaches the response.

One rule catches most beginners: the raw request payload is not in the context. A workflow that reads request data starts with a `parse_json` task, which lifts the payload into `data`. Without it, conditions referencing `data.*` evaluate against an empty object and quietly do nothing.

## Conditions

Conditions are [JSONLogic](../reference/expressions.md) expressions, compiled once when the engine builds. They appear at two levels, and the distinction matters:

- A **workflow-level `condition`** decides whether this workflow matches the request at all.
- A **task-level `condition`** decides whether that task runs, inside a workflow that already matched.

Branching inside a pipeline is the second one. Choosing between two pipelines is the first.

## How a channel picks a version

A channel names a workflow by id, but an id can have many versions, and several workflows can be bound to one channel. Orion resolves this the same way on every node:

1. **Only active workflows are considered.** Drafts and archived versions are invisible to traffic.
2. **Higher `priority` is evaluated first**, and the first workflow whose `condition` is truthy wins. A catch-all with low priority under a specific one with high priority is the usual shape.
3. **A rollout percentage splits versions.** Activating a new version at 25 sends about a quarter of traffic to it and the rest to the previously active one. The split is a stable hash of the request, so a given caller lands consistently instead of flickering between versions.

## Versioned, not edited

An active workflow is immutable. Changing one means creating a new version, testing it, and activating that. Rolling back is putting known-good content into a new version rather than a redeploy. That content is guaranteed to be what it was when it last served. [The entity lifecycle](./lifecycle.md) covers the rules. What matters here is that "edit the running logic" is not an operation Orion offers, on purpose.

## Errors

By default the pipeline halts on the first task that errors, meaning a handler error or a `5xx`. The error reaches the caller in the response envelope. Set `continue_on_error` on the workflow to collect errors and keep going instead. A task that records a `4xx`, as a failing [`validation`](../reference/functions/validation.md) rule does, halts nothing on its own. It is recorded and the pipeline proceeds, unless the task carries [`halt_on`](../reference/workflows.md#halting-on-failure). On an async channel, a task failure routes the trace to the dead-letter queue for retry.

## Next steps

- [Workflow definition](../reference/workflows.md): every field, the data context in full, and the matching and rollout semantics.
- [Task functions](../reference/functions/index.md): what each function does and the exact `input` it takes.
- [Expression language](../reference/expressions.md): the JSONLogic operator catalogue, and the silent-failure edges to avoid.
- [Author a workflow](../guides/author/workflows.md): the how-to layer, from a first task to groups and loops.
- [Test and promote a service](../get-started/tutorials/test-and-promote.md): run a workflow offline before it sees traffic.
