<!-- description: How to author an Orion workflow: reaching request data, branching with JSONLogic, task groups and terminal steps, calling out of process, and error handling. -->
# Authoring Workflows

A workflow is a JSON document listing tasks to run in order. This page is how to write one: how request data reaches your logic, how to branch, how tasks reach outside the process, and how errors come back out.

## Start with parse, always

The raw request payload is **not** in the expression context. A workflow that reads request data begins with `parse_json`, which lifts the payload into the data context under a name you choose:

```json
{ "id": "parse", "name": "Parse payload",
  "function": { "name": "parse_json", "input": { "source": "payload", "target": "order" } } }
```

Everything downstream then reads `data.order.*`.

> [!WARNING]
> **Skip this and nothing errors.** `{"var": "payload.total"}` resolves to
> nothing, conditions referencing `data.*` evaluate against an empty object,
> every conditional task silently skips, and the response comes back empty. It
> is the single most common way a first workflow "does nothing".

## Shape the data with `map`

`map` writes values into the context. Each mapping has a `path` to write and a `logic` producing the value — a literal, or a [JSONLogic](../reference/expressions.md) expression over what is already there:

```json
{ "id": "flag", "name": "Flag order",
  "function": { "name": "map", "input": { "mappings": [
    { "path": "data.order.flagged", "logic": true },
    { "path": "data.order.alert",
      "logic": { "cat": ["High-value order: $", { "var": "data.order.total" }] } }
  ]}}}
```

Paths are created if they do not exist. Order matters: a mapping can read what an earlier task wrote.

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

Most workflows want `"condition": true` at the top and conditions on individual tasks:

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

Two tasks with mutually exclusive conditions is how you write an if/else. Write the branches so exactly one fires — overlapping conditions both run, and the later one wins on any path they share.

## Group tasks, and stop early

A `tasks` element carrying its own `tasks` key is a **task group**: one condition guarding a contiguous run of tasks, instead of the same condition repeated on each.

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

The condition is evaluated **once, on entry**. A false result skips the whole span without evaluating the members' own conditions, so a task inside the block cannot switch off its own siblings by changing what the condition reads.

`terminal: true` ends the workflow after the step runs — on a group, after the whole span. Together they are the guard clause: *if this, answer and stop*. Without it every later task has to restate the negation of every earlier exit, and those conditions grow with each branch you add.

`terminal` is about **position, not outcome**. A false `condition` does not halt, and neither does a skipped task; a task that *failed* under `continue_on_error: true` does, because the author said nothing after this runs. It also works on a plain task, not just a group.

Groups nest, up to 8 deep, and share one id namespace with tasks — a group id colliding with a task id is refused at create. The field-by-field contract for both shapes is in the [Workflow Reference](../reference/workflows.md#task-groups).

> [!NOTE]
> Task groups need dataflow-rs 3.6, which Orion 1.2.0 ships. A definition using one **fails to load** on an older engine, loudly; a bare `terminal: true` is silently ignored there and every later task runs. Gate on the server version if you deploy definitions to instances you do not control.

## Say a thing once

A shared document — any JSON in the definition set carrying `constants`, `errors` or `fragments` — declares values and task sequences every workflow can reference, so a connector target or an error string lives in one place instead of being copied per workflow:

```json
{ "input": { "$from": "constants.db", "collection": "users" } }
{ "id": "_session", "use": "require-session", "with": { "deny_message": "Please sign in." } }
```

`$from` splices the named value's fields into the object around it (siblings win); `use` expands a parameterised task sequence with its ids namespaced by the call site. Both resolve **before** validation, so `lint`, `dry-run` and `test` all check the expanded form. Full rules in the [CLI reference](../reference/cli.md#shared-definitions).

`orion-server lint ./definitions` resolves the catalog automatically and reports an unresolved reference as an error; the single-file commands take `--definitions <dir>`.

## Reach outside the process

Connector-backed tasks call databases, HTTP APIs, caches, and Kafka. They name a connector rather than a URL, and write their result to an `output` path:

```json
{ "id": "enrich", "name": "Look up the customer",
  "function": { "name": "http_call", "input": {
    "connector": "crm",
    "method": "GET",
    "output": "data.customer"
  }}}
```

Because the credentials live on the connector and not in the task, your workflow JSON stays safe to commit. See [Connect Databases & APIs](./connectors.md).

**Which data function?** Reach for `data_query` and `data_write` first: one backend-neutral envelope that lowers to SQL, MongoDB, or Elasticsearch. Drop to `db_read`/`db_write` only when you need SQL the dialect cannot express.

## Use the scratch space

The context has three namespaces, and the difference matters:

- `data` — the working document. On a sync channel this *is* the response body, so anything you leave here, the caller sees.
- `metadata` — what the ingress recorded: channel id, HTTP method, headers, route parameters.
- `temp_data` — scratch. Intermediate values you need while computing but do not want to return.

Put working state in `temp_data` and keep the response clean:

```json
{ "path": "temp_data.raw_score", "logic": { "var": "data.risk.score" } }
```

## Decide what a failure does

By default the first failing task stops the workflow and the caller gets an error envelope. To collect errors and keep going instead, set `continue_on_error`:

```json
{ "workflow_id": "order-processing", "continue_on_error": true, "tasks": [ "..." ] }
```

Note the envelope this produces: `"status": "ok"` with a non-empty `errors` array. A client that only checks the HTTP code will read that as success, so anything relying on `continue_on_error` must inspect `errors`.

For finer control, `filter` takes `on_reject: "halt"` to stop the whole workflow, or `on_reject: "skip"` to skip only that task.

## Test it before you activate it

Check a workflow offline, before it can touch traffic:

```bash
orion-server lint workflow.json
orion-server dry-run -w workflow.json -i payload.json
```

`lint` checks the workflow against the schema; `dry-run` executes it against a sample payload and prints the context each task produced. See [Test Workflows Offline](./testing.md).

## Related

- [Workflow Schema](../reference/workflows.md) — every field, with defaults.
- [Task Functions](../reference/functions.md) — the input each function takes.
- [Expression Language](../reference/expressions.md) — the operators you can use in `logic` and `condition`.
- [Workflow Patterns](../guides/workflow-patterns.md) — the shapes these pieces make once combined.
