# Orion Test Suite

## Unit and Integration Tests

Run all standard tests (no external services required):

```bash
cargo test
```

Tests run against an in-memory SQLite database constructed by `tests/integration/common::test_app()`, so no additional setup is needed for the default suite.

### Layout

All integration tests are consolidated into a **single test binary** (`integration`) so the suite links once instead of ~43 times — this cuts the per-edit relink cost roughly 5×. Each former `tests/<name>_test.rs` file now lives at `tests/integration/<name>_test.rs` and is declared as a module in `tests/integration/main.rs`. Shared helpers live in `tests/integration/common/`.

To add a test file: create `tests/integration/<name>_test.rs`, reference shared helpers via `crate::common::…`, and add a `mod <name>_test;` line to `tests/integration/main.rs`.

### What's Covered

The consolidated `integration` binary exercises every public-facing surface of Orion:

- **Admin API** — `admin_workflows_test`, `admin_connectors_test`, `bulk_import_test`,
  `workflow_lifecycle_test`, `typed_enum_dtos_test`, `audit_log_test`, `backup_restore_test`
- **Data path & routing** — `rest_routing_test`, `channel_call_test`, `channel_config_test`,
  `channel_config_validation_test`, `pipeline_test`
- **Resilience / scaling** — `rate_limit_test`, `concurrency_test`, `circuit_breaker_test`,
  `pool_exhaustion_test`, `channel_dedup_test`, `channel_response_cache_test`
- **Async + tracing** — `async_traces_test`, `async_trace_edge_test`, `task_trace_test`,
  `trace_storage_test`, `trace_dlq_repo_test`, `profile_test`
- **Connectors** — `connector_db_test`, `connector_cache_test`, `connector_redis_test`,
  `mongodb_test`, `mysql_test`, `postgres_test`, `kafka_test`
- **CLI subcommands** — `cli_subcommands_test` covers `lint`, `dry-run`,
  `test-connectivity`, and `validate-config`
- **Errors & security** — `error_envelope_test`, `error_paths_test`,
  `protocol_required_fields_test`, `secret_references_test` (env:// resolver),
  `security_test`, `shutdown_test`
- **Scenarios** — `scenario_api_gateway_test`, `scenario_ecommerce_test`,
  `scenario_webhook_test` walk multi-step user journeys end-to-end
- **OpenAPI** — `openapi_test` snapshots the generated spec to catch accidental breaks
- **Function schemas** — `function_schema_test` verifies every workflow input schema

Most tests use `tower::ServiceExt::oneshot()` against the in-memory router; no HTTP listener is started.

## External Service Tests

Some tests exercise real database and cache connectors (PostgreSQL, MySQL, MongoDB, Redis). These are marked `#[ignore]` so they are skipped during normal `cargo test` runs.

### Start External Services

```bash
docker compose -f docker-compose.test.yml up -d
```

Wait for all services to become healthy:

```bash
docker compose -f docker-compose.test.yml ps
```

### Run All External Tests

```bash
cargo test -- --ignored
```

### Run a Specific External Test

```bash
cargo test test_postgres_db_write_and_read -- --ignored
cargo test test_mysql -- --ignored
cargo test test_redis -- --ignored
cargo test test_mongo -- --ignored
```

### Run a Specific Test Module

Since all tests share one binary, filter by the module name (the former file
name) rather than a `--test` target:

```bash
cargo test postgres_test:: -- --ignored
cargo test mysql_test:: -- --ignored
cargo test connector_redis_test:: -- --ignored
cargo test mongodb_test:: -- --ignored
```

### Stop External Services

```bash
docker compose -f docker-compose.test.yml down
```

## Service Ports

| Service    | Port  | Credentials                  |
|------------|-------|------------------------------|
| PostgreSQL | 5432  | postgres / test              |
| MySQL      | 3306  | root / test                  |
| Redis      | 6379  | (no auth)                    |
| MongoDB    | 27017 | (no auth)                    |

## Benchmarks

Performance scenarios live in `tests/benchmark/` and use the [`hey`](https://github.com/rakyll/hey) HTTP load generator. Run all scenarios:

```bash
BENCH_RELEASE=1 ./tests/benchmark/bench.sh
```

Captured runs live alongside the script under `tests/benchmark/results/`:

| Directory       | What it captures                                                               |
|-----------------|---------------------------------------------------------------------------------|
| `v2.1.5/`       | v0.1.x baseline (dataflow-rs 2.1.5 + datalogic-rs 4)                            |
| `v0.2.0/`       | v0.2.0 release (dataflow-rs 3.0 + datalogic-rs 5) — current default            |
| `v3.0.0/`       | First dataflow-rs 3.0 upgrade snapshot, kept for historical comparison         |
| `trace-modes/`  | sync vs async vs batch vs off trace-persistence cost on a steady workload      |

Each result directory contains a `SUMMARY.md` (Markdown table) plus the raw per-scenario `hey` output for reproducibility.
