# Orion scenario B across trace-storage modes

Branch `upgrade/dataflow-rs-v3` with `[tracing.storage]` enabled. Release build, M-series 10 cores, 15s @ c=50.

Workload: scenario B fixture (`tests/benchmark/fixtures/workflows/bench_simple_log.json` against `simple_payload.json`).

| Mode | Req/sec | P99 (ms) | vs `sync` |
|---|---:|---:|---:|
| **sync** (default; current baseline) | **7,375.6** | 17.0 | — |
| **async** (bounded queue, 4 workers, drop on overflow) | **44,816.0** | 2.7 | **6.08×** |
| **batch** (50 ms flush, 100-row batches, 4 workers) | **44,073.0** | 3.0 | **5.98×** |
| **off** (no persistence — upper bound) | **50,836.9** | 2.3 | **6.90×** |

`async` and `batch` recover **~88 %** of the throughput available at `off` while still persisting every trace under typical load. The remaining ~12 % gap to `off` is the bookkeeping cost of producing the persistence-queue tasks (allocation + send) plus the worker thread CPU.

Integration tests verify correctness at low load:

- `sync_mode_persists_traces_inline`
- `async_mode_persists_eventually`
- `batch_mode_persists_eventually`
- `off_mode_skips_persistence`
- `async_endpoint_off_mode_returns_null_trace_id_with_warning`
- `errors_only_filter_drops_successful_sync_traces`
- `channel_override_persists_when_global_is_off`

Under sustained overload, `async` mode with the default `max_pending=10_000`
will drop traces (counted via `trace_dropped_total{reason="overflow"}`). To
avoid silent drops in production, prefer `batch` mode or raise `max_pending`.

Per-channel overrides land via the channel's `config.tracing` field (any
subset of `mode` / `sample_rate` / `errors_only`). Unset fields inherit the
global default.
