# Testing Orion

The complete map of the repo's test estate: every layer, what it covers,
what it needs, how to run it, and which CI job gates it. Detail lives next
to the code — [`crates/orion-server/tests/README.md`](crates/orion-server/tests/README.md)
for the server's six cargo test binaries, [`tests/e2e/README.md`](tests/e2e/README.md)
for the end-to-end suite — this file is the top of that tree.

Three principles shape the setup:

1. **The default path needs nothing.** `cargo test --workspace` runs on a
   bare machine: in-memory SQLite, no Docker, no network, no running server.
   Everything that needs more is explicitly gated (`#[ignore]` + Docker, or
   a separate shell runner).
2. **Local commands mirror CI.** Every `just` recipe is the same invocation
   its CI job runs, so a green `just check` predicts the PR gate.
3. **Drift is guarded, not policed.** Committed artifacts that can go stale
   (the OpenAPI spec, CI's container-test filters, config/metrics docs) have
   tests that fail when they drift — keeping them honest is enforced, not
   remembered.

## Quick reference

| When | Run |
|------|-----|
| Before every commit | `cargo fmt` + `cargo clippy --workspace --all-targets` |
| Before a PR | `just check` (fmt, clippy `-D warnings`, workspace tests, doctests, rustdoc) |
| Touched the HTTP API surface | `just openapi` (regenerate the committed spec), then `just e2e` |
| Touched server ⇄ CLI interaction, `orion-api`, or `orion-client` | `just e2e` |
| Touched migrations or storage | `just test-containers` (needs Docker) |
| Touched workflow functions / engine | `cargo test --test integration` + `just workflow-tests` |
| Touched anything that walks a workflow's `tasks` | `cargo test -p orion-server --lib engine::steps` — a walk that misses task groups fails nothing else |
| Touched what a workflow may name, or how one is screened at load | `cargo test --test integration function_schema_test channel_load_refusal_test` — the create-time gate and the load-time screen are checked against a real engine, not against each other |
| Touched the authoring layer (`$from`, `use`, a `compile` pass) | `cargo test --test integration compile_test source_form_refusal_test` — a pass's `residue()` is read three ways (empty after compiling, `compile`'s report, the admin API's refusal), so a pass that only satisfies one of them is a passing test away from shipping |
| Touched `[vars]` / `[secrets]`, or anything a message carries into a trace | `cargo test --test integration secrets_and_vars_test` — the leak sweep asserts a secret reaches no recording surface *and* carries its own controls, so an assertion that stops seeing anything fails rather than passing quietly |
| Touched a **rider crate** (`orion-api`, `orion-client`), including its manifest | `cargo package --locked --workspace` (needs a clean tree) — no version bump: all four crates share `workspace.package.version` and a release moves it |
| Touched the MSRV surface (new language features) | `cargo +1.88.0 check --workspace --all-targets` — `just check` does **not** cover this; CI runs it as its own job |
| Touched the examples | `just workflow-tests` + `./examples/deploy.sh <name>` against a local server (`ORION_PLUGINS__ENABLED=true` for `fixed-width-statement`) |
| Touched the plugin sandbox, the manifest, the SDK, or the WIT | `cargo test --test integration plugin_runtime_test admin_plugins_test plugin_abi_cases_test plugin_sdk_test`; after a guest or SDK change, `crates/orion-server/tests/fixtures/plugins/build.sh` and `examples/plugins/fixed-width/build.sh` (needs `wasm32-unknown-unknown` + `wasm-tools`) and commit the components — the `plugin-sdk` CI job rebuilds both from source and runs the ABI cases against the fresh bytes |
| Release session | `RELEASING.md` — rc pipeline rehearsal, benchmarks, HA drill |

## Layer 1 — unit tests (every crate)

Inline `#[cfg(test)]` modules across all four crates: `orion-server`
(config parsing, error mapping, validation/SSRF, query lowering, the
definition-set loader and its cross-reference pass, the shared-definition
`$from`/fragment resolver, the step flattener, …),
`orion-cli` (string helpers and benchmark statistics only — see Known gaps),
`orion-api` (wire-contract serde: envelope shapes, skew-tolerant defaults,
enum round-trips), and `orion-client` (path builders, error classification).

```bash
cargo test --workspace          # what CI runs (server + CLI + both lib crates)
cargo test -p orion-api         # one crate
```

*Needs nothing. CI: `test` job (`--workspace --all-targets --no-fail-fast`).*

## Layer 2 — server integration binary

`crates/orion-server/tests/integration/` — one binary, one module per file,
declared in `main.rs`. Each test builds a full `AppState` + Axum router over
in-memory SQLite (`common::test_app()`) and drives it with
`tower::ServiceExt::oneshot()` — no HTTP listener. This is where admin CRUD
and versioning, data-plane routing, resilience (rate limits, dedup, caching,
circuit breakers, backpressure, drain), traces/DLQ, security (auth, masking,
SSRF, redaction), TLS (in-process `rcgen` certs), the server's own CLI
subcommands, and OpenAPI are covered.

`cli_subcommands_test.rs` and `fmt_cli_test.rs` are the modules that drive the
**compiled binary** rather than the router, because the surfaces they cover —
`lint` in both file and set mode, `dry-run`, the `*.case.json` runner, and
`fmt`'s exit codes, diffs and atomic writes — are exit codes and streams, not
HTTP responses.

The formatter has its own family: `json_front_end_test.rs` pins the
order-preserving parser to `serde_json` over the repository's JSON and a
hand-written conformance set (`tests/fixtures/json/`); `fmt_cases_test.rs`
runs the `tests/fixtures/fmt/<case>/{input,expected}.json` pairs, one per
layout rule; `fmt_invariants_test.rs` asserts round-trip, idempotence and the
width invariant over every shipped JSON file; `fmt_style_drift_test.rs` pins
the canonical key tables to the request schemas and the reference pages; and
`fmt_examples_test.rs` is the gate — every file under `examples/` and
`tests/e2e/` is in the house style, or the run fails with the diff and says
`just fmt`.

`clippy` has the same shape: `clippy_cases_test.rs` runs every rule over
`tests/fixtures/clippy/<rule>/{fires,quiet}/` — the rule must fire on the
first and **no rule** may fire on the second — and refuses a rule without
both; `clippy_docs_drift_test.rs` pins `docs/src/reference/clippy.md`'s
table to the registry; `clippy_cli_test.rs` drives the binary (exit codes,
`--list`, `--explain`, JSON, the lint-error short-circuit); and
`clippy_examples_test.rs` is the acceptance gate — the example packages and
the e2e workflow fixtures must trip nothing.

```bash
cargo test --test integration                     # the whole binary
cargo test --test integration rest_routing_test   # one module
```

**Drift guards live here** and are part of the default run:

- `openapi_test` — the committed `docs/openapi.json` must match what the
  server generates (`just openapi` refreshes it).
- `ci_filter_drift_test` — every `#[ignore]`d container module must be named
  by a CI filter and vice versa (see layer 3).
- `config_docs_drift_test` / `metrics_docs_drift_test` / `docs_link_test` —
  the book's config reference, metrics list, and internal links track the
  code.
- `function_schema_test` — the two function lists Orion cannot derive, pinned
  against a **live engine**. Workflow creation is validated before an engine
  exists, so `FunctionRegistry::builtin()` and the `/admin/functions` catalogue
  have to be declared; these assert set equality with the running engine's
  `can_dispatch` / `dispatchable_functions` in both directions. A name create
  accepts that nothing dispatches fails every request at runtime; a name the
  engine dispatches that create refuses is unusable for no visible reason.

*Needs nothing. CI: `test` job.*

## Layer 3 — container-gated suites (Docker)

Everything touching a real external service is `#[ignore]`d and starts its
own [testcontainers](https://rust.testcontainers.org/) — nothing to start
manually, only Docker. Five binaries participate (the per-process backend
pin and the process-global metrics recorder are why they are separate —
see the [server tests README](crates/orion-server/tests/README.md)):

```bash
just test-containers    # all of the below, in CI's exact invocations
```

| Suite | Covers |
|-------|--------|
| `integration -- --ignored <filters>` | Portable-dialect parity/round-trips on Postgres/MySQL/Mongo/ES, raw-SQL backends, Redis cache/dedup, column-type matrix, dynamic inputs, Kafka channels |
| `storage_postgres` / `storage_mysql` | Orion's own repositories on the other two backends |
| `schema_parity` | The three migration sets produce agreeing schemas (columns, typed+ordered indexes, views) |
| `cluster` | Multi-node contracts: two AppStates over shared Postgres + Redis, epoch watching, job leases (`--test-threads=1`) |

`metrics_exposition` (in-memory, no Docker) asserts the rendered `/metrics`
body from its own process, where the recorder race can't occur.

*Needs Docker. CI: `integration-containers` job — kept drift-free by
`ci_filter_drift_test`.*

## Layer 4 — end-to-end suite (`tests/e2e/`)

The workspace-level suite at the repo root: 14 shell suites drive a real
`orion-server` binary over HTTP with the `orion-cli` binary, both built from
the same tree — the one place the full contract chain (server ⇄ `orion-api`
⇄ `orion-client` ⇄ CLI rendering) is exercised end to end at one commit.
Suite 13 is data-driven, from two case directories with distinct
roles: [`examples/use-cases/`](examples/use-cases/) deploys the shipped
example packages (workflows referenced by file, never copied) and asserts
their live responses, and `tests/e2e/cases/` holds runtime-behaviour cases
(archive quarantine, dry-run traces, secret masking, error paths). Adding a
`.json` case to either extends the suite with no code changes.

```bash
just e2e                  # build both binaries + full suite
./tests/e2e/run.sh 07 08  # a subset
```

*Needs `jq`, `curl`. CI: `cli-e2e` job, every PR. Suite map and knobs:
[`tests/e2e/README.md`](tests/e2e/README.md).*

## Layer 5 — examples as tests

The examples are executable and CI treats them as a gate (`examples` job):

- **Offline:** every package workflow passes `orion-server lint`, and the
  [`examples/workflow-tests/`](examples/workflow-tests/) cases run each
  workflow through the real engine with stubbed connectors and the
  `fixed-width-statement` plugin running for real (`just workflow-tests`,
  which passes `--plugin-dir`).
- **Live:** CI boots a real server with plugins enabled (plus Postgres for
  `postgres-orders`), runs `quickstart.sh` twice and `deploy.sh` for
  **every** package twice — deployability and idempotency are both asserted,
  and `deploy.sh` uploads and activates a package's `plugin.toml` first.
- **From source:** the `plugin-sdk` CI job rebuilds the test fixture and the
  example codec from their Rust sources on the shipped `orion-plugin-sdk`,
  runs the ABI case suite against the fresh fixture and the example cases
  against the fresh codec — so "supported SDK" is a job, not a claim.

## Layer 6 — meta-suites (quality of the tests themselves)

- **Coverage ratchet** — `coverage` job, `cargo llvm-cov` with
  `--fail-under-lines 88` (measured ~89.5%). **Scoped to `orion-server`**: the
  job runs the default members, so `orion-cli` is neither compiled nor
  instrumented and the percentage says nothing about it. Raise the floor when
  the real number moves up; never lower it to make a red build green.
- **Mutation testing** — `mutants` job (PRs only): `cargo mutants --in-diff`
  over the security/correctness-critical globs in `.cargo/mutants.toml`
  (SSRF, admin auth, masking, circuit breaker, query dialect, error
  envelope). A surviving mutant fails the PR; accept a genuine equivalent
  with `#[mutants::skip]` + a comment, not a wider glob.
- **Doc tests** — `cargo test --doc` plus `RUSTDOCFLAGS="-D warnings" cargo doc`
  (`lint` job): documented examples must compile, intra-doc links must
  resolve.

## Layer 7 — platform and deployment gates

| Gate | Where | What it proves |
|------|-------|----------------|
| MSRV | `msrv` CI job | The workspace still compiles on Rust 1.98 (`cargo check --workspace --all-targets --locked` per push; the full suite on the weekly sweep) |
| Cross-OS builds | `cross-os-build.yml` | macOS/Windows/musl targets compile (musl promotion criterion: `RELEASING.md`) |
| Packaging | `package` CI job | `cargo package --locked --workspace` verify-builds the crates.io tarballs (riders included). Gated by the `changes` job: it runs when a manifest moves or a file is added/renamed under `crates/`, the only two ways an `include` allowlist can drop a needed file, and on the weekly sweep |
| Docker / Helm / Book | `docker-build`, `helm`, `book` jobs | Image builds; chart lints, renders, and rejects misspelled values; book builds |
| CodeQL | `codeql.yml` | Static security analysis |
| HA rolling drill | `ha-drill.yml` (path-filtered + weekly) | SIGTERM one of two replicas under load through the LB → zero non-2xx; then the plugin drill (`deploy/ha/plugin-drill.sh`): a plugin activated on one node converges on the other through the config epoch, and a new version activated under load through the LB produces zero non-2xx |
| Plugin SDK | `plugin-sdk` CI job (path-filtered) | The test fixture and the example codec rebuild from source on the shipped `orion-plugin-sdk`; the ABI case suite and the example's offline cases pass against the fresh components |

## Layer 8 — benchmarks (manual, recorded per release)

`crates/orion-server/tests/benchmark/bench.sh` (six `hey` scenarios, plus a
`cluster` mode against the HA compose stack). Not in CI — numbers from
shared runners are noise. Run on dedicated hardware at release checkpoints;
each release's record is committed under `crates/orion-server/tests/benchmark/results/vX.Y.Z/`
(procedure: `RELEASING.md`).

## CI at a glance

Push/PR: `changes`, `fmt`, `lint`, `msrv`, `test`, `cli-e2e`, `examples`,
`integration-containers` (a six-leg matrix, one runner per container suite),
`coverage`, `deny`, `package`, `book`, `helm`, `docker-build` — plus
`mutants` on PRs and `ha-drill`/`cross-os-build` when their paths change.
`changes` is the path gate for `package`; a weekly `schedule` run does the
whole lot unconditionally, including the two checks trimmed on the push path
(`msrv`'s full suite, `package`). Release tags gate on a green CI run for the
tagged commit (`ci-gate.yml`) before any artifact pipeline starts.

Every stable-Linux job restores one shared `linux-stable` cache that `test`
alone writes. Per-job caches used to total 10.24GB against GitHub's 10GB
repository limit, so each push evicted what it had just written and most jobs
recompiled from cold.

## Known gaps (accepted, with reasons)

- **`orion-cli`'s rendering, help/hint and argument-plumbing code has no unit
  tests.** The crate's inline tests cover two helpers (`utils.rs` and
  `commands/benchmark/stats.rs`); output formatting and error hints are
  exercised only indirectly, through the e2e suite's assertions on command
  output.
- **The e2e suite invokes 15 of the CLI's 16 command groups.** The seven the
  lifecycle suites exercise in depth (`workflows`, `channels`, `connectors`,
  `send`, `traces`, `engine`, `health`) plus eight covered at smoke depth by
  `suites/14_read_only_commands.sh` (`functions`, `metrics`, `audit-logs`,
  `backups`, `packages`, `dlq`, `completions`, `config`) — enough to catch a
  broken output shape or envelope, not enough to call them tested. `benchmark`
  is never invoked.
- **The e2e suite runs SQLite only.** Backend variance is covered at the
  integration layer (layer 3); duplicating the shell suite per backend was
  judged not worth the CI cost.
- **TLS, admin auth, and Kafka channels are integration-tested but not
  e2e-tested** — the shell suite runs a plain-HTTP, no-auth server.
- **Benchmarks are manual** by design (see layer 8).
- **Mutation testing is scoped**, not tree-wide: full-tree `cargo mutants`
  (~819 mutants) is an offline, hours-long run.

Closing one of these is welcome — see `CONTRIBUTING.md`.
