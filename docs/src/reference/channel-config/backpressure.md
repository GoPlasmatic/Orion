<!-- description: The backpressure block of a channel: a per-node concurrency permit shared by every ingress, with excess shed as 503 rather than queued. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `backpressure`

`backpressure` bounds a channel's in-flight work with a semaphore. When every permit is taken, more requests are refused with `503 Service Unavailable` immediately — load shedding, not queueing.

## Synopsis

```json
{ "backpressure": { "max_concurrent_per_node": 200 } }
```

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `max_concurrent_per_node` | integer | yes | — | Maximum concurrent requests for this channel on this node. |

The permit is per channel, not per ingress: synchronous requests, queued `/async` work, Kafka records, and `channel_call`s all draw from the same semaphore. Each channel's semaphore is independent, so a spike on one channel does not shed another's traffic.

**Cross-ingress semantics.** A Kafka record that cannot get a permit is left uncommitted for redelivery rather than shed. The transport can wait; an HTTP caller cannot be told to.

The semaphore is per process, as the name states: N replicas admit up to N × `max_concurrent_per_node` in flight in total.

## Related

- [Timeouts, retries and circuit breakers](../../operate/run/failure-handling.md): load shedding among the other controls.
- [Configure a channel](../../guides/author/channels.md): adding a guard in practice.
- [Deploy a cluster](../../operate/deploy/cluster.md): why the permit is per node.
- [Channel configuration](./index.md): every key, with its page.
