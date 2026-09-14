<!-- description: The timeout_ms key of a channel: the workflow execution deadline, and how each ingress defaults or clamps it to a transport ceiling. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `timeout_ms`

`timeout_ms` bounds workflow execution for one message. A synchronous request that exceeds it answers `504 Gateway Timeout`.

## Synopsis

```json
{ "timeout_ms": 5000 }
```

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `timeout_ms` | integer | no | per ingress, below | Maximum workflow execution time in milliseconds. |

The value governs every ingress. Where the channel declares none, each ingress falls back to its own server-level default. On two ingresses that server value is a **ceiling** the channel value is clamped to, never a default:

| Ingress | Channel declares none | Channel declares more than the transport allows |
|---|---|---|
| synchronous HTTP | runs to completion | honored — nothing else waits on it |
| `/async` | `trace_queue.processing_timeout_ms` | clamped to `trace_queue.processing_timeout_ms` |
| Kafka | `kafka.processing_timeout_ms` | clamped to `kafka.processing_timeout_ms` |
| `channel_call` | `engine.default_channel_call_timeout_ms` | honored — the calling task's own `timeout_ms` outranks it anyway |

<details><summary>Why the two clamps</summary>

On those paths the deadline protects something shared. A Kafka dispatch blocks the consumer's poll loop. A channel asking for ten minutes would push the consumer past librdkafka's `max.poll.interval.ms` and get it evicted from its group mid-record. An `/async` dispatch occupies one of a fixed number of queue workers, so an over-long deadline starves every other channel's queued work. A channel can shorten its deadline everywhere; it can lengthen it only where nothing else depends on it. Raise the transport setting if a channel genuinely needs longer there.

</details>

A `channel_call` task may set its own `timeout_ms`, which outranks the target channel's. See [Task Functions](../functions/index.md). The server-level settings live in the [Configuration Reference](../configuration/index.md).

## Related

- [Timeouts, retries and circuit breakers](../../operate/run/failure-handling.md): the deadline among the other controls.
- [Trace queue settings](../configuration/trace-queue.md): the `/async` ceiling.
- [Kafka settings](../configuration/kafka.md): the Kafka ceiling.
- [`channel_call`](../functions/channel_call.md): the task timeout that outranks a target's.
- [Channel configuration](./index.md): every key, with its page.
