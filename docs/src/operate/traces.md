<!-- description: A trace is Orion's record of one request: which workflow matched, which tasks ran, what each produced. Traces are also how async channels deliver results. -->
# Traces & Async Processing

A **trace** is Orion's record of one request: which workflow matched, which
tasks ran, what each produced, and how long it took. Traces are also the
delivery mechanism for async channels — the caller gets a trace id and reads the
result from the trace later.

That dual role is why this page exists. A trace-storage setting that looks like
a debugging convenience is, on the async path, the thing that returns the
answer.

## Choose a storage mode

`trace_storage.mode` decides when a trace row is written. It is a global default
that any channel can override with its `config.tracing` block.

| Mode | Behaviour | Use for |
|---|---|---|
| `sync` | Written inline before responding | Correctness-first estates. A served request implies a persisted trace |
| `async` | Enqueued to a bounded background queue, one write per task | Lifting the write cap when the trace table limits throughput |
| `batch` | Bounded queue; workers commit `batch_size` rows per transaction | Highest throughput |
| `off` | No persistence | Hot paths you never inspect |

**Use `sync` for:** anything where a missing trace is a problem. **Use `batch`
for:** high-volume channels where the database write rate caps request
throughput. The trade is explicit: in `async` and `batch` the request path runs
ahead of the trace table, and if it overruns `trace_storage.max_pending`, traces
are shed according to `async_on_overflow` (`drop`, or `block` to apply
backpressure to the request path).

> [!IMPORTANT]
> **`mode = "off"` does not turn off async traces.** It applies to the
> synchronous endpoint, where the caller already holds the answer. Appending
> `/async` *is* a request for a result to fetch later, so the row is written
> before the `202` returns and `trace_id` is always present. `off` is safe to
> combine with async channels.

The same asymmetry governs sampling: `trace_storage.sample_rate` applies to sync
traces only. To bound async trace storage, use `trace_storage.errors_only` or
`trace_queue.retention_hours` instead.

Every key, with defaults, is in
[Configuration › Trace Persistence](../reference/configuration.md#trace-persistence).

## Run an async channel

```bash
# Submit — returns immediately with a trace id and a trace token
curl -s -X POST http://localhost:8080/api/v1/data/orders/async \
  -H "Content-Type: application/json" \
  -d '{ "data": { "order_id": "ORD-123" } }'

# Read the result
curl -s -H "x-trace-token: <token>" \
  http://localhost:8080/api/v1/admin/traces/<trace-id>
```

The CLI does both halves in one command — it keeps the token from the `202` and
polls with it:

```bash
orion-cli send orders -f order.json --async-mode --wait

# Or read one later, with the token the submit printed:
orion-cli traces get <trace-id> --token <token>
orion-cli traces wait <trace-id> --token <token> --timeout 120
```

The `202` carries both `trace_id` and `trace_token`. Reading the trace needs
either that token or an admin credential — without one, any caller who guessed
an id could read another caller's payload.

Submissions run through a bounded queue:

```toml
[trace_queue]
workers = 4                          # concurrent async workers
buffer_size = 1000                   # queued submissions before rejection
processing_timeout_ms = 60000        # per-trace deadline
max_result_size_bytes = 1048576      # oversized results fail rather than truncate
max_queue_memory_bytes = 104857600   # total queued bytes before submissions get 503
```

Two bounds decide what a burst does. `buffer_size` caps the number of queued
submissions; `max_queue_memory_bytes` caps their total payload size. Whichever
is reached first, new submissions answer `503` rather than growing memory
without limit — load shedding, not queueing forever.

## Kafka ingress

A message consumed from Kafka runs the same admission guards and the same
workflow dispatch as an HTTP request, and it produces a `traces` row like one.
A scheduled run writes `mode = "cron"`, with the channel's authored
`transport_config.payload` as its `input_json` and the occurrence's id in the
[ledger](../reference/admin-api.md#cron-occurrences) linking the two. It follows
the `/async` contract rather than the sync one: `trace_storage.mode = "off"`
is upgraded to `sync` and sampling is forced, because the occurrence is the only
thing that observes the run and a run nobody can debug is worth less than the
storage it saves. `errors_only` still applies as documented.

The row's `mode` is `kafka`, so it can be told apart from the two HTTP paths:

```bash
curl "$ORION/api/v1/data/traces?mode=kafka&limit=20" -H "x-api-key: $KEY"
```

The channel's `config.tracing` and the global `trace_storage.mode` apply as they
do everywhere else — `off`, `errors_only` and `sample_rate` all suppress the row
for a Kafka message exactly as they do for an HTTP one, and a suppressed trace
costs no serialization.

Two fields differ from an HTTP trace, and both are deliberate:

| Field | On a Kafka trace | Why |
|---|---|---|
| `channel_id` | absent | A Kafka channel is addressed by topic; the consumer resolves it by name, and the id is a second lookup for a column nothing reads on this path. |
| `input_json` | absent | The record's payload is already carried on the stored result, so storing it again would double the row for a value it holds. |

Alongside the trace, a Kafka channel still has:

| Signal | Where |
|---|---|
| Per-message outcome counts (`ok`, `error`, `timeout`, `duplicate`) | `orion_messages_total{channel,status}` — see [Monitoring](monitoring.md) |
| Processing duration | `orion_message_duration_seconds{channel}` |
| Consumer lag, rebalances, ingest health | the `orion_kafka_*` metrics and `/health`'s `kafka` component |
| The failing record itself | the Kafka DLQ topic (`kafka.dlq`), which carries the record and the reason |
| Per-message detail | the process log — every refusal and failure is logged with `topic` and `channel` |

The record, not the trace, is still what you re-drive after a failure: the
Kafka DLQ topic holds it. The trace is what tells you what happened.

## Drain the dead letter queue

An async trace that fails is written to the `trace_dlq` table and retried
automatically with exponential backoff.

```toml
[trace_queue]
dlq_retry_enabled = true
dlq_max_retries = 5          # 1–16; backoff is 2^retries seconds
dlq_poll_interval_secs = 30
dlq_batch_size = 20          # rows claimed per tick — raise to drain a backlog faster
dlq_lease_secs = 60
```

A row that exhausts its retries is marked exhausted and stops being retried; it
stays in the table for you to inspect. Inspect and purge over the admin API:

```bash
curl -s "http://localhost:8080/api/v1/admin/trace-dlq"
curl -s -X POST "http://localhost:8080/api/v1/admin/trace-dlq/purge" \
  -H 'Content-Type: application/json' -d '{"older_than_hours": 168}'
```

`older_than_hours` travels in the body and is required — an omitted age must
not silently mean "everything". Only *exhausted* entries are purged; a row
still being retried is never deleted.

In cluster mode each row is claimed with a lease, so exactly one node retries
it, and the retry job itself is lease-gated so only one node polls per tick.

> [!WARNING]
> Turning off `dlq_retry_enabled` also freezes the `orion_trace_dlq_depth`
> gauge, because the retry loop is what refreshes it. A flat line then means
> "nobody is looking", not "the queue is empty".

## Keep the table bounded

Nothing else trims the `traces` table:

```toml
[trace_queue]
retention_hours = 72          # 0 keeps traces forever
cleanup_interval_secs = 3600
```

The cleanup job deletes in chunks so a large backlog does not lock the table,
and in cluster mode it is lease-gated to one replica per tick. It swallows
per-tick errors by design, which is why
[the job-staleness alert](./monitoring.md#what-to-alert-on) is the only thing
that tells you it stopped.

`retention_hours = 0` is a real choice for a compliance estate — just pair it
with `errors_only` or a sampling rate, or the largest table Orion writes grows
without bound.

## Read a trace

```bash
curl -s "http://localhost:8080/api/v1/admin/traces?channel=orders&status=failed&limit=20"
curl -s "http://localhost:8080/api/v1/admin/traces/<trace-id>"
```

`status` takes one of `pending`, `running`, `completed`, `failed`. It is
matched literally rather than validated, so a value outside that set — `error`,
say — comes back as an empty page rather than a `400`.

The list endpoint pages by cursor: pass `cursor` from `next_cursor` rather than
walking `offset`, and ask for `include_total=true` only when you genuinely need
the count. The reasoning — deep `offset` paging counts past every skipped row —
is in [Design Notes › Cursor paging](../reference/design-notes.md#cursor-paging).

A trace carries the per-task execution path, so it answers "which task skipped,
and why" without re-running anything. Field-by-field, the object is specified in
[Data API](../reference/data-api.md). Requests can also carry a profiling flag
to record per-function and per-connector timings.

## Related

- [Monitoring & Alerts](./monitoring.md): the DLQ-depth, job-staleness, and
  persistence-failure alerts that cover this subsystem.
- [Troubleshooting](./troubleshooting.md): a filling DLQ, a stuck queue, and
  what to do about each.
- [Data API](../reference/data-api.md): the trace object and the async
  submission contract.
- [Configuration Reference](../reference/configuration.md#trace-queue): every
  `[trace_queue]` and `[trace_storage]` key.
