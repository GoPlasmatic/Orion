# Orion v2.1.5 baseline (dataflow-rs 2.1.5 + datalogic-rs 4)

Captured 2026-05-15, branch `upgrade/dataflow-rs-v3` at the parent of the dep bump
(binary built with dataflow-rs 2.1.5, datalogic-rs 4.0.21).

Hardware: M-series, 10 cores. Profile: release. Duration: 15s per scenario. Concurrency: 50.

| Scenario | Req/sec | Avg (ms) | P99 (ms) | Errors |
|---|---:|---:|---:|---:|
| A: Health check baseline | 43031.70 | 1.20 | 2.60 | 0 |
| B: Simple workflow (1 log task) | 6575.80 | 7.50 | 33.80 | 0 |
| C: Complex workflow (4 tasks) | 4072.98 | 12.20 | 66.40 | 0 |
| D: Multi-workflow channel (12 workflows) | 3145.99 | 15.80 | 77.60 | 0 |
| E: Concurrency c=1 | 3306.93 | 0.30 | 3.70 | 0 |
| E: Concurrency c=10 | 5350.11 | 1.90 | 34.80 | 0 |
| E: Concurrency c=50 | 6759.27 | 7.40 | 18.30 | 0 |
| E: Concurrency c=100 | 6734.98 | 14.70 | 26.20 | 0 |
| F: Reload under load (29x) | 6743.00 | 7.40 | 17.60 | 0 |
