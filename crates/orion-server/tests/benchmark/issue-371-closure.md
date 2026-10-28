The dataflow-rs 4.0.3 run view addresses the remaining per-step context rebuild
reported here. The writer retains its arena view across conditions, templated
fields and synchronous tasks, splices writes into it and checkpoints marked
paths. See the [v4 run writer](https://github.com/GoPlasmatic/dataflow-rs/blob/v4.0.3/src/state/run.rs).

We measured a Community loop with a real Wasm plugin, templated fields,
conditions and a growing replay array. At 10 sweeps with tracing off, increasing
unrelated context from 0 to 16,384 objects leaves allocation requests and bytes
unchanged: 438 requests / 106,883 bytes. At 1,000 sweeps, the added context
increases allocated bytes by 12,308,000, compared with about 1.4 GB for one
JSON-sized full-context copy per sweep. This supports retiring the original
unconditional per-step rebuild claim. View traversal, checkpoints and arena
rebuilds still cost work; elapsed time grows with context size.

Evidence at commit `5b9e37d670ef44432093ab5f1095deb658203654`:

- [Report, workload boundaries, results and reproduction commands](https://github.com/GoPlasmatic/Orion/blob/5b9e37d670ef44432093ab5f1095deb658203654/crates/orion-server/tests/benchmark/RUN_VIEW.md)
- [Raw 36-case dataset with three samples per case, source hashes and environment](https://github.com/GoPlasmatic/Orion/blob/5b9e37d670ef44432093ab5f1095deb658203654/crates/orion-server/tests/benchmark/results/v2-run-view-2026-10-28/measurements.json)
- [Runnable loop and fan-out workload](https://github.com/GoPlasmatic/Orion/blob/5b9e37d670ef44432093ab5f1095deb658203654/crates/orion-plugin-host/examples/run_view.rs)

The dataset uses an instrumented **dev** build on an Apple M2 Pro with Rust
1.99.1. We checked workload outputs in all 36 cases. We also verified the four
recorded source hashes, all 108 samples and medians, and the report's 16 table
rows against the committed dataset. A fresh 36-case, three-round run reproduced
the median allocation requests and bytes in every case, with exact-output checks.

The original model workload belongs to the removed Community model subsystem,
so these results do not provide a comparable v3 speedup measurement. Optimized
production performance and HTTP/connector benchmarks remain in #385. Standalone
`ExprEngine::eval` calls need their own profiled workload if they remain costly.

This evidence supports closing the original workflow rebuild report as
addressed by v4. It does not establish constant execution cost or release
readiness.
