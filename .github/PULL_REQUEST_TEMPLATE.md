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
- [ ] If HTTP routes or request/response schemas changed, or behaviour or
      configuration changed: a matching change opened in GoPlasmatic/Orion-docs
      (the OpenAPI spec and the reference pages, which its drift checks hold
      to this repository), plus README or `config.toml.example` here
- [ ] If `examples/` changed: `./examples/deploy.sh <example>` still works
      against a fresh instance
