# Contributing to Orion

Contributions are welcome! Whether it's a bug fix, new feature, documentation improvement, or test — we appreciate the help.

## Getting Started

**Prerequisites:**

- [Rust 1.88+](https://www.rust-lang.org/tools/install) (Orion uses the 2024 edition; MSRV is 1.88)
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
cargo test                         # Run all tests
cargo test <test_name>             # Run a single test by name
cargo test --test integration      # Run the consolidated integration test binary
cargo clippy                       # Lint
cargo fmt                          # Format code
```

Run `cargo clippy` and `cargo fmt` before committing — both must pass cleanly.

## Project Structure

```
src/
  main.rs              # Binary entrypoint (thin wrapper over cli.rs)
  lib.rs               # Public module declarations
  bootstrap.rs         # Startup sequence: config -> pools -> repos -> engine -> server
  cli.rs               # CLI subcommands (migrate, lint, dry-run, test, preflight, ...)
  preflight.rs         # `orion-server preflight`: scan stored entities for 1.0 breaks
  channel/             # Channel registry, config, routing, deduplication, auth guards
  cluster/             # Cluster mode: config-epoch watcher, background-job leases
  config/              # Configuration loading (TOML + ORION_SECTION__KEY env overrides)
  connector/           # Connector types, registry, circuit breakers, pool caching
  engine/              # Dataflow engine & custom function handlers (functions/)
  errors.rs            # OrionError enum -> HTTP response mapping
  kafka/               # Kafka producer & consumer
  metrics/             # Prometheus metrics collection
  query/               # Portable data dialect (data_query / data_write -> SQL/Mongo/ES)
  queue/               # Async trace processing, DLQ retry
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
  benchmark/           # bench.sh + fixtures (hey-based performance scenarios)
migrations/            # SQLite / Postgres / MySQL migrations (embedded at compile time)
deploy/                # Helm chart (helm/orion), HA compose drill (ha/)
docs/                  # mdBook documentation (published to GitHub Pages)
```

## Making Changes

1. **Fork** the repository and create a branch from `main`
2. **Make your changes** — keep commits focused and atomic
3. **Write tests** for new functionality
4. **Run the checks:**
   ```bash
   cargo fmt && cargo clippy && cargo test
   ```
   If you changed the HTTP API (routes or request/response schemas), regenerate
   the checked-in OpenAPI spec — a test fails if it's stale:
   ```bash
   cargo run -- dump-openapi > docs/openapi.json
   ```
5. **Submit a pull request** with a clear description of what changed and why

### Database migrations

The shipped migration files are **checksum-frozen** once released (sqlx records
a checksum per applied migration), so never edit an existing `NNN_*.sql` — add
a new numbered file to **each** of `migrations/{sqlite,postgres,mysql}/`.
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
exactly this reason. `tests/schema_parity.rs` is what catches a change applied
to two backends out of three.

Write migrations **expand/contract** style: during a rolling deploy, old and
new binaries briefly share one database, so a release may only *add* schema
(columns, tables, indexes) alongside code that tolerates both shapes; drop or
rename the old shape in a *later* release, once no running replica depends on
it. Cluster deployments run `orion-server migrate` as a deploy step
(`storage.auto_migrate = false`), and replicas refuse to boot on a pending
migration.

## Testing

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
them locally. CI runs every one of them; run them yourself with Docker up:

```bash
cargo test --test storage_postgres -- --ignored     # testcontainers spin up the DB
cargo test --test cluster -- --ignored              # the 14-contract multi-node suite
cargo test --test integration -- --ignored <filter> # container-gated integration modules
```

The `#[ignore]` → CI-filter mapping is self-enforcing
(`tests/integration/ci_filter_drift_test.rs`): a container-gated module missing
from CI's name filters fails the build, in both directions.

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

Every commit that closed an item names it in the message body, so the history is
the index. `proposal.md` holds only what is still **open** — an item is deleted
from it as it ships, so a live ID will not be found there and its absence means
the work is done, not that the reference is stale.

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
never outrun a red build. The secrets they need and the full procedure are
in `RELEASING.md`.

1. **Version alignment.** `Cargo.toml`'s `version` is the release version;
   tags are `v`-prefixed (`v1.0.0`). The Helm chart needs **no** manual bump —
   `docker-release.yml` stamps both the chart version and `appVersion` from
   the tag at publish time; the in-tree `Chart.yaml` value is a development
   placeholder.
2. **CHANGELOG cut.** Fold `## [Unreleased]` into a dated
   `## [X.Y.Z] - YYYY-MM-DD` heading (merge per category), leave an empty
   `[Unreleased]` on top, and check the compare links at the foot of the file
   name the new tag.
3. **Tag and push.**
   ```bash
   git tag vX.Y.Z && git push origin vX.Y.Z
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
