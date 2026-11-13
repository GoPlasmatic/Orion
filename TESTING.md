# Testing Orion

The complete map of the repo's test estate: every layer, what it covers,
what it needs, how to run it, and which CI job gates it. Detail lives next
to the code — [`crates/orion-server/tests/README.md`](crates/orion-server/tests/README.md)
for the server's cargo test binaries — this file is the top of that tree.
The CLI's suites and the end-to-end suite live with the CLI in
[Orion-cli](https://github.com/codetiger/Orion-cli); the guards that hold the documentation to the code live
with the book in [GoPlasmatic/Orion-docs](https://github.com/GoPlasmatic/Orion-docs).

Three principles shape the setup:

1. **The default path needs nothing.** `cargo test --workspace` runs on a
   bare machine: in-memory SQLite, no Docker, no network, no running server.
   Everything that needs more is explicitly gated (`#[ignore]` + Docker, or
   a separate shell runner).
2. **Local commands mirror CI.** Every `just` recipe is the same invocation
   its CI job runs, so a green `just check` predicts the PR gate.
3. **Drift is guarded, not policed.** Committed artifacts that can go stale
   (CI's container-test filters, `config.toml.example`, the release stamps
   here; the OpenAPI spec and the reference pages in Orion-docs) have
   tests that fail when they drift — keeping them honest is enforced, not
   remembered.

## Quick reference

| When | Run |
|------|-----|
| Before every commit | `cargo fmt` + `cargo clippy --workspace --all-targets` |
| Before a PR | `just check` (fmt, clippy `-D warnings`, workspace tests, doctests, rustdoc) |
| Touched the HTTP API surface | `cargo test --test integration openapi_test admin_guard_test`, then the matching change to the committed spec in Orion-docs (`just openapi` prints what this build serves) |
| Touched server ⇄ CLI interaction, `orion-api`, or `orion-client` | Orion-cli's suites, its `tests/e2e` included, built against this tree (its README, *Contributing*, shows the local `[patch]`) |
| Touched migrations or storage | `just test-containers` (needs Docker) |
| Touched workflow functions / engine | `cargo test --test integration` + `cargo test -p orion-engine` |
| Touched the cron scheduler, the ledger, or a cron channel's validation | `cargo test --test integration cron_` + `cargo test -p orion-server --lib cron:: channel::cron storage::repositories::cron` — the DST rules, the misfire policies and the claim/singleton SQL are unit-tested where they are pure; the integration tests drive the real loops against a per-second schedule |
| Touched anything that walks a workflow's `tasks` | `cargo test -p orion-server --lib engine::steps` — a walk that misses task groups fails nothing else |
| Touched what a workflow may name, or how one is screened at load | `cargo test --test integration function_schema_test channel_load_refusal_test` — the create-time gate and the load-time screen are checked against a real engine, not against each other |
| Touched the authoring layer (`$from`, `use`, a `compile` pass) | `cargo test -p orion-authoring definitions::compile` + `cargo test --test integration source_form_refusal_test`, and Orion-cli's `compile_test` — a pass's `residue()` is read three ways (empty after compiling, `compile`'s report, the admin API's refusal), so a pass that only satisfies one of them is a passing test away from shipping |
| Touched `[vars]` / `[secrets]`, or anything a message carries into a trace | `cargo test --test integration secrets_and_vars_test` — the leak sweep asserts a secret reaches no recording surface *and* carries its own controls, so an assertion that stops seeing anything fails rather than passing quietly |
| Touched a **rider crate** (`orion-api`, `orion-client`, `orion-definitions`, `orion-extension-api`, `orion-common`, `orion-storage`, `orion-engine`, `orion-plugin-host`, `orion-runtime`, `orion-authoring`, `orion-http`, `orion-plugin-sdk`), including its manifest | `cargo package --locked --workspace` (needs a clean tree) — no version bump: every crate shares `workspace.package.version` and a release moves it |
| Touched the MSRV surface (new language features) | `cargo +1.98 check --workspace --all-targets` — `just check` does **not** cover this; CI runs it as its own job |
| Touched the examples | `cargo test -p orion-authoring` (format and clippy gates), the offline cases in [`examples/workflow-tests/`](examples/workflow-tests/) with `orion-cli test`, and `./examples/deploy.sh <name>` against a local server (`ORION_PLUGINS__ENABLED=true` for `fixed-width-statement` and `c4-referee`; it needs an installed `orion-cli`) |
| Touched the plugin sandbox, the manifest, the SDK, or the WIT | `cargo test --test integration plugin_runtime_test admin_plugins_test plugin_abi_cases_test plugin_sdk_test`; after a guest or SDK change, `crates/orion-server/tests/fixtures/plugins/build.sh` `examples/plugins/fixed-width/build.sh` and `examples/plugins/c4-rules/build.sh` (needs `wasm32-unknown-unknown` + `wasm-tools`) and commit the components — the `plugin-sdk` CI job rebuilds all three from source and runs the ABI cases against the fresh bytes |
| Release session | `RELEASING.md` — rc pipeline rehearsal, benchmarks, HA drill |

## Layer 1 — unit tests (every crate)

Inline `#[cfg(test)]` modules in every crate: `orion-server`
(config parsing, error mapping, validation/SSRF, query lowering, the
definition-set loader and its cross-reference pass, the shared-definition
`$from`/fragment resolver, the step flattener, …),
`orion-api` (wire-contract serde: envelope shapes, skew-tolerant defaults,
enum round-trips), `orion-client` (path builders, error classification), and
`orion-plugin-sdk` (the JSON boundary and the error classes of the guest ABI).

```bash
cargo test --workspace          # what CI runs (all workspace crates)
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

The tests that drive the **compiled `orion-cli` binary** — `lint` in both
file and set mode, `dry-run`, the `*.case.json` runner, `compile`, `fmt`'s and
`clippy`'s exit codes and output, `clippy --fix`, `sql check`, plugin signing
— live in [Orion-cli](https://github.com/codetiger/Orion-cli), beside the CLI.

The parser, formatter and clippy corpus checks run as `orion-authoring` unit
tests; their fixtures live under `orion-server/tests/fixtures` (Orion-cli
keeps copies of the ones its tests read).
Expression-budget cases run as `orion-engine` unit tests. Book links, layering
and public-text guards run in `orion-docs-check`.

The formatter has its own family: `json_front_end_test.rs` pins the
order-preserving parser to `serde_json` over the repository's JSON and a
hand-written conformance set (`tests/fixtures/json/`); `fmt_cases_test.rs`
runs the `tests/fixtures/fmt/<case>/{input,expected}.json` pairs, one per
layout rule; `fmt_invariants_test.rs` asserts round-trip, idempotence and the
width invariant over every shipped JSON file; `fmt_style_drift_test.rs` (docs-check) pins
the canonical key tables to the request schemas (Orion-docs holds the
reference pages to them); and `fmt_examples_test.rs` is the gate — every file
under `examples/` is in the house style, or the run fails with the diff
(`orion-cli fmt examples` fixes it).

`clippy` has the same shape: `clippy_cases_test.rs` runs every rule over
`tests/fixtures/clippy/<rule>/{fires,quiet}/` — the rule must fire on the
first and **no rule** may fire on the second — and refuses a rule without
both; and `clippy_examples_test.rs` is the acceptance gate — the example
packages must trip nothing. The clippy reference pages are held to the
registry in Orion-docs; the binary's `clippy`, `clippy --fix` (over
`tests/fixtures/clippy-fix/<case>/{before,after}/`) and `sql check` tests are
in Orion-cli.

```bash
cargo test --test integration                     # the whole binary
cargo test --test integration rest_routing_test   # one module
```

**Function records.** Each host function's answers are pinned in
`crates/*/tests/fixtures/function-records/<module>.json` (`twin::pinned`); a run
with `ORION_BLESS=1` rewrites them. A re-bless is a behaviour change: review its
diff like code, never as noise. `function_records_test` (docs-check) refuses a
record that captured the blessing machine (an absolute path, a temp directory).

**Drift guards** are part of the default run. The code-only guards — the
CI filters, crate licences, module layering, the extension API's
dependencies, function records, the README and Helm release stamps, tracker
IDs in crate rustdoc, and the code-side halves of the config, metrics,
functions, field and v2 error code and formatter-style guards — are their own
small binary, `cargo test -p orion-docs-check`
(`crates/orion-docs-check/tests/drift/`), linked against the library crates
alone. The guards that read the book or the committed OpenAPI spec (the
reference pages, routes, audit actions, the clippy pages, the mdBook pin, doc
links) moved with the book to [GoPlasmatic/Orion-docs](https://github.com/GoPlasmatic/Orion-docs), where they run against a pinned Orion
tag. The rest live here:

- `openapi_test` — the served OpenAPI document is well formed and lists every
  expected route.
- `ci_filter_drift_test` (docs-check) — every `#[ignore]`d container module
  must be named by a CI filter and vice versa (see layer 3).
- `config_docs_drift_test` (docs-check) — `config.toml.example` matches the
  struct defaults and documents every setting, and every `ORION_*` name in
  this repo's prose is real.
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
| `integration -- --ignored <filters>` | Portable-dialect parity/round-trips on Postgres/MySQL, raw-SQL backends, Vault secret resolution, column-type matrix |
| `storage_postgres` / `storage_mysql` | Orion's own repositories on the other two backends |
| `schema_parity` | The three migration sets produce agreeing schemas (columns, typed+ordered indexes, views) |
| `cluster` | Multi-node contracts: two AppStates over one shared Postgres, the v2 reconciler, job leases, the cron ledger — one occurrence per instant under two reconcilers, and a `forbid` singleton that holds across nodes (`--test-threads=1`) |

`metrics_exposition` (in-memory, no Docker) asserts the rendered `/metrics`
body from its own process, where the recorder race can't occur.

*Needs Docker. CI: `integration-containers` job, a six-leg matrix (one
runner per invocation) — kept drift-free by `ci_filter_drift_test`.*

## Layer 4 — end-to-end suite (in Orion-cli)

The shell suites that drive a real `orion-server` binary over HTTP with the
`orion-cli` binary — the one place the full contract chain (server ⇄
`orion-api` ⇄ `orion-client` ⇄ CLI rendering) is exercised end to end — live
in [Orion-cli](https://github.com/codetiger/Orion-cli), with the CLI. Its CI runs them against an `orion-server` installed
from the Orion tag the CLI is built from. Every test deploys what it needs the
way an operator does: a definition set compiled with `orion-cli compile` and
applied with `orion-cli deployments apply -d`, replacing the node's head. The
data-driven suite reads this repo's [`examples/use-cases/`](examples/use-cases/),
which deploys the shipped example packages whole (never copied) and asserts
their live responses; adding a `.json` case there extends it with no code
changes.

## Layer 5 — examples as tests

The examples are executable, and two repositories gate them:

- **Here:** `fmt_examples_test` and `clippy_examples_test` (`orion-authoring`)
  hold every example file to the house style and every package to zero clippy
  findings.
- **Offline, in Orion-cli's CI** (its `examples` job, over this repo's
  `examples/` at the tag): every package workflow passes `orion-cli lint`, and
  the [`examples/workflow-tests/`](examples/workflow-tests/) cases run each
  workflow through the real engine with stubbed connectors and the
  `fixed-width-statement` and `c4-referee` plugins running for real
  (`--plugin-dir` for each).
- **Live, in the same job:** a real server with plugins enabled (plus Postgres
  for `postgres-orders`), `quickstart.sh` run twice and `deploy.sh` for
  **every** package twice — deployability and idempotency are both asserted.
  Both scripts compile with `orion-cli compile` and deploy with
  `orion-cli deployments apply -d`; each deploy replaces the node's head, so
  every package is proven on its own, and the second run applies nothing.
- **From source:** the `plugin-sdk` CI job here rebuilds the test fixture and
  both example guests from their Rust sources on the shipped
  `orion-plugin-sdk` and runs the ABI case suite and the sandbox suites
  against the fresh fixture — so "supported SDK" is a job, not a claim.

## Layer 6 — meta-suites (quality of the tests themselves)

- **Coverage ratchet** — `coverage` job, `cargo llvm-cov` with
  `--fail-under-lines 88` (measured ~89.5%). **Scoped to the default members**
  (`orion-server` and `orion-docs-check`), so the library crates' own unit
  suites do not run under it. Raise the floor when
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
| Docker / Helm | `docker-build`, `helm` jobs | Image builds; chart lints, renders, and rejects misspelled values |
| CodeQL | `codeql.yml` | Static security analysis |
| HA rolling drill | `ha-drill.yml` (path-filtered + weekly) | SIGTERM one of two replicas under load through the LB, then the plugin and cron drills. CI also grows the stack to three nodes and verifies plugin convergence there. Setup uses compile + `deployments apply -d`; each drill restores the prior head. Until a v2 image ships, scripts select the local-build overlay. Record zero non-2xx across rolling/plugin replacement and the cron singleton/recovery verdicts against the candidate image. |
| Plugin SDK | `plugin-sdk` CI job (path-filtered) | The test fixture and both example guests rebuild from source on the shipped `orion-plugin-sdk`; the ABI case suite and the sandbox suites pass against the fresh components |

## Layer 8 — benchmarks (manual, recorded per release)

`crates/orion-server/tests/benchmark/bench.sh` (seven `hey` scenarios, plus
a `cluster` mode against the HA compose stack). Not in CI — numbers from
shared runners are noise. Run on dedicated hardware at release checkpoints;
each release's record is committed under `crates/orion-server/tests/benchmark/results/vX.Y.Z/`
(procedure: `RELEASING.md`).

The HTTP runner uses matching server/CLI binaries, compiles complete definition
sets and applies v2 deployments. `multi` puts twelve workflows and channels in
one head; `reload` alternates two definitions under traffic and checks that each
apply commits a new revision. Probes verify serving behavior before timing;
request errors fail the run. Use a short debug run to validate the setup and a
dedicated release build for performance claims. See
[the HTTP benchmark guide](crates/orion-server/tests/benchmark/README.md).

For the v4 run-view and fan-out workloads, run
`python3 crates/orion-server/tests/benchmark/run-view.py --profile dev --rounds 3`.
It drives the real plugin host directly and records engine-window allocations,
time and whole-process peak RSS. Methodology, measured results and limitations
are in [the run-view report](crates/orion-server/tests/benchmark/RUN_VIEW.md).
The behavioral regressions run in `orion-engine` with
`cargo test -p orion-engine run_view_tests`; they assert no timing thresholds.

## CI at a glance

Push/PR: `changes`, `fmt`, `lint`, `msrv`, `test`,
`integration-containers` (a six-leg matrix, one runner per container suite),
`coverage`, `deny`, `package`, `helm`, `docker-build` — plus
`mutants` on PRs, `plugin-sdk` and `ha-drill`/`cross-os-build` when their
paths change.
`changes` is the path gate for `package`; a weekly `schedule` run does the
whole lot unconditionally, including the two checks trimmed on the push path
(`msrv`'s full suite, `package`). Release tags gate on a green CI run for the
tagged commit (`ci-gate.yml`) before any artifact pipeline starts.

Every stable-Linux job restores one shared `linux-stable` cache that `test`
alone writes. Per-job caches used to total 10.24GB against GitHub's 10GB
repository limit, so each push evicted what it had just written and most jobs
recompiled from cold.

## Known gaps (accepted, with reasons)

- **The e2e suite runs SQLite only.** Backend variance is covered at the
  integration layer (layer 3); duplicating the shell suite per backend was
  judged not worth the CI cost.
- **TLS and admin auth are integration-tested but not
  e2e-tested** — Orion-cli's shell suite runs a plain-HTTP, no-auth server.
- **Benchmarks are manual** by design (see layer 8).
- **Mutation testing is scoped**, not tree-wide: full-tree `cargo mutants`
  (~819 mutants) is an offline, hours-long run.

Closing one of these is welcome — see `CONTRIBUTING.md`.
