<!-- description: The channel_call task function: invoke another channel's workflow in-process with no network hop, with cycle detection and a maximum call depth. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `channel_call`

Invokes another channel's workflow **in-process**: no network hop. The called
channel keeps its own versioning and governance. Cycle detection and a max call
depth prevent runaway recursion.

## Synopsis

```json
{
  "name": "channel_call",
  "input": {
    "channel": "customer-lookup",
    "data": {
      "var": "data.order.customer_id"
    },
    "output": "data.customer",
    "timeout_ms": 0
  }
}
```

## Description

`channel_call` is a composition function. It runs inside the calling request and names no connector.

**Retry safety:** `depends_on` `channel`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `channel` | string \| JSONLogic | yes | — | Target channel. Accepts the pre-1.0 name `channel_logic` |
| `data` | any \| JSONLogic | no | request payload | Payload passed to the target channel. Accepts the pre-1.0 name `data_logic` |
| `output` | string \| JSONLogic | no | `"data"` | Dotted path where the called channel's response is stored. Accepts the pre-1.0 name `response_path` |
| `timeout_ms` | number | no | from config | Per-call timeout in milliseconds |

`channel` and `data` are each one field, not the `channel`/`channel_logic` and
`data`/`data_logic` pairs they were before 1.5. A literal is JSONLogic for itself, so the static spelling is unchanged and still folds once when the engine is built. An expression in the same field is what makes the target or the payload depend on the message. The old names remain accepted as aliases. Supplying both spellings of one field is an error, not a precedence rule.

## Examples

```json
{
  "name": "channel_call",
  "input": {
    "channel": "customer-lookup",
    "data": { "var": "data.order.customer_id" },
    "output": "data.customer"
  }
}
```

A computed `channel` routes one task to a channel the message names. The
dependency endpoint reports `has_dynamic_channel_calls` for a workflow that
contains one, because the static list of targets cannot be complete:

```json
{
  "name": "channel_call",
  "input": {
    "channel": { "cat": ["notify-", { "var": "data.region" }] },
    "output": "data.notified"
  }
}
```

## Related

- [Channels](../../concepts/channels.md): what a channel is, and what calling one in-process keeps.
- [Author a workflow](../../guides/author/workflows.md): composing pipelines out of tasks.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Common workflow patterns](../../guides/patterns/workflow-patterns.md): composition and fan-out shapes in practice.
- [Admin API › Workflows](../admin-api/workflows.md): the dependencies endpoint that lists a workflow's targets.
