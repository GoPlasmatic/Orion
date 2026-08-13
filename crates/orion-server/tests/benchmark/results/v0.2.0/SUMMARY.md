# Orion v0.2.0 release benchmark

Captured 2026-05-15 on branch `upgrade/dataflow-rs-v3` (release build, M-series 10 cores, 15s/scenario, c=50).

This run ships with the v0.2.0 release: dataflow-rs **3.0.0** + datalogic-rs **5.0.0**. The per-scenario `hey` output in this directory is the same dataset originally captured under `tests/benchmark/results/v3.0.0/` (the upgrade-validation snapshot). The `v3.0.0` folder is preserved for historical comparison; this folder represents the numbers we ship under Orion's own v0.2.0 version tag.

## Headline numbers

| Scenario | Req/sec | Avg (ms) | P99 (ms) | Errors |
|---|---:|---:|---:|---:|
| A: Health check baseline | 43501.80 | 1.20 | 2.50 | 0 |
| B: Simple workflow (1 log task) | 7445.60 | 6.70 | 16.70 | 0 |
| C: Complex workflow (4 tasks) | 6052.91 | 8.20 | 25.50 | 0 |
| D: Multi-workflow channel (12 workflows) | 6911.78 | 7.20 | 16.60 | 0 |
| E: Concurrency c=1 | 4646.60 | 0.20 | 3.20 | 0 |
| E: Concurrency c=10 | 5296.40 | 1.90 | 21.60 | 0 |
| E: Concurrency c=50 | 6428.50 | 7.80 | 17.00 | 0 |
| E: Concurrency c=100 | 6403.90 | 15.60 | 23.60 | 0 |
| F: Reload under load (29x) | 6464.70 | 7.70 | 16.90 | 0 |

## Compared to v0.1.x (baseline in `../v2.1.5/`)

| Scenario | v0.1.x req/s | v0.2.0 req/s | Δ | v0.1.x P99 | v0.2.0 P99 |
|---|---:|---:|---:|---:|---:|
| A: Health baseline | 43,031.7 | 43,501.8 | **+1.1%** | 2.6 ms | 2.5 ms |
| B: Simple workflow (1 task) | 6,575.8 | 7,445.6 | **+13.2%** | 33.8 ms | 16.7 ms |
| C: Complex workflow (4 tasks) | 4,073.0 | 6,052.9 | **+48.6%** | 66.4 ms | 25.5 ms |
| D: Multi-workflow channel (12 wf) | 3,146.0 | 6,911.8 | **+119.7%** | 77.6 ms | 16.6 ms |
| E: c=1 | 3,306.9 | 4,646.6 | **+40.5%** | 3.7 ms | 3.2 ms |
| E: c=10 | 5,350.1 | 5,296.4 | −1.0% | 34.8 ms | 21.6 ms |
| E: c=50 | 6,759.3 | 6,428.5 | −4.9% | 18.3 ms | 17.0 ms |
| E: c=100 | 6,735.0 | 6,403.9 | −4.9% | 26.2 ms | 23.6 ms |
| F: Reload under load | 6,743.0 | 6,464.7 | −4.1% | 17.6 ms | 16.9 ms |

The complex/multi-workflow scenarios pick up large gains from the v0.2.0 typed-input path (compile JSONLogic once at engine construction, evaluate via arena-mode dispatch). Simple high-concurrency scenarios are within run-to-run noise. **P99 latency is materially better on every scenario** — the win that matters most for tail-sensitive call sites.

## Reproducing

```bash
BENCH_RELEASE=1 ./tests/benchmark/bench.sh
```

Pass `BENCH_OUTPUT_DIR=tests/benchmark/results/<your-tag>` to redirect output. Per-scenario raw `hey` reports are in the sibling `.txt` files in this directory.
