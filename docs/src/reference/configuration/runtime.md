<!-- description: The runtime settings of orion-server: the engine, ingest bounds, the trace queue and trace persistence, audit retention, query bounds and cron. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Runtime settings

The engine, ingest, traces, audit, query bounds and the cron scheduler. Every table on these pages carries the wire name, the default the code uses, and the `ORION_*` override.

| Page | Holds |
|---|---|
| [Engine settings](./engine.md) | channel_call depth and timeout, loop and cache bounds, sticky rollouts, connector load failures, the ops budget and the circuit breaker. |
| [Ingest settings](./ingest.md) | the data-plane request body bound, separate from the admin plane's own limit, with its default and override. |
| [Trace queue settings](./trace-queue.md) | async workers and buffer, retention and cleanup, per-trace deadlines, result and memory caps, and the dead-letter retry loop. |
| [Trace persistence settings](./trace-storage.md) | the sync, async, batch and off persistence modes, sampling, errors-only, queue capacity and overflow behaviour. |
| [Audit log settings](./audit.md) | audit log retention and cleanup, the pending-write queue bound, and the shutdown drain timeout, with their defaults. |
| [Query and write bounds](./query.md) | default and maximum page size, the skip cap, rows per bulk write, and the unfiltered-mutation opt-in. |
| [Cron scheduler settings](./cron.md) | scheduler capacity on this node, poll interval, workers, claim batch and lease, heartbeat, misfire grace and catch-up ceiling. |

## Related

- [Server configuration](./index.md): every section, by what you are configuring.
- [How settings are resolved](./how-settings-are-resolved.md): defaults, the file, and the environment.
- [Production checklist](../../operate/production-checklist.md): which settings to change before real traffic.
