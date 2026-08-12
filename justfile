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
    cargo test --test integration -- --ignored data_parity_test data_roundtrip_test postgres_test mysql_test mongodb_test es_test connector_redis_test db_column_types_test dynamic_inputs_test vault_test
    cargo test --test integration -- --ignored kafka_test
    cargo test --test storage_postgres -- --ignored
    cargo test --test storage_mysql -- --ignored
    cargo test --test schema_parity -- --ignored
    cargo test --test cluster -- --ignored --test-threads=1

# Build the documentation book (needs mdbook).
docs:
    mdbook build docs

# Format the tree in place.
fmt:
    cargo fmt

# Offline workflow regression tests (the examples' *.case.json suite).
workflow-tests:
    cargo run -- test examples/workflow-tests
