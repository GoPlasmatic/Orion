# Author Workflows

A workflow is a JSON document listing tasks to run in order. This page is how to
write one: how request data reaches your logic, how to branch, and how errors
come back out.

## Start with parse, always

The raw request payload is **not** in the expression context. A workflow that
reads request data begins with `parse_json`, which lifts the payload into the
data context under a name you choose:

```json
{ "id": "parse", "name": "Parse payload",
  "function": { "name": "parse_json", "input": { "source": "payload", "target": "order" } } }
```

Everything downstream then reads `data.order.*`.

> [!WARNING]
> Skip this and nothing errors. `{"var": "payload.total"}` resolves to nothing,
> conditions referencing `data.*` evaluate against an empty object, every
> conditional task silently skips, and the response comes back empty. It is the
> single most common way a first workflow "does nothing".

## Shape the data with `map`

`map` writes values into the context. Each mapping has a `path` to write and a
`logic` producing the value — a literal, or a JSONLogic expression over what is
already there:

```json
{ "id": "flag", "name": "Flag order",
  "function": { "name": "map", "input": { "mappings": [
    { "path": "data.order.flagged", "logic": true },
    { "path": "data.order.alert",
      "logic": { "cat": ["High-value order: $", { "var": "data.order.total" }] } }
  ]}}}
```

Paths are created if they do not exist. Order matters: a mapping can read what
an earlier task wrote.

> [!WARNING]
> **A misspelled operator is not an error.** `{"catt": [...]}` is not a
> mis-typed `cat` — it is a literal object, and it lands at the target path
> verbatim. Inside a condition, that literal is truthy, so the condition always
> fires. Check names against
> [Expression Language](../reference/expressions.md) and dry-run before you
> activate.

## Branch with conditions

Conditions appear at two levels, and picking the wrong one is a common mistake:

| Level | Decides | Use it for |
|---|---|---|
| Workflow `condition` | Whether this workflow matches the request at all | Choosing between two pipelines for one channel |
| Task `condition` | Whether that task runs inside a matched workflow | Branching inside one pipeline |

Most workflows want `"condition": true` at the top and conditions on individual
tasks:

```json
{
  "condition": true,
  "tasks": [
    { "id": "parse", "function": { "name": "parse_json", "input": { "source": "payload", "target": "order" } } },
    { "id": "vip", "condition": { ">=": [{ "var": "data.order.amount" }, 500] },
      "function": { "name": "map", "input": { "mappings": [{ "path": "data.order.tier", "logic": "vip" }] } } },
    { "id": "standard", "condition": { "<": [{ "var": "data.order.amount" }, 500] },
      "function": { "name": "map", "input": { "mappings": [{ "path": "data.order.tier", "logic": "standard" }] } } }
  ]
}
```

Two tasks with mutually exclusive conditions is how you write an if/else. Write
the branches so exactly one fires — overlapping conditions both run, and the
later one wins on any path they share.

## Reach outside the process

Connector-backed tasks call databases, HTTP APIs, caches, and Kafka. They name a
connector rather than a URL, and write their result to an `output` path:

```json
{ "id": "enrich", "name": "Look up the customer",
  "function": { "name": "http_call", "input": {
    "connector": "crm",
    "method": "GET",
    "output": "data.customer"
  }}}
```

Credentials live in the connector, never in the workflow — which is what makes a
workflow safe to commit, review, and let an AI write. See
[Connect Databases & APIs](./connectors.md).

**Choosing a data function:** prefer `data_query` and `data_write`, the portable
dialect. They are parameterized, injection-safe by construction, and render to
SQL, MongoDB, or Elasticsearch from the same envelope. Drop to `db_read` /
`db_write` only for SQL the dialect cannot express.

## Use the scratch space

The context has three areas. `data` is the working document and, for a sync
channel, the response body. `metadata` is stamped by the ingress — channel name,
method, headers, route parameters. `temp_data` is scratch that never reaches the
response.

Put intermediate values in `temp_data` when the caller has no business seeing
them:

```json
{ "path": "temp_data.raw_score", "logic": { "var": "data.risk.score" } }
```

## Decide what a failure does

By default the pipeline halts at the first failing task and the error goes back
to the caller. Set `continue_on_error` to collect errors and keep going:

```json
{ "workflow_id": "order-processing", "continue_on_error": true, "tasks": [ "..." ] }
```

The response then carries `"status": "ok"` **and** a non-empty `errors` array —
so anything relying on this must inspect `errors` rather than the HTTP code. For
finer control, `filter` can halt the workflow (`on_reject: "halt"`) or skip only
its own task (`on_reject: "skip"`).

## Test it before you activate it

```bash
orion-server lint workflow.json
orion-server dry-run -w workflow.json -i payload.json
```

Both run offline. The dry run prints which tasks executed, which were skipped by
their condition, and the final context — which is how you check the branch logic
you just wrote actually branches. See [Test Workflows Offline](./testing.md).

## Related

- [Workflow Schema](../reference/workflows.md) — every field, the full data
  context, matching and rollout.
- [Task Functions](../reference/functions.md) — each function's exact `input`.
- [Expression Language](../reference/expressions.md) — the operator catalogue
  and its sharp edges.
- [Common Workflow Patterns](../guides/workflow-patterns.md) — the shapes these
  pieces make in practice.
