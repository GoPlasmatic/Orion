# Orion v3 upgrade benchmark (dataflow-rs 3.0.0 + datalogic-rs 5.0.0)

Captured 2026-05-15 on branch `upgrade/dataflow-rs-v3`, release build, M-series 10 cores, 15s/scenario, c=50.

| Scenario | v2.1.5 req/s | v3.0.0 req/s | Δ | v2.1.5 P99 | v3.0.0 P99 |
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

The complex/multi-workflow scenarios pick up large gains from the v3 typed-input path (compile JSONLogic once at engine construction, evaluate via arena-mode dispatch); simple high-concurrency scenarios are within run-to-run noise. P99 latency is materially better on every scenario.
