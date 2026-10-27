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

The repo is a cargo workspace: `crates/orion-server` (the
runtime), `crates/orion-cli` (the CLI — see its own `CLAUDE.md`), three shared
library crates — `crates/orion-api` (the wire contract),
`crates/orion-client` (the HTTP transport) and `crates/orion-definitions`
(the static definition primitives) — `crates/orion-extension-api` (the
traits and contract types an edition implements against), `crates/orion-common`
(the runtime's shared primitives: configuration, `OrionError`, crypto,
metrics, request and trace context), `crates/orion-storage` (the state
database: pools, the embedded migrations, row models and repositories),
`crates/orion-engine` (the workflow engine: Orion's functions and operators,
the loader, connectors, the data dialect and JWT), `crates/orion-plugin-host`
(the Wasmtime sandbox, the plugin manifest and the WIT world), `crates/orion-runtime`
(the serving node: channels and guards, the runtime generation and reload,
cron, the trace queues, cluster coordination, the v2 extensions),
`crates/orion-authoring` (definition sets, compile, fmt, clippy and the
validation every definition passes), `crates/orion-http` (the HTTP layer:
the Axum router, every route, the middleware stack and the OpenAPI document) and
`crates/orion-plugin-sdk` (the guest crate a plugin author links against; it
depends on nothing else here). The end-to-end suite that drives the server with the CLI lives at the
repo root (`tests/e2e/`). Bare cargo commands target the server via
`default-members`.

```
crates/orion-server/
 src/
  main.rs              # Binary entrypoint (thin wrapper over cli.rs)
  lib.rs               # Public module declarations
  bootstrap.rs         # Startup sequence: config -> pools -> repos -> engine -> server
  cli.rs               # CLI subcommands (migrate, lint, dry-run, test, preflight, ...)
 tests/
  integration/         # Consolidated integration test binary (main.rs + *_test.rs modules)
  integration/common/  # Test helpers (test_app, json_request, body_json)
  cluster/             # Multi-node cluster suite (container-gated, #[ignore])
  schema_parity.rs     # Cross-backend schema comparison (container-gated)
  storage_postgres.rs  # PostgreSQL repository suite (container-gated)
  storage_mysql.rs     # MySQL repository suite (container-gated)
  metrics_exposition.rs # Rendered /metrics assertions (own process — the recorder is global)
  benchmark/           # bench.sh + fixtures (hey-based performance scenarios)
crates/orion-common/   # Shared runtime primitives: config, OrionError, crypto, metrics, contexts
crates/orion-storage/  # State database: pools, models, repositories
crates/orion-engine/   # Workflow engine: functions, operators, loader, connectors, query, jwt
crates/orion-plugin-host/ # WebAssembly plugins: Wasmtime sandbox, manifest, handler, limits, wit/
crates/orion-runtime/  # Serving node: channel/, runtime/, cron/, queue/, cluster/, extensions/, auth
crates/orion-authoring/ # Authoring: definitions/ (check, compile, fmt, clippy), validation/
crates/orion-http/     # HTTP layer: routes/ (admin, v2_admin, data, openapi), middleware, rate limiting
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
`crates/orion-storage/migrations/{sqlite,postgres,mysql}/`.
Each set starts at `001_v2_baseline.sql`, the whole schema of an Orion 2 state
database; Orion 1.x databases are refused at startup rather than upgraded. To
bootstrap the set for a new backend, copy an existing backend's baseline and
adapt the dialect by hand (types) — there is no generator.

There are no triggers. A table's `updated_at` is written by the UPDATE itself
(`helpers::updated_now()` in `storage/repositories/`), so a new UPDATE on a
table with that column must set it; `schema_parity.rs` refuses a trigger on any
backend, and the repository contract (`tests/support/repository_contract.rs`)
checks that each UPDATE moves the column.

**The three sequences are independent after the baseline.** Each backend has its
own directory and its own numbering, and a change that applies to only two of
them advances only those two — so past `001` the same number can mean different
things per backend. Never assume the numbers line up, and **name migrations
rather than number them** in commit messages, runbooks and docs.
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
async fn an_order_over_the_limit_is_flagged() {
    let app = common::test_app().await;

    // Deploys the workflow behind a sync HTTP channel named after it, through
    // the v2 path an operator uses (definitions + an applied deployment).
    common::create_and_activate_channel(
        &app,
        "orders",
        common::workflow_with_tasks("orders", json!([{
            "id": "flag", "name": "flag",
            "function": {"name": "map", "input": {"mappings": [
                {"path": "data.flagged", "logic": {">": [{"var": "data.total"}, 100]}}
            ]}}
        }])),
    )
    .await;

    let req = json_request("POST", "/api/v2/data/orders", Some(json!({"data": {"total": 250}})));
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["data"]["flagged"], true);
}
```

### Unit tests

Add unit tests beside the code in the crate that owns it: an inline `#[cfg(test)] mod tests` for a few cases, a sibling `tests.rs` (beside a `mod.rs`) or `foo_tests.rs` (beside `foo.rs`, via `#[path]`) when the module grows. `crates/orion-common/src/config/mod.rs` and `crates/orion-common/src/errors_tests.rs` are examples.

### Container-gated tests

Tests that need a real backend (PostgreSQL, MySQL, Vault, or a
multi-node cluster) are `#[ignore]`d, so `cargo test` skips
them locally. CI runs every one of them; run them yourself with Docker up
([`tests/README.md`](crates/orion-server/tests/README.md) explains the six-binary layout, why the
backend is pinned per process, and how the CI filters are kept drift-free):

```bash
cargo test --test storage_postgres -- --ignored     # testcontainers spin up the DB
cargo test --test cluster -- --ignored              # the 14-contract multi-node suite
cargo test --test integration -- --ignored <filter> # container-gated integration modules
```

The `#[ignore]` → CI-filter mapping is self-enforcing
(`crates/orion-docs-check/tests/drift/ci_filter_drift_test.rs`): a container-gated module missing
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
./tests/e2e/run.sh 03 04    # a subset, by suite-number prefix
```

### Doc tests

CI also runs `cargo test --doc` and `RUSTDOCFLAGS="-D warnings" cargo doc
--workspace --no-deps --lib` — a documented example that stops compiling, or a broken
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

### Adding a workflow function

Implement the typed `FunctionConfig`, `dataflow_rs::Function` and
`functions::definition::OrionFunction` in one module under `orion-engine`.
The config supplies field names, order, descriptions, requiredness and output
defaults. Put retry policy, static validation and sparse field metadata in
`OrionFunction::DEFINITION`, and construct the handler in `from_deps`.
The associated `Connector` type supplies both the authoring rule and the
runtime's typed connector lookup.

Declare the module and add `module::FunctionType` to the `functions!`
inventory in `functions/mod.rs`. That inventory registers the handler and
builds its catalogue entry. Declare its offline stub target when it needs a backend; use the
declaration's `output_path` helper so real calls and stubs share null handling.
Add behavior tests beside the implementation and document the function under
`docs/src/reference/functions/`.

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

1. **Version bump.** Every crate shares `workspace.package.version` in the
   root `Cargo.toml`; `cargo release <level>` rewrites it and every
   `[workspace.dependencies]` requirement together, then tags. A bare
   `v`-prefixed tag (`v1.0.0`) releases every package at that version, and
   lockstep is what keeps the rider crates (`orion-api`, `orion-client`,
   `orion-definitions`, `orion-extension-api`, `orion-common`, `orion-storage`, `orion-engine`, `orion-plugin-host`, `orion-runtime`, `orion-authoring`, `orion-http`, `orion-plugin-sdk`) publishable — their version always moves with a
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
