# Common Workflow Patterns

Six shapes that cover most workflows. Each states the problem, the pattern, and
the mistake it prevents.

## Parse, then process

**Problem.** Task conditions referencing `data.*` never fire, and the response
comes back empty.

**Pattern.** Every workflow that reads request data starts with `parse_json`:

```json
{
  "tasks": [
    { "id": "parse", "function": { "name": "parse_json",
        "input": { "source": "payload", "target": "order" } } },
    { "id": "process", "condition": { ">": [{ "var": "data.order.total" }, 100] },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.order.priority", "logic": "high" } ] } } }
  ]
}
```

**Why it bites.** The raw payload sits outside the expression context, so
`{"var": "payload.total"}` resolves to nothing and every condition over `data.*`
evaluates against `{}`. Nothing errors. The workflow simply does nothing, which
is the hardest failure to read.

## Mutually exclusive branches

**Problem.** Two tasks that were meant as if/else both run.

**Pattern.** Write branch conditions that cannot both be true:

```json
[
  { "id": "vip",      "condition": { ">=": [{ "var": "data.order.amount" }, 500] }, "function": "..." },
  { "id": "standard", "condition": { "<":  [{ "var": "data.order.amount" }, 500] }, "function": "..." }
]
```

**Why it bites.** Tasks are independent, not exclusive. `>= 500` paired with
`<= 500` both fire at exactly 500, and whichever runs later wins on every path
they share — a bug that only appears on the boundary value.

## Workflow condition vs task condition

**Problem.** A whole workflow is skipped when only one step should have been, or
vice versa.

**Pattern.**

| Use | When |
|---|---|
| Workflow `condition` | You have two *different pipelines* for one channel and want to pick between them |
| Task `condition` | You have one pipeline with a branch inside it |

Most workflows are `"condition": true` at the top with conditions on tasks. Reach
for a workflow-level condition when the pipelines genuinely differ — a v2 payload
shape alongside a v1, say — and give the specific one a higher `priority` so it
is evaluated first.

## Enrich from an external system

**Problem.** Credentials end up in workflow JSON, which is then unreviewable and
unpromotable.

**Pattern.** Reference a connector by name; keep the secret in the connector:

```json
{
  "tasks": [
    { "id": "parse", "function": { "name": "parse_json",
        "input": { "source": "payload", "target": "event" } } },
    { "id": "notify", "function": { "name": "http_call", "input": {
        "connector": "slack-webhook",
        "method": "POST",
        "body_logic": { "var": "data.event" },
        "output": "data.notified"
    }}}
  ]
}
```

**Why it bites.** A workflow with an embedded credential exports as `"******"`
and is refused on import, so it cannot be promoted at all — and it was in your
git history the whole time. See
[Connect Databases & APIs](../build/connectors.md).

## Compose channels in-process

**Problem.** One service needs another service's answer, and an HTTP hop between
them costs latency and adds a failure mode.

**Pattern.** `channel_call` invokes another channel's workflow in-process. The
caller builds the sub-request, calls, and reads the result:

```json
{{#include ../../../examples/packages/channel-composition/workflow.json}}
```

The called channel is an ordinary service with its own workflow, channel, and
versions — it just happens to also be reachable from inside another workflow:

```json
{{#include ../../../examples/packages/channel-composition/workflow-lookup.json}}
```

**Running the example:**

```bash
./examples/deploy.sh channel-composition

curl -s -X POST http://localhost:8080/api/v1/data/order-enrichment \
  -H 'Content-Type: application/json' \
  --data @examples/packages/channel-composition/request.json
```

```json
{
  "status": "ok",
  "data": {
    "order": { "order_id": "ORD-5511", "customer_id": 42, "total": 800,
               "tier": "vip", "discount_pct": 15 },
    "customer": { "lookup": { "customer_id": 42, "tier": "vip", "discount_pct": 15 } }
  },
  "errors": []
}
```

Four things this shows that are easy to get wrong:

- **The payload is built first.** `channel_call`'s `data_logic` is a single
  expression, and JSONLogic has no object constructor — so the caller assembles
  the sub-request in `temp_data` with `map`, then passes it by reference.
- **`output` receives the callee's whole data context**, not its HTTP envelope.
  The lookup workflow parses into `data.lookup`, so the caller reads
  `data.customer.lookup.tier`.
- **The callee is independently callable.** `POST /api/v1/data/customer-lookup`
  works on its own, which is what makes it a service rather than a subroutine.
- **The call is a function call.** No network hop, no serialization round-trip.
  Cycles are detected and refused.

Two things to know before composing: the called channel applies **its own**
guards, minus the ones a `channel_call` cannot have (no `auth`, no origin check,
no dedup), and its `timeout_ms` applies inside your request's budget. Size the
caller's timeout above the callee's.

## Collect errors instead of halting

**Problem.** One optional enrichment fails and the whole request fails with it.

**Pattern.** Set `continue_on_error` and read the `errors` array:

```json
{ "workflow_id": "order-processing", "continue_on_error": true, "tasks": [ "..." ] }
```

```json
{
  "status": "ok",
  "data": { "order": { "...": "..." } },
  "errors": [ { "code": "TASK_ERROR", "task_id": "enrich", "message": "HTTP request failed..." } ]
}
```

**Why it bites.** The envelope says `"status": "ok"` with a non-empty `errors`
array, so a client checking only the HTTP code reads a partial result as a
complete one. If you turn this on, the caller **must** inspect `errors`. For
per-task control instead, `filter` with `on_reject: "skip"` skips only its own
task.

## Related

- [Author Workflows](../build/workflows.md) — the how-to these patterns sit on.
- [Worked Examples: Prompt to Service](./worked-examples.md) — four of these
  patterns as deployable packages.
- [Expression Language](../reference/expressions.md) — the operators, and the
  misspelling trap.
- [Task Functions](../reference/functions.md) — every function's input schema.
