<!-- description: A channel is an Orion service endpoint: where traffic arrives, which workflow runs, and the contract — auth, rate limits, validation — a caller must satisfy. -->
# Understand Channels

**Page type:** Concept · **Audience:** Service authors

A **channel** is a service endpoint. It says where traffic arrives, which
workflow runs when it does, and what contract the caller has to satisfy on the
way in.

```orion-diagram
{
  "direction": "LR",
  "nodes": [
    { "id": "rest", "label": "POST /orders", "sublabel": "rest · sync", "type": "channel" },
    { "id": "async", "label": "POST /reports/async", "sublabel": "rest · async", "type": "channel" },
    { "id": "kafka", "label": "topic order.placed", "sublabel": "kafka", "type": "channel", "shape": "queue" },
    { "id": "wf1", "label": "order-processing", "type": "service" },
    { "id": "wf2", "label": "report-build", "type": "service" },
    { "id": "wf3", "label": "order-events", "type": "service" }
  ],
  "edges": [
    { "from": "rest", "to": "wf1" },
    { "from": "async", "to": "wf2" },
    { "from": "kafka", "to": "wf3" }
  ]
}
```

A channel names exactly one workflow. That is what makes the channel the unit of
selection when a service is [packaged](./packages.md): picking the endpoints
picks the service.

## Protocols

Three protocols, set once and immutable across a channel's versions:

- **`rest`**: a method and a path pattern, for example `POST /orders` or
  `GET /orders/{id}`. Path parameters reach the workflow as request metadata.
- **`http`**: routes identically to `rest`. Both also stay reachable by channel
  name at `/api/v1/data/{name}`.
- **`kafka`**: the channel declares a topic; Orion registers a consumer for it
  at startup and on every engine reload.

## Sync or async

`channel_type` decides whether the caller waits.

| | `sync` | `async` |
|---|---|---|
| **Answer** | The finished result | `202` with a trace id |
| **Result read from** | The response body | `GET /api/v1/admin/traces/{id}` |
| **Bounded by** | The channel's `timeout_ms` | The trace queue's capacity |

**Use sync for:** request/response APIs — validation, enrichment, lookups,
transformations the caller needs an answer from. **Use async for:** work the
caller should not block on, where a trace id and a later poll are enough.

Any REST or HTTP channel serves its async form at `/{route_pattern}/async`, so
the same endpoint can be called either way.

## Traffic controls

A channel declares its own guards in a `config` object, and Orion enforces them
before any workflow logic runs. Each one is a few lines of JSON, not code you
write:

- **`auth`**: API-key or HMAC-signature verification for HTTP callers. Failures
  are a uniform `401`.
- **`rate_limit`**: a token bucket that answers `429` when it empties. The
  default bucket is **per caller**, not per channel.
- **`validation_logic`**: a JSONLogic predicate over the request; a falsy
  result rejects it with `400` before the workflow starts.
- **`deduplication`**: an idempotency key that turns a replay into `409`
  instead of a second execution.
- **`cache`**: a response cache for repeated identical requests.
- **`backpressure`**: a concurrency cap per node; excess is shed with `503`
  rather than queued indefinitely.
- **`timeout_ms`**, **`origin_allow_list`**, **`response`** and **`tracing`** —
  the deadline, a server-side `Origin` check, response shaping, and a
  per-channel trace-storage override.

Every key, default, and interaction is specified in
[Channel Configuration](../reference/channel-config.md). Two properties are
worth carrying in your head from here:

- **Guards apply per ingress, not per protocol.** A Kafka record, an `/async`
  submission and an in-process `channel_call` get the same contract as a
  synchronous request, minus only what their transport cannot carry — a Kafka
  record has no `Origin` header to check.
- **A config that no longer parses quarantines the channel.** It is refused at
  every ingress rather than served with a guard silently missing. See
  [The Entity Lifecycle](./lifecycle.md).

## Channels calling channels

A workflow can invoke another channel's workflow with the `channel_call`
function. The call runs **in-process**: no network hop, no serialization
round-trip, while the called channel keeps its own workflow, versions, and
governance. Cycles are detected and refused.

That is what lets one Orion instance hold a set of small, independently
versioned services instead of one large workflow.

## Next steps

- [Channel Configuration](../reference/channel-config.md): every guard key,
  with defaults and per-ingress semantics.
- [Workflows](./workflows.md): what the channel hands the request to.
- [Data API](../reference/data-api.md): how a request path resolves to a
  channel, and the shape of what comes back.
- [Understand the HTTP Flow](../getting-started/first-service.md): create a channel
  and call it.
