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
  main.rs              # CLI entrypoint, startup sequence
  lib.rs               # Public module declarations
  channel/             # Channel registry, config, routing, deduplication
  config/              # Configuration loading (TOML + ORION_SECTION__KEY env overrides)
  connector/           # Connector types, registry, circuit breakers, pool caching
  engine/              # Dataflow engine & custom function handlers (functions/)
  errors.rs            # OrionError enum -> HTTP response mapping
  kafka/               # Kafka producer & consumer
  metrics/             # Prometheus metrics collection
  queue/               # Async trace processing, DLQ retry
  server/              # Axum routes (routes/), middleware, AppState
  storage/             # Database abstraction, models, repositories (repositories/)
  validation/          # Input validation, SSRF protection
tests/
  integration/         # Consolidated integration test binary (main.rs + *_test.rs modules)
  integration/common/  # Test helpers (test_app, json_request, body_json)
migrations/            # SQLite / Postgres / MySQL migrations (embedded at compile time)
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

## License

By contributing, you agree that your contributions will be licensed under the [Apache-2.0 License](LICENSE).
