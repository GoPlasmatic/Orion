<!-- description: The [trace_queue] settings: async workers and buffer, retention and cleanup, per-trace deadlines, result and memory caps, and the dead-letter retry loop. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Trace queue settings

The async trace pipeline: `POST /{channel}/async` enqueues and workers execute. Failures land in a database dead-letter queue with automatic retry.

## Synopsis

```toml
[trace_queue]
workers = 4
buffer_size = 1000
shutdown_timeout_secs = 30
retention_hours = 72
cleanup_interval_secs = 3600
processing_timeout_ms = 60000
max_result_size_bytes = 1048576
max_queue_memory_bytes = 104857600
dlq_retry_enabled = true
dlq_max_retries = 5
dlq_poll_interval_secs = 30
dlq_batch_size = 20
dlq_lease_secs = 60
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `trace_queue.workers` | `4` | `ORION_TRACE_QUEUE__WORKERS` | Raise for more concurrent async processing. This is the real worker knob. |
| `trace_queue.buffer_size` | `1000` | `ORION_TRACE_QUEUE__BUFFER_SIZE` | Raise to absorb bigger bursts before submissions are rejected. |
| `trace_queue.shutdown_timeout_secs` | `30` | `ORION_TRACE_QUEUE__SHUTDOWN_TIMEOUT_SECS` | How long shutdown waits for in-flight traces. |
| `trace_queue.retention_hours` | `72` | `ORION_TRACE_QUEUE__RETENTION_HOURS` | Lower to shrink the `traces` table; `0` keeps traces forever. |
| `trace_queue.cleanup_interval_secs` | `3600` | `ORION_TRACE_QUEUE__CLEANUP_INTERVAL_SECS` | How often the trace cleanup job runs. |
| `trace_queue.processing_timeout_ms` | `60000` | `ORION_TRACE_QUEUE__PROCESSING_TIMEOUT_MS` | Per-trace deadline on the async path. |
| `trace_queue.max_result_size_bytes` | `1048576` | `ORION_TRACE_QUEUE__MAX_RESULT_SIZE_BYTES` | Raise for large results; oversized ones are rejected (sync) or failed (async). |
| `trace_queue.max_queue_memory_bytes` | `104857600` | `ORION_TRACE_QUEUE__MAX_QUEUE_MEMORY_BYTES` | Total queued payload bytes before new submissions get `503`. |
| `trace_queue.dlq_retry_enabled` | `true` | `ORION_TRACE_QUEUE__DLQ_RETRY_ENABLED` | Disable only to freeze the DLQ for inspection — note the `orion_trace_dlq_depth` gauge stops updating with it. |
| `trace_queue.dlq_max_retries` | `5` | `ORION_TRACE_QUEUE__DLQ_MAX_RETRIES` | Attempts before a row is marked exhausted. Must be 1–16 (backoff is 2^retries seconds); use `dlq_retry_enabled` to turn retries off. |
| `trace_queue.dlq_poll_interval_secs` | `30` | `ORION_TRACE_QUEUE__DLQ_POLL_INTERVAL_SECS` | How often the retry worker polls. |
| `trace_queue.dlq_batch_size` | `20` | `ORION_TRACE_QUEUE__DLQ_BATCH_SIZE` | Rows claimed per retry tick. Raise to drain a large backlog faster. |
| `trace_queue.dlq_lease_secs` | `60` | `ORION_TRACE_QUEUE__DLQ_LEASE_SECS` | How long a claimed row stays leased to one node. |

## Related

- [Traces and async processing](../../operate/run/traces.md): the async pipeline these settings size.
- [`orion-cli dlq`](../cli/orion-cli/dlq.md): draining the dead-letter queue by hand.
- [Trace persistence settings](./trace-storage.md): how sync traces are written.
- [Server configuration](./index.md): every section, by what you are configuring.
