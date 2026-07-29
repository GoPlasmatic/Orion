# Orion Test Suite

## Test Binaries

The suite is split into five test binaries. The storage backend is pinned per
process (a global `DB_BACKEND` OnceLock), which is why the Postgres/MySQL
storage tests and the cluster tests cannot live in the integration binary.

| Binary | Location | Backend | Docker |
|--------|----------|---------|--------|
| `integration` | `tests/integration/` | in-memory SQLite | only for the `#[ignore]` tests |
| `cluster` | `tests/cluster/` | Postgres + Redis testcontainers | required (all `#[ignore]`) |
| `storage_postgres` | `tests/storage_postgres.rs` | Postgres testcontainer | required (all `#[ignore]`) |
| `storage_mysql` | `tests/storage_mysql.rs` | MySQL testcontainer | required (all `#[ignore]`) |
| `schema_parity` | `tests/schema_parity.rs` | all three | only for the cross-backend test |

`schema_parity` (D10) migrates each backend from scratch and asserts the three
schemas agree — columns with their normalised types and nullability, every
`idx_*` index **with its ordered column list**, and the view columns. Comparing
index columns rather than just names is the point: an index that exists on all
three but covers different columns serves a different query. It goes through
`orion::storage::migrator_for` rather than `run_migrations`, which is how one
process can migrate a backend it is not pinned to. Its SQLite half and its
normaliser tests run in the default suite; the cross-backend comparison is
`#[ignore]`d and needs Docker. Failure messages name the table, the column and
both types so a CI-only failure is diagnosable from the log alone.

Two allow-lists carry the deliberate exceptions, each entry with its reason:
`BACKEND_SPECIFIC_INDEXES` (partial and partial-unique indexes MySQL cannot
express) and `DIVERGENT_INDEX_COLUMNS` (the DLQ claim index, whose predicate
lives in a `WHERE` clause on SQLite/Postgres and in the key on MySQL). Anything
not listed must match on all three.

## Default Suite

```bash
cargo test
```

No external services are needed: the integration tests run against an
in-memory SQLite database built by `tests/integration/common::test_app()`,
and drive the router directly with `tower::ServiceExt::oneshot()` (no HTTP
listener). Container-gated tests are `#[ignore]` and skipped.

### Layout

All integration tests link into a **single binary**: each file under
`tests/integration/` is a module declared in `tests/integration/main.rs`, so
the suite links once instead of ~60 times. Shared helpers live in
`tests/integration/common/` (`mod.rs` for the app/channel/workflow builders,
`dsl.rs` for the data-plane task DSL, `backends.rs` for the testcontainers
harness); the same `common` module is reused by the `cluster` binary via
`#[path]`.

To add a test file: create `tests/integration/<name>_test.rs`, use helpers
via `crate::common::…`, and add `mod <name>_test;` to
`tests/integration/main.rs`.

### What's Covered

Thematically: admin API CRUD/versioning/lifecycle for workflows, channels and
connectors; data-plane routing and pipelines; the portable
`data_query`/`data_write` dialects; resilience (rate limiting, dedup,
response caching, circuit breakers, backpressure, drain/shutdown); async
traces and the DLQ; security (auth, secret masking, SSRF, trace redaction);
TLS (certificates are generated in-process with `rcgen`, so these always
run); CLI subcommands; OpenAPI; and end-to-end scenario walks. For the full
module-by-module list:

```bash
cargo test --test integration -- --list
```

Filter by module name to run one file's tests:

```bash
cargo test --test integration rest_routing_test
```

## Container-Gated Tests

Tests that need a real external service start their own ephemeral
[testcontainers](https://rust.testcontainers.org/) (Postgres, MySQL, MongoDB,
Elasticsearch, Redis, Kafka) — nothing to start manually, only Docker. They
are `#[ignore]`d so the default run stays Docker-free:

```bash
# Integration binary: portable-dialect round-trips, raw-SQL backends,
# Mongo/ES connectors, Redis cache/dedup, column-type matrix, dynamic
# inputs, Kafka channels
cargo test --test integration -- --ignored data_roundtrip_test postgres_test mysql_test mongodb_test es_test connector_redis_test db_column_types_test dynamic_inputs_test
cargo test --test integration -- --ignored kafka_test

# Orion's own storage on Postgres / MySQL
cargo test --test storage_postgres -- --ignored
cargo test --test storage_mysql -- --ignored

# Cross-backend schema parity (all three migration sets)
cargo test --test schema_parity -- --ignored

# Multi-node cluster mode (two AppStates over shared Postgres + Redis)
cargo test --test cluster -- --ignored --test-threads=1
```

CI runs exactly these invocations in the `integration-containers` job
(`.github/workflows/ci.yml`); Kafka gets its own step because sharing one
invocation with the other containers starves the brokers.

## Benchmarks

Performance scenarios live in `tests/benchmark/` and use the
[`hey`](https://github.com/rakyll/hey) HTTP load generator:

```bash
BENCH_RELEASE=1 ./tests/benchmark/bench.sh
```

Each run writes a `SUMMARY.md` plus raw per-scenario `hey` output to
`tests/benchmark/results/`. The directory is gitignored; results are local
snapshots, not checked in. The headline numbers live in the README's
[Performance](../README.md#performance) section.
