# Orion v1.0.0 release benchmark

Captured 2026-08-09 on branch `v1.0.0` (release build, 30s/scenario, c=50).

**Hardware:** Apple Mac Mini, M2 Pro, 10 cores (6P + 4E), 16 GB RAM,
macOS 26.6 (25G72). This is the maintainer's desktop, chosen deliberately for
the 1.0 record, not the fully isolated host `RELEASING.md` §C13 prescribes —
a coding-agent session was resident during the runs (file edits only, no
builds). Treat small deltas against other records accordingly.

**Not in this record:** the cluster scenario (`bench.sh cluster`, scenario G).
The capture host has no Docker, so the HA compose stack could not run. The
single-instance record matches the v0.2.0 scenario coverage exactly.

## Headline numbers

| Scenario | Req/sec | Avg (ms) | P99 (ms) | Errors |
|---|---:|---:|---:|---:|
| A: Health check baseline | 58801.29 | 1.50 | 1.70 | 0 |
| B: Simple workflow (1 log task) | 5655.20 | 8.70 | 12.60 | 0 |
| C: Complex workflow (4 tasks) | 5150.68 | 9.70 | 22.20 | 0 |
| D: Loaded estate (12 channels) | 5166.78 | 9.60 | 18.90 | 0 |
| E: Concurrency c=1 | 4318.44 | 0.20 | 3.60 | 0 |
| E: Concurrency c=10 | 5026.84 | 2.00 | 36.00 | 0 |
| E: Concurrency c=50 | 5034.20 | 9.90 | 15.40 | 0 |
| E: Concurrency c=100 | 4932.66 | 20.20 | 30.40 | 0 |
| F: Reload under load (56x) | 5019.14 | 9.90 | 15.80 | 0 |

## Compared to v0.2.0 (`../v0.2.0/`)

The runs are not strictly comparable — v0.2.0 was 15s/scenario on an earlier
macOS, and scenario D was redefined for 1.0 (the old "12 workflows on one
channel" exercised the same code path as B; the new D is a 12-channel
estate). With that said:

| Scenario | v0.2.0 req/s | v1.0.0 req/s | Δ | v0.2.0 P99 | v1.0.0 P99 |
|---|---:|---:|---:|---:|---:|
| A: Health baseline | 43,501.8 | 58,801.3 | **+35.2%** | 2.5 ms | 1.7 ms |
| B: Simple workflow (1 task) | 7,445.6 | 5,655.2 | −24.0% | 16.7 ms | **12.6 ms** |
| C: Complex workflow (4 tasks) | 6,052.9 | 5,150.7 | −14.9% | 25.5 ms | **22.2 ms** |
| D: (redefined — not comparable) | — | 5,166.8 | — | — | 18.9 ms |
| E: c=1 | 4,646.6 | 4,318.4 | −7.1% | 3.2 ms | 3.6 ms |
| E: c=10 | 5,296.4 | 5,026.8 | −5.1% | 21.6 ms | 36.0 ms |
| E: c=50 | 6,428.5 | 5,034.2 | −21.7% | 17.0 ms | **15.4 ms** |
| E: c=100 | 6,403.9 | 4,932.7 | −23.0% | 23.6 ms | **30.4 ms** |
| F: Reload under load | 6,464.7 | 5,019.1 | −22.4% | 16.9 ms | **15.8 ms** |

Reading it honestly: the health path got dramatically faster, and P99 improved
on most c=50 workflow scenarios, while straight-line workflow throughput sits
15–24% below the v0.2.0 record. Known differences on the workflow hot path
since v0.2.0 include the always-on per-task timing
(`orion_task_duration_seconds` via dataflow-rs's `ExecutionObserver`) and the
channel guard work added through the 1.0 audits; the run-condition caveats
above also apply. Zero errors anywhere, including 56 hot reloads under
sustained load.

## Reproducing

```bash
BENCH_RELEASE=1 BENCH_DURATION=30s ./tests/benchmark/bench.sh
```

Pass `BENCH_OUTPUT_DIR=tests/benchmark/results/<your-tag>` to redirect output.
Per-scenario raw `hey` reports are in the sibling `.txt` files in this
directory. The cluster scenario needs the HA compose stack:
`docker compose -f docker-compose.ha.yml up -d`, then
`BENCH_RELEASE=1 ./tests/benchmark/bench.sh cluster`.
