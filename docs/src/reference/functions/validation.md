<!-- description: The validation task function (alias validate): evaluate a list of JSONLogic rules and record each failure in the response's errors array without halting. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `validation`

Evaluates a list of rules. Each rule's `logic` must evaluate to exactly `true`;
any other result records the rule's `message` in the response's error list.
`validate` is an accepted alias for `validation`.

## Synopsis

```json
{
  "name": "validation",
  "input": {
    "rules": []
  }
}
```

## Description

`validation` is an engine built-in from dataflow-rs. Orion declares no input schema for it, so a mistake in `input` surfaces when the task runs rather than when the workflow is saved.

**Retry safety:** `pure`. An engine built-in reads and writes the message and nothing else. Validation is non-destructive: it never mutates the data context.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `rules` | array | yes | — | List of `{ "logic", "message" }` rules |
| `rules[].logic` | JSONLogic | yes | — | Must evaluate to `true` to pass |
| `rules[].message` | string | yes | — | Error message recorded when the rule fails |

## Examples

```json
{
  "name": "validation",
  "input": {
    "rules": [
      { "logic": { "!!": [{ "var": "data.order.customer_id" }] }, "message": "customer_id is required" },
      { "logic": { ">": [{ "var": "data.order.total" }, 0] },        "message": "total must be positive" }
    ]
  }
}
```

## Caveats

### A failed rule records an error and carries on

**A failed rule records an error; it does not stop the workflow unless you say so.** The task returns a `400` and the message lands in the response's `errors` array. The engine's rule is that `4xx` warns and carries on; `continue_on_error` governs `5xx` and handler errors only. So a `validation`
followed by unguarded tasks proceeds exactly as if it had passed.

Collecting every failure and carrying on is a legitimate shape, which is why
it is the default. When you meant a gate, add
[`halt_on`](../workflows.md#halting-on-failure):

```json
{ "id": "check", "name": "Check", "halt_on": "failure",
  "function": { "name": "validation", "input": { "rules": [
    { "logic": { "==": [1, 1] }, "message": "…" } ] } } }
```

The task keeps its own `400` on the audit trail and in `metadata.progress`.
Two older spellings still work and are better when you need something else. [`filter`](./filter.md) halts with no body and records `299`. A later task with a `condition` on the failure plus `terminal: true` is the only form that can answer with a status of its own.

`terminal: true` on the `validation` itself does not help — it is about
[position, not outcome](../workflows.md#terminal-steps), so it halts whether
the rules passed or failed. `orion-server lint` reports the unguarded shape
as `engine.unguarded_validation`.

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Author a workflow](../../guides/author/workflows.md): conditions, mapping and validation in practice.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Workflow definition › Halting on failure](../workflows.md#halting-on-failure): turning a validation into a gate.
- [Workflow definition](../workflows.md#the-data-context): the data context every function reads and writes.
