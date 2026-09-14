<!-- description: A channel is an Orion service endpoint: where traffic arrives, which workflow runs, and the contract — auth, rate limits, validation — a caller must satisfy. -->
<!-- type: concept -->
<!-- last_verified: 2026-09-14 -->

# Channels

A *channel* is a service endpoint. It says where traffic arrives, which workflow runs when it does, and what contract the caller has to satisfy on the way in.

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

A channel names exactly one workflow. That is what makes the channel the unit of selection when a service is [packaged](./packages.md): picking the endpoints picks the service.

## Protocols

Four protocols, set once and immutable across a channel's versions:

- **`rest`**: a method and a path pattern, such as `POST /orders` or `GET /orders/{id}`. Path parameters reach the workflow as request metadata.
- **`http`**: routes identically to `rest`. Both also stay reachable by channel name at `/api/v1/data/{name}`.
- **`kafka`**: the channel declares a topic. Orion registers a consumer for it at startup and on every engine reload.
- **`cron`**: the channel declares a schedule and a fixed payload. Orion materializes a durable occurrence per scheduled instant and runs it. It is the one protocol with no caller, so it registers no route, no topic, and is not reachable by name.

## Sync or async

`channel_type` decides whether the caller waits. A `cron` channel is always `async`, because nothing is waiting for it.

| | `sync` | `async` |
|---|---|---|
| **Answer** | The finished result | `202` with a trace id |
| **Result read from** | The response body | `GET /api/v1/admin/traces/{id}` |
| **Bounded by** | The channel's `timeout_ms` | The trace queue's capacity |

Use sync for request/response APIs: validation, enrichment, lookups, and transformations the caller needs an answer from. Use async for work the caller should not block on, where a trace id and a later poll are enough. Any REST or HTTP channel serves its async form at `/{route_pattern}/async`, so the same endpoint can be called either way.

## Scheduled work

A cron channel binds a schedule to a workflow the way a REST channel binds a route to one. That makes a schedule an *ingress*, not part of the workflow. One workflow can carry an hourly trigger and a nightly one with different payloads. Changing when something runs does not create a new version of what it does.

Occurrences are durable rows, so a schedule survives a restart, a rolling deploy and a node failure. A run that was missed is visible rather than absent. A `forbid` concurrency policy makes a schedule non-overlapping across the whole cluster. See [Cron transport](../reference/channel-config/cron.md).

## Traffic controls

A channel declares its own guards in a `config` object, and Orion enforces them before any workflow logic runs. Each one is a few lines of JSON, not code you write:

- **`auth`**: API-key, HMAC-signature or JWT verification for HTTP callers. Failures are a uniform `401`.
- **`rate_limit`**: a token bucket that answers `429` when it empties. The default bucket is per caller, not per channel.
- **`validation_logic`**: a JSONLogic predicate over the request. A falsy result rejects it with `400` before the workflow starts.
- **`deduplication`**: an idempotency key that turns a replay into `409` instead of a second execution.
- **`cache`**: a response cache for repeated identical requests.
- **`backpressure`**: a concurrency cap per node. Excess is shed with `503` rather than queued indefinitely.
- **`timeout_ms`**, **`origin_allow_list`**, **`response`** and **`tracing`**: the deadline, a server-side `Origin` check, response shaping, and a per-channel trace-storage override.

Every key, default and interaction is specified in [Channel configuration](../reference/channel-config/index.md). Two properties are worth carrying in your head from here:

- **Guards apply per ingress, not per protocol.** A Kafka record, an `/async` submission and an in-process `channel_call` get the same contract as a synchronous request, minus only what their transport cannot carry. A Kafka record has no `Origin` header to check.
- **A config that no longer parses quarantines the channel.** It is refused at every ingress rather than served with a guard silently missing. See [The entity lifecycle](./lifecycle.md).

## Channels calling channels

A workflow can invoke another channel's workflow with the `channel_call` function. The call runs in-process, with no network hop and no serialization round-trip, while the called channel keeps its own workflow, versions and governance. Cycles are detected and refused.

That is what lets one Orion instance hold a set of small, independently versioned services instead of one large workflow.

## Next steps

- [Channel configuration](../reference/channel-config/index.md): every guard key, with defaults and per-ingress semantics.
- [Workflows](./workflows.md): what the channel hands the request to.
- [Data API](../reference/data-api.md): how a request path resolves to a channel, and the shape of what comes back.
- [Configure a channel](../guides/author/channels.md): the guards as a walkthrough, one at a time.
