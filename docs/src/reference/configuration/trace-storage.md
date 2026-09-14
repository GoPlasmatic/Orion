<!-- description: The [trace_storage] settings: the sync, async, batch and off persistence modes, sampling, errors-only, queue capacity and overflow behaviour. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Trace persistence settings

Orion's own per-request trace records — rows in the `traces` table, read through `/api/v1/admin/traces`. Unrelated to the OTLP export in `[tracing]` above; before 1.0 these keys lived under `[tracing.storage]`, which is exactly the confusion the split removes. A channel can override the mode with its `config.tracing` field; unset per-channel fields fall back to what is set here.

## Synopsis

```toml
[trace_storage]
mode = "sync"
sample_rate = 1.0
errors_only = false
max_pending = 10000
async_on_overflow = "drop"
overflow_block_timeout_ms = 100
async_workers = 4
batch_size = 1000
batch_flush_interval_ms = 100
batch_workers = 4
```

## Description

| Mode | Behaviour |
|---|---|
| `sync` | Write inline before responding. Strongest durability; throughput capped by single-writer contention. |
| `async` | Enqueue to a bounded background queue, one database write per task. |
| `batch` | Bounded queue; workers commit `batch_size` rows per transaction. Highest throughput. |
| `off` | No persistence at all. |

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `trace_storage.mode` | `"sync"` | `ORION_TRACE_STORAGE__MODE` | A served request implies a persisted trace, at the cost of the DB's write rate capping throughput. Set `batch` or `async` to lift that cap — the request path then runs ahead of the trace table and can overrun `max_pending`, at which point traces are shed per `async_on_overflow`. |
| `trace_storage.sample_rate` | `1.0` | `ORION_TRACE_STORAGE__SAMPLE_RATE` | Fraction of traces persisted, `0.0` to `1.0`. Applies to sync traces only — an async submission's trace row is how its result is delivered, so async traces always persist regardless of this rate; bound async storage with `errors_only` or `trace_queue.retention_hours` instead. |
| `trace_storage.errors_only` | `false` | `ORION_TRACE_STORAGE__ERRORS_ONLY` | Persist only traces that ended with errors — a cheap way to keep the table small. |
| `trace_storage.max_pending` | `10000` | `ORION_TRACE_STORAGE__MAX_PENDING` | Queue capacity in `async` and `batch` modes. |
| `trace_storage.async_on_overflow` | `"drop"` | `ORION_TRACE_STORAGE__ASYNC_ON_OVERFLOW` | `drop` or `block`. `block` applies backpressure to the request path. |
| `trace_storage.overflow_block_timeout_ms` | `100` | `ORION_TRACE_STORAGE__OVERFLOW_BLOCK_TIMEOUT_MS` | How long `block` waits for capacity before dropping anyway. |
| `trace_storage.async_workers` | `4` | `ORION_TRACE_STORAGE__ASYNC_WORKERS` | Worker count in `async` mode. |
| `trace_storage.batch_size` | `1000` | `ORION_TRACE_STORAGE__BATCH_SIZE` | Rows per transaction in `batch` mode, and the dominant term in how fast the queue drains: measured on SQLite with 4 workers, `100` drains 26k rows/s and `1000` drains 45k rows/s. Max 1000 — the batch INSERT binds ~11 parameters per row against SQLite's 32 766-bind statement cap. |
| `trace_storage.batch_flush_interval_ms` | `100` | `ORION_TRACE_STORAGE__BATCH_FLUSH_INTERVAL_MS` | How long a partial batch waits before flushing. |
| `trace_storage.batch_workers` | `4` | `ORION_TRACE_STORAGE__BATCH_WORKERS` | Worker count in `batch` mode; each owns an independent batch. |

`mode = "off"` applies to the **synchronous** endpoint, where the caller already holds the answer. It does not disable persistence for `POST /{channel}/async`. Appending `/async` *is* a request for a result to be fetched later. The trace row is written before the `202` is returned, so `trace_id` is always present. `off` is safe to combine with async channels.

## Related

- [Traces and async processing](../../operate/run/traces.md): choosing a mode, and what each costs.
- [Channel configuration › `tracing`](../channel-config/tracing.md): the per-channel override.
- [Trace queue settings](./trace-queue.md): the async path these modes feed.
- [Server configuration](./index.md): every section, by what you are configuring.
