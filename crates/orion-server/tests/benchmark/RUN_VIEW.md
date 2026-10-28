# v4 run-view evidence for Orion #371 and #372

Measured on 2026-10-28. The current Community workloads support retiring the
original per-step context rebuild and per-element message-clone reports.
No engine rewrite is indicated by these results. Issue closure still requires
posting this evidence to GitHub; no issue state was changed here.

## Reproduce

From the workspace root, with the committed Wasm fixture available:

```sh
python3 crates/orion-server/tests/benchmark/run-view.py --profile dev --rounds 3
cargo test -p orion-engine run_view_tests
```

The runner writes `target/run-view-results.json` by default. `--output` selects
another path; `--profile release` or `--profile profiling` selects an optimized
build. It uses `/usr/bin/time -l` on macOS and `-v` on Linux for process peak RSS.
The benchmark is an example target in `orion-plugin-host`, using the shared
fixture from the server tests at runtime. It does not add a server test binary.

[Raw measurements](results/v2-run-view-2026-10-28/measurements.json) contain all
36 cases, three samples per case, toolchain/hardware, base commit, dirty status
and SHA-256 hashes of the workload, allocator, runner and lockfile. This run used
Rust 1.99.1, dataflow-rs 4.0.3, datalogic-rs 5.9.0, datavalue-rs 0.3.3 and
Wasmtime 49.0.1 on an Apple M2 Pro (10 logical CPUs, 16 GiB RAM), macOS 27.0.1.
The build profile was **dev**. Times describe this instrumented development build;
they are not production throughput claims. There is no comparable v3 run in this
report; the historical model workload used capabilities removed from Community.

## Workloads and measurement boundary

Both workloads run through Orion's expression registry and real Wasm plugin
handlers. Each case is a fresh process, with compilation and linking completed
before one warm-up and three measured runs. Each run gets a fresh message.

- The loop runs 10 or 1,000 sweeps. Each sweep evaluates its condition, resolves
  a templated field through `test.fixture.upper`, and appends the result through
  an inline map to a growing replay array. Every result is checked after timing.
- Fan-out runs 10 or 1,000 items through `test.fixture.identity`, at concurrency
  ceilings of 1 and 8, collecting results in element order. The cheap guest may
  finish without yielding; the separate yielding-host regressions prove overlapping
  execution and isolation. This workload does not measure parallel CPU speedup.
- Unrelated context contains 0, 1,024 or 16,384 small objects, each with an ID and
  64-byte string. This exercises container spines and keys, rather than hiding
  copies behind one shared string. Item count and output size stay fixed within
  each context-size comparison. At 1,000 steps the input grows from 3,928 bytes
  to 1,418,225 bytes of JSON.
- Audit and audit change capture are off, matching Orion's serving run options.
  Trace capture is measured separately, off and on, without redaction or a cap.

The allocation window surrounds only `engine.run_with`. It excludes message
construction/cloning, compilation, warm-up, JSON output, verification and teardown
of the returned run/message. The example's counting allocator delegates to
`System` and counts successful alloc/alloc_zeroed/realloc requests and their full
requested sizes, not net retained bytes. It is enabled only during each window.
It does not count allocations outside Rust's global allocator, including Wasmtime
virtual-memory mappings. Current-thread execution avoids unrelated Tokio workers.
Pool reuse and arena capacity growth can still vary between samples.

Peak RSS is the **whole process** high-water mark, including cold compilation,
input setup and verification. It cannot be attributed solely to the engine window
or compared as a per-run live-heap counter. Time includes allocator instrumentation.

## Results

Selected medians below use the smallest/largest context. All three context sizes,
raw samples and both fan-out concurrency ceilings are in the JSON. MB are decimal.

| Workload | Steps | Unrelated objects | Trace | Median ms | Allocation requests | Allocated MB | Process peak RSS MB |
|---|---:|---:|---|---:|---:|---:|---:|
| loop | 10 | 0 | off | 0.90 | 438 | 0.107 | 36.00 |
| loop | 10 | 0 | on | 0.95 | 852 | 0.144 | 36.31 |
| loop | 10 | 16384 | off | 3.87 | 438 | 0.107 | 62.06 |
| loop | 10 | 16384 | on | 8.19 | 66,389 | 3.650 | 63.73 |
| loop | 1000 | 0 | off | 94.40 | 42,581 | 10.680 | 36.42 |
| loop | 1000 | 0 | on | 101.57 | 82,702 | 14.678 | 39.16 |
| loop | 1000 | 16384 | off | 110.12 | 42,047 | 22.988 | 64.91 |
| loop | 1000 | 16384 | on | 122.77 | 147,645 | 30.525 | 71.48 |
| fan | 10 | 0 | off | 0.83 | 444 | 0.123 | 36.45 |
| fan | 10 | 0 | on | 0.85 | 628 | 0.141 | 35.62 |
| fan | 10 | 16384 | off | 3.47 | 444 | 0.123 | 59.93 |
| fan | 10 | 16384 | on | 8.19 | 66,165 | 3.647 | 63.29 |
| fan | 1000 | 0 | off | 75.72 | 40,044 | 11.903 | 36.72 |
| fan | 1000 | 0 | on | 78.19 | 57,064 | 13.535 | 37.63 |
| fan | 1000 | 16384 | off | 79.33 | 40,045 | 18.260 | 64.24 |
| fan | 1000 | 16384 | on | 85.93 | 122,602 | 23.398 | 64.70 |

For 10 steps with tracing off, adding 16,384 unrelated objects changes neither
allocation requests nor allocated bytes: loop remains at 438 / 106,883 bytes;
fan-out remains at 444 / 122,510 bytes. Elapsed time still grows, so the absence
of additional heap requests does not imply zero traversal or arena work.

At 1,000 steps, the added context increases allocated bytes by 12,308,000 for
the loop and 6,356,976 for fan-out. These totals are far below copying the
additional 1.414 MB of JSON-sized context for every step (about 1.4 GB, before
container/key overhead). The [v4 run writer](https://github.com/GoPlasmatic/dataflow-rs/blob/v4.0.3/src/state/run.rs)
explains why some context-sized work remains: it checkpoints marked writes and rebuilds its arena when garbage
crosses a live-size threshold. The benchmark does not expose private counters
and cannot attribute every measured byte to a specific checkpoint.

The [v4 fan-out implementation](https://github.com/GoPlasmatic/dataflow-rs/blob/v4.0.3/src/run/fan_out.rs)
borrows the run view and folds overlays. Fan-out's measured allocation totals match at concurrency ceilings 1 and 8 for
every corresponding case. Tracing adds a context snapshot and per-step changes;
its context-sized allocations are expected and do not establish per-element
message clones. Legitimate item/result materialization and the growing replay
also allocate. No timing threshold is used in tests.

## Behavioral evidence and remaining scope

`orion-engine/src/run_view_tests.rs` uses Orion's expressions and a yielding host
function at concurrency 1 and 4. It verifies the same base for each element,
read-your-own-writes across yields, no sibling writes, temporary binding cleanup,
ordered results and final writes. It checks failures retain only the successful
prefix, with null placeholders in unfilled collection slots, and cancellation
folds no element writes. Every scenario runs with trace capture off and on;
traces validate and replay to the final data and temporary context.

These measurements and source paths support resolving the original workflow
claims in #371/#372. They do not establish constant execution cost, universal
performance parity or an improvement to standalone `ExprEngine::eval` calls.
A standalone evaluator investigation needs its own profiled workload. Optimized
release performance, production HTTP/connector workloads and the broader #385
release matrix remain separate release evidence.

Validation of this checkout: all 36 benchmark cases completed with exact-output
checks; all three engine regressions pass. The full Rust gate passes: formatting,
strict workspace Clippy, 3,107 test executions (79 ignored), default-member
doctests and warnings-as-errors workspace Rustdoc. Documentation structure/link
lint passes. Container-gated tests were not run for this workload change.
