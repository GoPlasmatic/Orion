<!-- description: The tracing block of a channel: a per-channel override of the trace persistence mode, sample rate and errors-only policy, plus task details. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `tracing`

`tracing` overrides the global [`[trace_storage]`](../configuration/trace-storage.md) policy for one channel. Each field is independently optional; an unset field falls back to the global value.

## Synopsis

```json
{
  "tracing": { "mode": "async", "errors_only": true, "task_details": true }
}
```

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `mode` | string | no | global `trace_storage.mode` | `sync`, `async`, `batch`, or `off`. |
| `sample_rate` | number | no | global `trace_storage.sample_rate` | Fraction of traces persisted, `0.0`–`1.0`. Applies to sync traces only; async traces always persist. |
| `errors_only` | boolean | no | global `trace_storage.errors_only` | Persist only traces that ended with errors. |
| `task_details` | boolean | no | `false` | Capture a per-task execution trace into `task_trace_json`. No global setting exists — this is per-channel only. |

> [!NOTE]
> Each `task_details` trace grows with message size times task count. Enable it for debugging, not as a default. The recorded shape is specified under [the trace object](../data-api.md#the-trace-object).

## Related

- [Trace persistence settings](../configuration/trace-storage.md): the global policy this overrides.
- [Traces and async processing](../../operate/run/traces.md): choosing a mode.
- [Data API › The trace object](../data-api.md#the-trace-object): the recorded shape of `task_details`.
- [Channel configuration](./index.md): every key, with its page.
