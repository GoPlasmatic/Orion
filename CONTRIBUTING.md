# Contributing to Orion

Contributions are welcome! Whether it's a bug fix, new feature, documentation improvement, or test — we appreciate the help.

## Getting Started

**Prerequisites:**

- [Rust 1.85+](https://www.rust-lang.org/tools/install) (Orion uses the 2024 edition)
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
cargo build                        # Build (default, no Kafka)
cargo build --features kafka       # Build with Kafka support
cargo test                         # Run all tests
cargo test --features kafka        # Include Kafka-gated tests
cargo test <test_name>             # Run a single test by name
cargo clippy                       # Lint
cargo clippy --features kafka      # Lint including Kafka code
cargo fmt                          # Format code
```

Run `cargo clippy` and `cargo fmt` before committing — both must pass cleanly.

## Project Structure

```
src/
  main.rs              # Server binary entry point
  lib.rs               # Library root (shared between server and CLI)
  cli/main.rs          # CLI binary entry point
  config/              # Configuration loading (TOML + env vars)
  engine/              # Rule engine, custom functions (http_call, enrich, publish_kafka)
  server/              # Axum routes, middleware, AppState
  storage/             # Repository traits and SQLite implementations
  errors.rs            # Error types and HTTP error responses
  connector/           # Connector registry and secret masking
  queue/               # Async job queue
tests/
  common/mod.rs        # Test helpers (test_app, json_request, body_json)
  *.rs                 # Integration tests
migrations/            # SQLite migrations (embedded at compile time)
docs/                  # Documentation
```

## Making Changes

1. **Fork** the repository and create a branch from `main`
2. **Make your changes** — keep commits focused and atomic
3. **Write tests** for new functionality
4. **Run the checks:**
   ```bash
   cargo fmt && cargo clippy && cargo test
   ```
5. **Submit a pull request** with a clear description of what changed and why

## Testing

### Integration tests

Integration tests use an in-memory SQLite database and the full Axum router — no running server needed. The test helpers in `tests/common/mod.rs` provide:

- `test_app()` — creates a ready-to-use `Router` with in-memory DB, repos, and engine
- `json_request(method, uri, body)` — builds an HTTP `Request<Body>` with JSON content-type
- `body_json(response)` — extracts and parses the response body as `serde_json::Value`

**Example pattern:**

```rust
#[tokio::test]
async fn test_my_feature() {
    let app = test_app().await;

    let req = json_request("POST", "/api/v1/admin/rules", json!({
        "name": "Test Rule",
        "channel": "test",
        "condition": true,
        "tasks": []
    }));

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = body_json(response).await;
    assert_eq!(body["name"], "Test Rule");
}
```

### Unit tests

Add unit tests inline in the relevant module using `#[cfg(test)]` blocks. See `src/config/mod.rs` or `src/errors.rs` for examples.

### Running a single test

```bash
cargo test test_my_feature              # By test name
cargo test --test admin_rules_test      # By test file
```

## Code Style

- **Rust 2024 edition** — the codebase uses let-chains (`if let Some(x) = a && let Some(y) = b`)
- **`cargo fmt`** — all code must be formatted
- **`cargo clippy`** — all warnings must be resolved
- **Error handling** — use `OrionError` variants from `src/errors.rs` for new error cases
- **Async** — all repository traits use `async_trait`; keep I/O operations async

## License

By contributing, you agree that your contributions will be licensed under the [Apache-2.0 License](LICENSE).
