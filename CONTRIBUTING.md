# Contributing to Orion

Contributions are welcome! Whether it's a bug fix, new feature, documentation improvement, or test — we appreciate the help. By participating you agree to our [Code of Conduct](CODE_OF_CONDUCT.md).

(`CLAUDE.md` at the repository root is context for AI coding agents working in this repo — human contributors can ignore it.)

## Getting Started

**Prerequisites:**

- [Rust 1.98+](https://www.rust-lang.org/tools/install) (Orion uses the 2024 edition; MSRV is 1.98)
  — and the MSRV now tracks [Wasmtime's](https://docs.wasmtime.dev/stability-release.html) (stable minus two), which the plugin sandbox depends on; a Wasmtime upgrade may move it
- SQLite (bundled — no separate install needed)

**Clone and build:**

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion
cargo build
cargo test
```

If all tests pass, you're ready to go.

## Development Workflow

```bash
cargo build                        # Build (all capabilities compiled in — no feature flags)
cargo build --release              # Release build
cargo test                         # Run the server suite (default-members)
cargo test --workspace             # Server + CLI suites — what CI runs
cargo test <test_name>             # Run a single test by name
cargo test --test integration      # Run the consolidated integration test binary
cargo clippy --workspace --all-targets  # Lint (matches CI)
cargo fmt --all                    # Format code
```

Run `cargo clippy` and `cargo fmt` before committing — both must pass cleanly.

## Project Structure

The repo is a cargo workspace of five crates: `crates/orion-server` (the
runtime), `crates/orion-cli` (the CLI — see its own `CLAUDE.md`), two shared
library crates — `crates/orion-api` (the wire contract) and
`crates/orion-client` (the HTTP transport) — and `crates/orion-plugin-sdk`
(the guest crate a plugin author links against; it depends on nothing else
here). The end-to-end suite that drives the server with the CLI lives at the
repo root (`tests/e2e/`). Bare cargo commands target the server via
`default-members`.

```
crates/orion-server/
 src/
  main.rs              # Binary entrypoint (thin wrapper over cli.rs)
  lib.rs               # Public module declarations
  bootstrap.rs         # Startup sequence: config -> pools -> repos -> engine -> server
  cli.rs               # CLI subcommands (migrate, lint, dry-run, test, preflight, ...)
  preflight.rs         # `orion-server preflight`: scan stored entities for 1.0 breaks
  package_cli.rs       # `orion-server package`: export/lint/plan/apply/diff promotion CLI
  channel/             # Channel registry, config, routing, deduplication, auth guards
  cluster/             # Cluster mode: config-epoch watcher, background-job leases
  cron/                # Scheduled channels: reconciler, workers, occurrence ledger
  config/              # Configuration loading (TOML + ORION_SECTION__KEY env overrides)
  connector/           # Connector types, registry, circuit breakers, pool caching
  definitions/         # Definition sets: the JSON front end, analysis, fmt, clippy, compile
  engine/              # Dataflow engine & custom function handlers (functions/)
  errors.rs            # OrionError enum -> HTTP response mapping
  jwt/                 # Shared JWT core: verify, sign, JWKS cache
  kafka/               # Kafka producer & consumer
  metrics.rs           # Prometheus metrics collection
  plugin/              # WebAssembly plugins: Wasmtime sandbox, manifest, handler, limits
  query/               # Portable data dialect (data_query / data_write -> SQL/Mongo/ES)
  queue/               # Async trace processing, DLQ retry
  runtime/             # The published generation (engine + channel estate), reload, task supervisor
  server/              # Axum routes (routes/), middleware, AppState
  storage/             # Database abstraction, models, repositories (repositories/)
  validation/          # Input validation, SSRF protection
 tests/
  integration/         # Consolidated integration test binary (main.rs + *_test.rs modules)
  integration/common/  # Test helpers (test_app, json_request, body_json)
  cluster/             # Multi-node cluster suite (container-gated, #[ignore])
  schema_parity.rs     # Cross-backend schema comparison (container-gated)
  storage_postgres.rs  # PostgreSQL repository suite (container-gated)
  storage_mysql.rs     # MySQL repository suite (container-gated)
  metrics_exposition.rs # Rendered /metrics assertions (own process — the recorder is global)
  benchmark/           # bench.sh + fixtures (hey-based performance scenarios)
 migrations/           # SQLite / Postgres / MySQL migrations (embedded at compile time)
crates/orion-cli/
 src/commands/         # clap subcommands (one file per command group)
crates/orion-api/      # Shared wire contract (DTOs, enums, error envelope)
crates/orion-client/   # Shared HTTP transport (OrionClient, endpoint paths)
crates/orion-plugin-sdk/ # Guest SDK for plugin authors (WIT bindings, export macro)
tests/e2e/             # Shell end-to-end suites: orion-cli against orion-server (`just e2e`)
examples/              # Deployable packages, plugin sources, offline workflow tests, e2e use cases
deploy/                # Helm chart (helm/orion), HA compose drill (ha/)
docs/                  # mdBook documentation (published to docs.goplasmatic.io)
```

## Your First Contribution

Issues labelled [`good first issue`](https://github.com/GoPlasmatic/Orion/labels/good%20first%20issue)
are scoped to be finishable without knowing the whole codebase, and
[`help wanted`](https://github.com/GoPlasmatic/Orion/labels/help%20wanted)
marks work we'd love a hand with. Not sure where something lives? Ask in
[Discussions](https://github.com/GoPlasmatic/Orion/discussions) and a
maintainer will point you at the right module. Documentation fixes are real
contributions — the book under `docs/src/` ships with the same review bar as
code.

## Making Changes

1. **Fork** the repository and create a branch from `main`
2. **Make your changes** — keep commits focused and atomic
3. **Write tests** for new functionality
4. **Run the checks.** With [`just`](https://github.com/casey/just) installed,
   one command runs the full CI-equivalent gate — fmt, clippy with
   `-D warnings`, tests, doc-tests and rustdoc:
   ```bash
   just check
   ```
   Without `just`, the equivalent is the four commands it wraps. Note the
   `--workspace` flags: bare `cargo clippy` / `cargo test` only cover the
   server, because `default-members` points at it, so they silently skip
   `orion-cli` — which CI does not.
   ```bash
   cargo fmt --all --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   cargo test --doc
   ```
   The `justfile` at the repo root also carries `just test-containers`,
   `just openapi`, `just docs` and `just e2e`.
   If you changed the HTTP API (routes or request/response schemas), regenerate
   the checked-in OpenAPI spec — a test fails if it's stale:
   ```bash
   cargo run -- dump-openapi > docs/openapi.json
   ```
5. **Submit a pull request** with a clear description of what changed and why

### Database migrations

The shipped migration files are **checksum-frozen** once released (sqlx records
a checksum per applied migration), so never edit an existing `NNN_*.sql` — add
a new numbered file to **each** of
`crates/orion-server/migrations/{sqlite,postgres,mysql}/`.
To bootstrap the migration set for a new backend, copy the newest existing
backend's `001_initial.sql` and adapt the dialect by hand (types, triggers,
view syntax) — that is how the shipped sets were produced; there is no
generator.

**The three sequences are independent.** Each backend has its own directory and
its own numbering, and a change that applies to only two of them advances only
those two — so the same number means different things per backend (`004` is
`cluster_coordination` on SQLite, `bigint_columns` on PostgreSQL,
`active_immutability` on MySQL). Never assume the numbers line up, and **name
migrations rather than number them** in commit messages, runbooks and docs.
`orion-server migrate --dry-run` prints backend, number and name together for
exactly this reason. `crates/orion-server/tests/schema_parity.rs` is what catches a change applied
to two backends out of three.

Write migrations **expand/contract** style: during a rolling deploy, old and
new binaries briefly share one database, so a release may only *add* schema
(columns, tables, indexes) alongside code that tolerates both shapes; drop or
rename the old shape in a *later* release, once no running replica depends on
it. Cluster deployments run `orion-server migrate` as a deploy step
(`storage.auto_migrate = false`), and replicas refuse to boot on a pending
migration.

## Testing

The full map of the test estate — every suite, what it covers, what it
needs, and which CI job runs it — is [`TESTING.md`](TESTING.md). The layers
you will touch most often:

### Integration tests

Integration tests use an in-memory SQLite database and the full Axum router — no running server needed. New test files go in `tests/integration/` and are declared as modules in `tests/integration/main.rs`. The test helpers in `tests/integration/common/mod.rs` provide:

- `test_app()` — creates a ready-to-use `Router` with in-memory DB, repos, and engine
- `json_request(method, uri, body)` — builds an HTTP `Request<Body>` with JSON content-type
- `body_json(response)` — extracts and parses the response body as `serde_json::Value`

**Example pattern:**

```rust
#[tokio::test]
async fn test_my_feature() {
    let app = common::test_app().await;

    let req = json_request("POST", "/api/v1/admin/workflows", Some(json!({
        "workflow_id": "test-workflow",
        "name": "Test Workflow",
        "condition": true,
        "tasks": []
    })));

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Responses are wrapped in a `data` envelope; new workflows start as drafts.
    let body = body_json(response).await;
    assert_eq!(body["data"]["name"], "Test Workflow");
    assert_eq!(body["data"]["status"], "draft");
}
```

### Unit tests

Add unit tests inline in the relevant module using `#[cfg(test)]` blocks. See `src/config/mod.rs` or `src/errors.rs` for examples.

### Container-gated tests

Tests that need a real backend (PostgreSQL, MySQL, MongoDB, Elasticsearch,
Kafka, Redis, or a multi-node cluster) are `#[ignore]`d, so `cargo test` skips
them locally. CI runs every one of them; run them yourself with Docker up
([`tests/README.md`](crates/orion-server/tests/README.md) explains the six-binary layout, why the
backend is pinned per process, and how the CI filters are kept drift-free):

```bash
cargo test --test storage_postgres -- --ignored     # testcontainers spin up the DB
cargo test --test cluster -- --ignored              # the 14-contract multi-node suite
cargo test --test integration -- --ignored <filter> # container-gated integration modules
```

The `#[ignore]` → CI-filter mapping is self-enforcing
(`tests/integration/ci_filter_drift_test.rs`): a container-gated module missing
from CI's name filters fails the build, in both directions.

### End-to-end suite

`tests/e2e/` at the repo root drives a real `orion-server` binary over HTTP
with the `orion-cli` binary — both built from your tree — through 12 shell
suites. The last suite is data-driven: scenario cases in
`examples/use-cases/` deploy the shipped example packages, and
runtime-behaviour cases live in `tests/e2e/cases/`. It needs `jq` and
`curl`; CI runs it on every PR (the `cli-e2e` job):

```bash
just e2e                    # or: ./tests/e2e/run.sh
./tests/e2e/run.sh 07 08    # a subset, by suite-number prefix
```

### Doc tests

CI also runs `cargo test --doc` and `RUSTDOCFLAGS="-D warnings" cargo doc
--no-deps --lib` — a documented example that stops compiling, or a broken
intra-doc link, fails a PR even though a plain `cargo test --all-targets`
never executes either.

### Running a single test

```bash
cargo test test_my_feature              # By test name
cargo test --test integration           # Run the whole integration binary
```

## Code Style

- **Rust 2024 edition** — the codebase uses let-chains (`if let Some(x) = a && let Some(y) = b`)
- **`cargo fmt`** — all code must be formatted
- **`cargo clippy`** — all warnings must be resolved
- **Error handling** — use `OrionError` variants from `src/errors.rs` for new error cases
- **Async** — all repository traits use `async_trait`; keep I/O operations async

### Comments citing an item ID

Around 900 comments across `src/` open with a short code — `N10`, `R13`, `F35`,
`W8`, `S15`. These are items from the pre-1.0 audits, and the code is the reason
the comment exists rather than a note about it. Resolve one with:

```bash
git log --grep=N10          # the commit that closed it, with the full rationale
```

The IDs are a historical record — safe to ignore entirely when reading or
writing code. Chasing one needs the full history: on a shallow clone run
`git fetch --unshallow` first.

Every commit that closed an item names it in the message body, so the history is
the index. The `proposal.md` trackers that held open items are retired — both
audits closed out before the 1.0 tag, and the last four carried items live where
they are actioned: P12 and C13 in `RELEASING.md`'s release procedure, P13's
revisit criterion in `dist-workspace.toml`, B6 an external listing process. An
ID's absence from the tree means the work is done (or externally tracked), not
that the reference is stale.

Two rules keep this readable. Each comment must stand on its own without the ID:
the prefix points at the history for a reader who wants the argument, and is
never the only place the reason is written. And when a comment's claim stops
being true, correct the comment — the ID records why the code was written, not a
promise that it still behaves that way.

New comments do not need an ID. The scheme is a record of how 1.0 was reached,
not a convention to keep feeding.

## Cutting a Release

Releases are tag-driven. Pushing a version tag runs three workflows —
`release.yml` (binaries, installers, Homebrew), `docker-release.yml`
(multi-arch image, Helm chart, signing/attestation), and
`crates-publish.yml` (crates.io; skips prerelease tags) — and all three
gate on a successful CI run for the tagged commit (`ci-gate`), so a tag can
never outrun a red build. Note that crates.io carries `orion-server` and the
rider crates only — `orion-cli` ships through the installers, the Homebrew
tap and GHCR, because its crates.io name belongs to an unrelated crate. The
secrets they need and the full procedure are in `RELEASING.md`.

1. **Version bump.** All five crates share `workspace.package.version` in the
   root `Cargo.toml`; `cargo release <level>` rewrites it and the two
   `[workspace.dependencies]` requirements together, then tags. A bare
   `v`-prefixed tag (`v1.0.0`) releases every package at that version, and
   lockstep is what keeps the rider crates (`orion-api`, `orion-client`,
   `orion-plugin-sdk`) publishable — their version always moves with a
   release, so the skip-if-present rider publish can never leave
   `orion-server` resolving older crates.io content. The Helm chart needs **no** manual bump —
   `docker-release.yml` stamps both the chart version and `appVersion` from
   the tag at publish time; the in-tree `Chart.yaml` value is a development
   placeholder.
2. **CHANGELOG cut.** Fold `## [Unreleased]` into a dated
   `## [X.Y.Z] - YYYY-MM-DD` heading (merge per category), leave an empty
   `[Unreleased]` on top, and check the compare links at the foot of the file
   name the new tag.
3. **Tag and push.** Push the tag by its full `refs/tags/` name. Releases are
   cut from `main` and no release branch exists today, but the form is
   unambiguous whatever a branch is called — and a branch named after its
   version, which is how the `v1.0.0` branch worked, makes a bare
   `git push origin vX.Y.Z` fail as an ambiguous refspec.
   ```bash
   git tag vX.Y.Z && git push origin refs/tags/vX.Y.Z
   ```
4. **Watch the release land.** `release.yml` builds each dist target
   (macOS/Windows are pre-proven per PR by `cross-os-build.yml`, but the
   release build is the real one) and publishes the GitHub Release;
   `docker-release.yml` builds per-platform images, merges the manifest, then
   signs and attests **the merged manifest digest** and pushes the chart to
   `oci://ghcr.io/goplasmatic/charts`.
5. **Verify what shipped.** The merge job's final step already runs
   `cosign verify` and `gh attestation verify` against the published tag and
   fails the release if nothing verifiable landed — confirm it ran green
   rather than assuming. `dry_run` dispatches skip the merge job entirely, so
   a scratch tag is the only true rehearsal of this half.

Expand/contract migration discipline across releases is described under
[Database migrations](#database-migrations) — a release may only *add* schema
alongside code that tolerates both shapes.

## License

By contributing, you agree that your contributions will be licensed under the [Apache-2.0 License](LICENSE).
