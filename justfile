# Common developer tasks — `just --list` shows everything.
# Recipes mirror what CI actually runs (.github/workflows/ci.yml), so a green
# `just check` locally means the PR gate will agree.

# The full pre-PR gate in one command. --workspace matches CI: bare cargo
# commands only cover the server (default-members), but the PR gate runs
# every crate.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace --all-targets
    cargo test --doc
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --lib

# Print the OpenAPI spec this build serves. The committed copy lives in the
# GoPlasmatic/Orion-docs repository, whose drift checks compare it to the
# pinned Orion; regenerate it there after changing routes or schemas.
openapi:
    cargo run -- dump-openapi

# Container-gated suites (need Docker; each starts its own testcontainers).
test-containers:
    cargo test --test integration -- --ignored postgres_test mysql_test db_column_types_test data_roundtrip_test data_parity_test integrity_errors_test
    cargo test --test integration -- --ignored vault_test
    cargo test --test storage_postgres -- --ignored
    cargo test --test storage_mysql -- --ignored
    cargo test --test schema_parity -- --ignored
    cargo test --test cluster -- --ignored --test-threads=1

# Format the Rust. Definition files (examples/) are formatted, linted and
# tested with orion-cli, which lives in its own repository and checks this
# repo's examples/ in its CI.
fmt:
    cargo fmt --all
