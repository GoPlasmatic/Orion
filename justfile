# Common developer tasks — `just --list` shows everything.
# Recipes mirror what CI actually runs (.github/workflows/ci.yml), so a green
# `just check` locally means the PR gate will agree.

# The full pre-PR gate in one command. --workspace matches CI: bare cargo
# commands only cover the server (default-members), but the PR gate runs
# both crates.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    cargo test --doc
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --lib

# End-to-end suite: builds both binaries from this tree, then drives a
# real orion-server over HTTP with the orion-cli binary.
e2e:
    ./tests/e2e/run.sh

# Regenerate the committed OpenAPI spec after changing routes or
# request/response schemas (a test fails while it is stale).
openapi:
    cargo run -- dump-openapi > docs/openapi.json

# Container-gated suites (need Docker; each starts its own testcontainers).
test-containers:
    cargo test --test integration -- --ignored postgres_test mysql_test db_column_types_test data_roundtrip_test data_parity_test integrity_errors_test
    cargo test --test integration -- --ignored mongodb_test es_test connector_redis_test dynamic_inputs_test vault_test
    cargo test --test integration -- --ignored kafka_test
    cargo test --test storage_postgres -- --ignored
    cargo test --test storage_mysql -- --ignored
    cargo test --test schema_parity -- --ignored
    cargo test --test cluster -- --ignored --test-threads=1

# Build the deployable documentation site into docs/book/ (needs mdbook).
# Same script the deploy runs, so what you get here is what Cloudflare serves.
docs:
    bash docs/build.sh

# Serve the built book the way Cloudflare will, on http://localhost:8787 —
# exact-match .html URLs, the "/" proxy rule and the _headers rules all apply,
# which plain `mdbook serve` does not model. Run `just docs` first.
docs-preview:
    cd docs && npx wrangler dev

# Format the tree in place: the Rust, then every definition file and fixture
# the repo ships (fmt_examples_test fails while one is out of style).
fmt:
    cargo fmt
    cargo run -q -- fmt examples tests/e2e/cases tests/e2e/fixtures

# Offline workflow regression tests (the examples' *.case.json suite).
workflow-tests:
    cargo run -- test examples/workflow-tests --plugin-dir examples/packages/fixed-width-statement
