<!-- description: The validation_logic key of a channel: a JSONLogic predicate over data and metadata that rejects a request with 400 before its workflow runs. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `validation_logic`

`validation_logic` is a JSONLogic predicate evaluated before workflow execution on every ingress. A truthy result admits the request; a falsy one rejects it with `400 Bad Request`. JSONLogic truthiness applies: `false`, `null`, `0`, `""`, and `[]` are falsy.

## Synopsis

```json
{
  "validation_logic": {
    "and": [
      { "!!": [{ "var": "data.order_id" }] },
      { ">": [{ "var": "data.amount" }, 0] }
    ]
  }
}
```

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `validation_logic` | JSONLogic | no | none | Predicate over `{data, metadata}`. See the [Expression Language](../expressions.md). |

**Context.** The expression evaluates against exactly `{ "data": …, "metadata": … }`. `data` is the request payload as submitted. The guard runs before any workflow task, so `data.order_id` resolves here even though the workflow itself reads the payload only after `parse_json`. `metadata` has the same shape the workflow's data context carries (headers, query, path params, channel name; transport-dependent). See the [Workflow Schema](../workflows.md). On Kafka, `metadata` carries the record coordinates and no headers.

Rules:

- An expression that cannot be evaluated against a request rejects it with the same opaque `400` — the detail is logged, not returned, because the data plane is anonymous.
- A `validation_logic` that does not compile quarantines the channel at load.
- Payload size is bounded globally by [`ingest.max_payload_size`](../configuration/ingest.md), not per channel.

## Related

- [Expression language](../expressions.md): every operator the predicate may use.
- [Configure a channel](../../guides/author/channels.md): adding a guard in practice.
- [Workflow definition › The data context](../workflows.md#the-data-context): the `metadata` shape the predicate reads.
- [Channel configuration](./index.md): every key, with its page.
