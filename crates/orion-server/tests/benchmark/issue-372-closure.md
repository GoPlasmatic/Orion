The dataflow-rs 4.0.3 fan-out path addresses the remaining per-element context
clone reported here. Each element borrows the run view through `CallStart` and
records writes in its own overlay. The engine folds those writes in element
order. See the [v4 fan-out implementation](https://github.com/GoPlasmatic/dataflow-rs/blob/v4.0.3/src/run/fan_out.rs).

We measured a real Wasm identity plugin over 10 and 1,000 items, with concurrency
ceilings of 1 and 8, three unrelated-context sizes, and tracing off/on. With
tracing off, adding 16,384 unrelated objects leaves the 10-item case unchanged
at 444 allocation requests / 122,510 bytes. For 1,000 items, it adds 6,356,976
allocated bytes, compared with about 1.4 GB for one JSON-sized context copy per
element. Median allocation requests and bytes agree between concurrency ceilings
1 and 8 in all 12 paired cases.

Evidence at commit `5b9e37d670ef44432093ab5f1095deb658203654`:

- [Report, workload boundaries, results and reproduction commands](https://github.com/GoPlasmatic/Orion/blob/5b9e37d670ef44432093ab5f1095deb658203654/crates/orion-server/tests/benchmark/RUN_VIEW.md)
- [Raw 36-case dataset with three samples per case, source hashes and environment](https://github.com/GoPlasmatic/Orion/blob/5b9e37d670ef44432093ab5f1095deb658203654/crates/orion-server/tests/benchmark/results/v2-run-view-2026-10-28/measurements.json)
- [Runnable loop and fan-out workload](https://github.com/GoPlasmatic/Orion/blob/5b9e37d670ef44432093ab5f1095deb658203654/crates/orion-plugin-host/examples/run_view.rs)
- [Yielding-host behavior regressions](https://github.com/GoPlasmatic/Orion/blob/5b9e37d670ef44432093ab5f1095deb658203654/crates/orion-engine/src/run_view_tests.rs)

All three behavior regressions pass. They cover concurrency 1 and 4 with tracing
off/on: shared base visibility, read-your-own-writes across yields, sibling
isolation, temporary-binding cleanup, ordered results and writes, successful
prefixes on failure, cancellation without folded writes, and trace replay.
The successful concurrent case verifies four overlapping calls.

The dataset uses an instrumented **dev** build on an Apple M2 Pro with Rust
1.99.1. The cheap Wasm guest may finish without yielding, so the timed workload
does not measure parallel CPU speedup. Tracing snapshots, item/result
materialization and checkpoint work still allocate. Peak RSS covers the whole
process, including compilation and setup. We checked exact outputs in all 36
cases and verified the recorded source hashes, samples, medians and report rows.
A fresh 36-case, three-round run reproduced the median allocation requests and
bytes in every case, with exact-output checks.

This evidence supports closing the original whole-message fan-out clone report
as addressed by v4. Optimized production performance remains in #385; the removed
Community model workload has no comparable v3 baseline in this report.
