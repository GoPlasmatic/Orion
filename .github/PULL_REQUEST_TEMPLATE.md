## What & why

<!-- What does this PR change, and what problem does it solve?
     Link the issue if there is one: Fixes #123 -->

## How was it tested?

<!-- cargo test? A new integration test? Deployed an example against a live
     instance? For connector/backend changes, mention which backends you ran. -->

## Checklist

- [ ] `cargo fmt && cargo clippy && cargo test` pass cleanly (or `just check`
      for the full CI-equivalent gate)
- [ ] New functionality has tests
- [ ] If HTTP routes or request/response schemas changed: regenerated the
      OpenAPI spec (`cargo run -- dump-openapi > docs/openapi.json` — a test
      fails if it's stale)
- [ ] If behavior or configuration changed: docs updated (`docs/src/`, README,
      or `config.toml.example`)
- [ ] If `examples/` changed: `./examples/deploy.sh <example>` still works
      against a fresh instance
