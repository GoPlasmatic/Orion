# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Orion is a declarative services runtime written in Rust. It exposes business logic management through channels (service endpoints) and workflows (task pipelines powered by dataflow-rs) via a REST API. Ships as a single binary with an embedded SQLite database.

This repo is a cargo workspace (monorepo) with four crates: `crates/orion-server` (the runtime — everything below describes it), `crates/orion-cli` (the CLI, which has its own `CLAUDE.md`), and two shared library crates. `crates/orion-api` is the wire contract: response DTOs, domain enums, the error envelope + `codes` registry, and the bulk-import report — the server serializes these types, clients deserialize them; the server re-exports them under their pre-1.0 paths (e.g. `crate::errors::FieldError`, `storage::models::EntityStatus`). Its deserialization is tolerant by design (every field defaults, unknown fields ignored) so version skew between server and CLI keeps working; its `utoipa` feature (enabled only by the server) adds the `ToSchema` derives for the OpenAPI document. `crates/orion-client` is the one HTTP transport over that contract — `OrionClient` (auth, envelope unwrap, typed `ClientError`) plus the `paths` module every endpoint path is built from; both `orion-cli` and the server's `package_cli` drive it, and it is the only crate that owns reqwest for API calls. Neither library crate has a release cycle of its own — no tag names them; `crates-publish.yml` publishes them automatically as riders (skip-if-present, dependency order) right before `orion-server`, because crates.io refuses a crate whose dependency it doesn't host. **All four crates share one version**, `workspace.package.version` in the root `Cargo.toml`, inherited with `version.workspace = true` and repeated only in the two `[workspace.dependencies]` requirements (a path dependency needs a `version` to be publishable). That is what makes the riders safe: a release always moves the number, so skip-if-present can never fire on a rider whose contents changed. Bump it with `cargo release`, never by hand in a member manifest. Note that `orion-server` is the *only* binary crate on crates.io: the `orion-cli` name there belongs to an unrelated 2021 crate, so the CLI ships via the dist installers, the Homebrew tap, GHCR and `cargo install --git` instead (see `RELEASING.md`). `default-members` points bare cargo commands at the server; use `--workspace` or `-p orion-cli` to reach the CLI. Only the UI lives in a separate repo (Orion-ui).

- **Rust Edition:** 2024. **MSRV: 1.98** (`rust-version` in Cargo.toml) — the toolchain the images build on (`rust:1.98-slim` in both Dockerfiles) and the one CI's MSRV job runs the suite against. The *language* floor is lower: let-chains (`if let Some(x) = a && let Some(y) = b`) landed in 1.88, alongside dependency requirements (`mongodb`, `serde_with`, `time`, `tonic`).
- **Core dependencies:** `dataflow-rs` 3.11 (workflow engine), `axum` 0.8 (HTTP), `sqlx` 0.8 (database), `sea-query` 1.0 (portable SQL builder) with `sea-query-sqlx` 0.8 as its sqlx binder (the crate that replaced `sea-query-binder`). dataflow-rs 3.6 made a workflow's `tasks` array hold **steps**: an element carrying a `tasks` key is a task group, and any step may set `terminal: true` to end the workflow. The engine flattens that tree at parse, but Orion's own analysis reads the *authored* JSON — so every walk over `tasks` must go through `engine::walk_steps`/`leaf_tasks` (`engine/steps.rs`) or it silently skips everything inside a group. sea-query 1.0 moved the comparison operators (`.eq`, `.lte`, `.is_in`, `.like`, …) off `Expr` and onto the `ExprTrait` trait, so a module building predicates needs `use sea_query::ExprTrait;` — and because that trait is blanket-implemented for everything convertible into an `Expr`, it also makes a bare `.max(…)` on an integer ambiguous with `Ord::max` (spell it `Ord::max(a, b)`).
- **dataflow-rs 3.7 is "the host surface" release, and Orion no longer mirrors what it publishes.** Before adding a check about a workflow definition, look for the engine's own answer: `Workflow::validate_authored` (authored-JSON shape, every issue, authored coordinates), `Engine`/`EngineBuilder::check_workflow` (against the real handler registry and the real `TemplateCompiler`), `can_dispatch` / `dispatchable_functions` (what an engine will actually run), `Engine::operator_names` (the live JSONLogic vocabulary), `walk_authored_steps` / `is_group` / `MAX_GROUP_DEPTH` (the step grammar), `Rollout::partition` / `validate_set` (bucket arithmetic), `RetryPolicy` / `retry_with_policy` / `retry_with_attempts` (the retry loop), `TaskContext::workflow_id` / `task_id` / `loop_counter` (execution identity), and the `ExecutionObserver` lifecycle callbacks. `engine/steps.rs`, `engine/operators.rs::operator_names`, `engine/loader.rs::screen_workflow` and `engine/functions/mod.rs`'s retry re-export are all thin adapters over these — do not reintroduce a local copy. **Not every issue `check_workflow` reports is fatal — ask `IssueCode::severity()` (3.11), never a local list.** `Severity::Advisory` (`ESCAPED_TEMPLATE_KEY`, `UNGUARDED_VALIDATION`, `GROUP_CONTINUE_ON_ERROR`) is the only class safe to pass a pre-build screen; `Rejected` is refused by `build`, and **`Defect` — `MISSING_HANDLER` — builds cleanly and then fails every message, so it must still quarantine** (F54, the `enrich` case). `severity()` is a match with no wildcard arm, so a code a later minor adds is classified upstream rather than arriving as a silent refusal here; Orion kept a hand-maintained list until 3.11, and it took a code edit per release to avoid quarantining working channels (3.9 and 3.10 both). `screen_workflow` and `validation::engine_advisories` both select on the severity, so the serving screen and the author-facing report cannot disagree; an advisory code neither knows by name is reported as `engine.advisory` rather than dropped. The two lists Orion must still keep (`CUSTOM_HANDLER_FUNCTIONS`, the `/admin/functions` catalogue) exist because both are consulted before an engine exists; they are pinned against a live engine by `function_schema_test.rs`.
- **`datalogic-rs` 5 (JSONLogic) and `datavalue` are reached through `dataflow-rs`**, not pinned directly. dataflow-rs's public API is written in terms of both — `TaskContext::datalogic()` returns `&Arc<datalogic_rs::Engine>`, the whole context/path surface is `datavalue::OwnedDataValue` — so a second pin would let their major versions skew from the ones dataflow-rs links. Add `use dataflow_rs::datalogic_rs;` (or `::datavalue`) to a module that needs them; a bare `datalogic_rs::` path will not resolve, and note that a file-level `use` does **not** reach an inner `#[cfg(test)] mod tests`. Orion cannot enable a datalogic feature directly, but dataflow-rs 3.2 added `all-operators`, which passes through to the `datetime`, `ext-string`, `ext-array`, `ext-math`, `ext-control`, `error-handling` and (since 3.4/5.2) `ext-object` gates — and the server enables it, so those operators *are* available. Orion additionally registers its own custom operators (`src/engine/operators.rs`: base64/base64url/hex encode+decode, `random`, `url_encode`, `url_decode`, `join`) on every engine it builds, via dataflow-rs 3.4's `with_datalogic_operator` builder passthrough. `docs/src/reference/expressions.md#available-operators` is the resulting vocabulary, asserted against the engine by `jsonlogic_operators_test.rs`.
- **Binaries:** `orion-server` (`crates/orion-server/src/main.rs`) and `orion-cli` (`crates/orion-cli/src/main.rs`)

## Build & Development Commands

```bash
cargo build                        # Build (all features included)
cargo build --release              # Release build

cargo run -- --config ./config.toml  # Run with config file

cargo test                         # Run the server suite (default-members)
cargo test --workspace             # Server + CLI suites — what CI runs
cargo test <test_name>             # Run a single test by name

cargo clippy --workspace --all-targets  # Lint (matches CI)
cargo fmt --all                    # Format code

just e2e                           # End-to-end suite (tests/e2e): CLI against a real orion-server
```

**`just check` is the pre-PR gate** — it runs exactly what the CI PR jobs run, in one command:
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`, `cargo test --doc`, and `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --lib`.
Note the `--workspace` flags: bare `cargo test`/`cargo clippy` only cover the server
(`default-members`), so they silently skip `orion-cli` — CI does not. Other recipes:
`just openapi` (regenerate the committed spec), `just test-containers` (the Docker-gated
suites), `just workflow-tests`, `just docs` / `just docs-preview` (mdbook site), `just fmt`.

**After changing routes or request/response schemas, regenerate the committed spec** —
`cargo run -- dump-openapi > docs/openapi.json` (or `just openapi`). `openapi_test.rs`
fails while it is stale.

Docker: `docker build -t orion .` for the server; `docker build -f crates/orion-cli/Dockerfile -t orion-cli .` for the CLI (both multi-stage from the workspace-root context).

## Runtime Configuration

All capabilities are compiled into a single binary — no feature flags. Behaviour is controlled at runtime:

| Capability | Configuration | Default |
|-----------|--------------|---------|
| Database backend | `storage.url` scheme (`sqlite:`, `postgres://`, `mysql://`) | SQLite |
| Kafka | `kafka.enabled` | Disabled |
| OpenTelemetry | `tracing.enabled` | Disabled |
| Trace persistence mode | `trace_storage.mode` (`sync` / `async` / `batch` / `off`) — global default with per-channel override via `config.tracing` | Sync |
| TLS/HTTPS | `server.tls.enabled` | Disabled |
| Swagger UI / OpenAPI spec | `server.docs.enabled` (unset = enabled outside production) | Enabled outside production |
| SQL connectors | `db_read`/`db_write` functions | Always available |
| Redis cache | `cache_read`/`cache_write` with Redis backend | Always available |
| MongoDB connector | `mongo_read`/`mongo_write`/`mongo_aggregate` functions | Always available |
| SMTP connector | `send_email` function | Always available |
| Object storage connector | `storage_presign`/`storage_head` functions (presign + metadata only; no data path) | Always available |

## Architecture

### Module Structure

Paths below are relative to `crates/orion-server/`.

```
src/
├── main.rs              # clap CLI entrypoint; declares the binary-only cli/package_cli modules
├── bootstrap.rs         # Startup sequence: config → pools → repos → engine → HTTP server
├── cli.rs               # Diagnostic subcommands: validate-config, migrate, lint, dry-run, test, test-connectivity, dump-openapi
├── preflight.rs         # `orion-server preflight` — scans the stored estate for upgrade breaks
├── package_cli.rs       # `orion-server package` — export/lint/plan/apply/diff promotion CLI
├── lib.rs               # Public module declarations
├── channel/             # Channel registry, config, routing, rate limiting, request guards
├── cluster/             # Multi-node coordination: epoch watcher, job leases
├── config/              # Configuration loading & validation
├── connector/           # Connector types, registry, circuit breakers, pool caching, secret resolution
├── crypto.rs            # Shared primitives: the base64/hex encoding table, HMAC compute/verify, the rustls provider. A leaf — `engine::operators`, `channel::auth`, `connector::sigv4` and `jwt` all consume it
├── definitions/         # A definition set (channels+workflows+connectors) and the cross-reference pass over it: `lint <dir>` and `package lint` share it; shared.rs holds the `$from` / fragment resolver, whose expansion descends into task groups via `engine::is_group` — a flat loop there namespaces only a fragment's top-level ids and leaks the rest into the host workflow
│   ├── json.rs          # Order-preserving, span-carrying JSON front end (own parser; `serde_json::Value` is BTreeMap-backed and loses author order). `fmt` parses with it; `Document::locate(path)` maps a finding's path to line:col
│   ├── analysis/        # Facts every clippy rule reads: flattened steps with reads/writes/certainty (`Reads::uncertain` = a computed or element-scoped read — a rule needing complete reads must then stay silent), every expression compiled by datalogic (`is_constant`, evaluate-against-context), the channel→workflow binding. `operators.rs` classifies every operator as scoping or not; a new operator fails a test until classified
│   ├── clippy/          # `orion-server clippy`: `Rule` trait + `rules::ALL` registry. No configuration, no suppression — a rule ships only with a proof (`explain()` must say "Proof" and "Silent when") and `tests/fixtures/clippy/<id>/{fires,quiet}/`. Duplication rules read the *source* form (`from_directory_raw`); semantic rules the compiled form
│   ├── fmt/             # `orion-server fmt`: style.rs (the numbers + canonical key tables — no user configuration by design), roles.rs (shape classifier; operator nodes by structure, unary/leaf/compound), printer.rs (measure-then-emit, linear). `format_str` re-parses its output and refuses to return a document that differs from the input as a `Value`
│   └── compile.rs       # The authoring layer: an ordered pipeline of `Pass`es (source form → canonical form). A new simplification is a new pass; its `residue()` is what `compile` reports, what the pipeline test asserts empty, and what the admin API refuses by name
├── engine/              # Dataflow engine build, observer, custom function handlers, and `execute_admitted` — the one post-admission step every transport runs
│   ├── steps.rs         # Flattens a `tasks` array of steps (task or task group) — every walk over tasks goes through it
│   └── functions/       # http_call, channel_call, db_read/write, data_query/write, cache_read/write, mongo_read/write/aggregate, publish_kafka, send_email, storage_presign/head, crypto, jwt_sign/verify
├── errors.rs            # OrionError enum → HTTP response mapping
├── jwt/                 # Shared JWT core (verify, sign, JWKS cache) behind three surfaces: `jwt` channel auth, jwt_verify, jwt_sign
├── kafka/               # Kafka producer & consumer
├── metrics.rs           # Prometheus metrics collection
├── query/               # Portable data dialect: IR, lowering, schema; backends sql/mongo/es
├── queue/               # Async trace/audit processing, DLQ retry
├── request_context.rs   # Per-request identity in a task-local, read by `errors` and channel error bodies; the middleware that fills it is in server/
├── runtime/             # Process-level concerns above engine/channel/kafka/queue: `generation.rs` (`RuntimeGeneration` = engine + channel estate, published as one value through `RuntimeHandle`), `tasks.rs` (the background-task supervisor behind `/readyz`), `reload.rs` (rebuilds that generation + the Kafka consumer), `handler_deps`
├── server/              # HTTP server, middleware, state
│   └── routes/          # admin/ (workflows, channels, connectors, packages, functions, engine, audit, backups, trace_dlq), data/
├── storage/             # Database abstraction, content hashing, config encryption
│   ├── models/          # Row types, DTOs, enums
│   └── repositories/    # workflows, channels, connectors, packages, traces, trace_dlq, audit_logs, cluster
├── text.rs              # String similarity (edit distance) shared by the "did you mean" suggestions
├── trace_context.rs     # W3C trace propagation over string maps (Kafka, the trace queue, http_call); the axum middleware is in server/
└── validation/          # Input validation, SSRF protection
```

### Startup Sequence (main.rs → bootstrap.rs)

CLI args → config (TOML + `ORION_SECTION__KEY` env overrides) → tracing → metrics → detect DB backend from URL → DB pool + migrations → repositories (workflows, channels, connectors, packages, traces, trace_dlq, audit_logs) → ConnectorRegistry → HTTP client → runtime handle (pre-created around an empty generation, for channel_call) → cache pool → external pool caches (SQL, MongoDB) → custom functions → Kafka producer (if enabled) → load active channels + workflows → filter by include/exclude patterns → build engine + channel estate → publish generation 1 → trace queue workers → trace cleanup → DLQ retry → Kafka consumer (if enabled, config + DB topics merged — **after** the queues, because it writes a trace per message) → rate limiter → Axum HTTP server → graceful shutdown on SIGTERM/SIGINT.

### Key Architectural Patterns

- **Channels + Workflows:** Channels are service endpoints (sync/async, REST/HTTP/Kafka) that link to workflows. Workflows are versioned task pipelines with JSONLogic conditions. A channel references a workflow via `workflow_id`. The channels, workflows, and connectors of one service form a **package** — the versioned unit `orion-server package` exports/imports between instances (modular monolith; receipts under `/api/v1/admin/packages`, docs in `docs/src/concepts/packages.md`, runnable examples in `examples/packages/`).
- **Repository pattern:** Trait-based (`WorkflowRepository`, `ChannelRepository`, `ConnectorRepository`, `PackageRepository`, `TraceRepository`, `TraceDlqRepository`, `AuditLogRepository`, `ClusterRepository`) with SQL implementations. Traits use `async_trait`. All repos are stored as `Arc<dyn Trait>` in `AppState`.
- **One runtime generation:** the engine and the channel estate are **one published value**, `RuntimeGeneration { id, engine, channels }`, held in an `ArcSwap` behind `Arc<RuntimeHandle>` (`runtime/generation.rs`) and reachable as `state.runtime`. `publish(engine, channels)` is the only writer and takes both halves, so there is no way to store one. **Every ingress loads it exactly once per unit of work** — one HTTP request (`data/mod.rs`, above the route match), one Kafka record, one queued trace, one `channel_call` — and passes the generation down; `execute_admitted` takes `&Arc<Engine>` from it rather than looking one up. Two loads in a request re-open the bug this closes: while the engine and the estate were two `ArcSwap`s stored one after the other, a request could be admitted by the new estate and executed by the old engine, matching zero workflows and answering `200` with the caller's own input. `concurrency_test::a_reload_publishes_exactly_one_generation` counts the stores.
- **Engine hot-reload:** `runtime/reload.rs` reads the database, builds both halves off to the side, publishes them with one atomic store, then restarts the Kafka consumer if the topic set changed; the whole sequence is serialised by `AppStateInner::reload_lock` (the only lock in it — `ChannelLoader` has none). A failure while building publishes nothing, so the previous generation keeps serving whole. Reload triggers on status changes (activate/archive), delete, and manually via `POST /api/v1/admin/engine/reload`. Draft creates/updates do not trigger reload. In cluster mode the mutation then bumps the config epoch with an `EpochScope` so peers resync only what moved.
- **Channel estate:** `ChannelSnapshot` (`channel/registry.rs`) is the channel half of a generation — `ChannelRuntimeConfig` per active channel (parsed config, rate limiters, compiled validation logic, backpressure semaphores, dedup stores, response caches), a `RouteTable` for REST route matching, and the quarantine map, all self-consistent (N17). It is an immutable value with no lock and no `ArcSwap` of its own. `ChannelLoader` (`state.channel_loader`) builds the next one from the previous one, carrying unchanged channels over by `Arc` so a reload costs what changed.
- **Custom async functions:** 18 handlers implement `dataflow_rs::engine::functions::AsyncFunctionHandler`, registered in `engine/handlers.rs::build_custom_functions()` (re-exported from `engine/mod.rs`) and enumerated in `engine/loader.rs::CUSTOM_HANDLER_FUNCTIONS` — which is the list to check, not this sentence: `http_call`, `channel_call`, `cache_read`, `cache_write`, `db_read`, `db_write`, `data_query`, `data_write`, `mongo_read`, `mongo_write`, `mongo_aggregate`, `publish_kafka`, `send_email`, `storage_presign`, `storage_head`, `crypto`, `jwt_sign`, `jwt_verify`. `data_query`/`data_write` are the portable read/write dialects (backend-neutral filter + envelope → SQL/MongoDB/ES) in `src/query/`; `db_read`/`db_write` are the raw-SQL escape hatch; `crypto`, `jwt_sign` and `jwt_verify` are self-contained (no connector, no egress), so dry-run executes them for real rather than stubbing them.
- **Connector registry:** In-memory `RwLock<HashMap<String, Arc<ConnectorConfig>>>` with secret masking on API reads, circuit breakers per connector with LRU eviction. Db/es connector configs carry per-operation gates (`operations: { read, insert, update, delete, upsert, raw_write }`, all default `true`) enforced by the data handlers — e.g. set `"delete": false` to make a connector delete-proof.
- **Trace queue:** `tokio::sync::mpsc` channel with semaphore-limited concurrency for async trace processing (`queue/mod.rs`). Failed traces go to DLQ table with automatic retry.
- **Error handling:** `OrionError` enum in `errors.rs` implements `axum::response::IntoResponse`, mapping variants to HTTP status codes. Returns JSON `{"error": {"code": "...", "message": "..."}}`.
- **AppState** (`server/state.rs`): Central shared state struct. Coherent clusters are grouped into sub-structs (R26): `repos` (`storage::repositories::Repositories` — workflows, channels, connectors, packages, traces, trace_dlq, audit_logs), `kafka` (producer, consumer_handle, ingest_status), and `caches` (cache_pool, sql_pool_cache, mongo_pool_cache). Runtime-singular fields stay flat: the runtime handle, the channel loader, connector registry, trace/audit queues, config, metrics handle, HTTP client, DataLogic instance, rate limit state, readiness flag, cluster runtime, admin-auth failure tracker, trusted proxies. Passed to all route handlers via Axum's `State` extractor.

### Middleware Stack (server/mod.rs)

1. CatchPanicLayer (outermost — panic recovery)
2. OTel trace context extraction (if `tracing.enabled`)
3. HTTP metrics middleware
4. Admin auth middleware (if enabled)
5. Rate limiting middleware (if enabled)
6. Body limit (max payload size)
7. Compression (gzip/brotli)
8. Security headers (CSP, X-Frame-Options, X-Content-Type-Options, Referrer-Policy, Permissions-Policy, HSTS)
9. Request ID layer (generate/propagate x-request-id)
10. Trace layer (request/response tracing)
11. CORS layer

### Request Processing Flow

```
HTTP Request → Axum Router → Data Route Handler
  → Runtime generation loaded ONCE (engine + channel estate, atomically published)
  → Route Resolution (REST pattern match → channel name lookup → fallback)
  → Channel estate (ingress guards, in order per channel/guards.rs::apply_guards: rate limit, auth, origin, validation, dedup, response cache, backpressure)
  → Engine (generation.engine — the same generation the guards above came from)
    → Channel Router (match by channel name)
    → Workflow Matcher (JSONLogic condition evaluation + rollout bucket)
    → Task Pipeline (ordered function execution)
  → Response (cache store, JSON response)
```

### API Structure

- **Admin** (`/api/v1/admin/`):
  - **Channels:** CRUD, status management (draft/active/archived; `?dry_run=true` pre-flight, `?reload=defer`), versioning, import/export (`?on_conflict=fail|skip|new_version`), validate, tags (`?tag=` filter). Names are unique per `channel_id`; activation requires an active workflow.
  - **Workflows:** CRUD, status management, versioning, rollout, dry-run test, import/export, validate, `GET /{id}/dependencies` (connector refs + `channel_call` targets)
  - **Connectors:** CRUD (`enabled` flag, tags), reload, test, import/export, validate, circuit breakers (list/reset)
  - **Packages:** promotion receipts — list/get/put; applied versions are content-immutable (same version + different `content_hash` → 409)
  - **Functions:** `GET /functions` — per-function input schemas for tooling
  - **Engine:** status, reload (also batches promotions committed with `?reload=defer`)
  - **Audit logs:** list with filtering; `X-Orion-Change-Context` request header lands in `details`
  - **Backups:** create and list SQLite backups (`VACUUM INTO`, refused in cluster mode). There is **no restore endpoint** — restore is an offline stop/replace-file/start procedure; PostgreSQL and MySQL have no in-product backup and rely on operator snapshot/PITR tooling. See `docs/src/operate/backup-restore.md`.
- **Data** (`/api/v1/data/`): Dynamic handler `/{*path}` — resolves to channel via REST route match or name lookup. Supports sync and async (trailing `/async`). Trace list/get endpoints.
- **Operational:** `GET /health`, `GET /healthz` (liveness), `GET /readyz` (readiness), `GET /metrics`
- **API docs:** `GET /docs` (Swagger UI), `GET /api/v1/openapi.json` — gated by `server.docs.enabled` (unset = served only outside production; 404 when disabled)

### Database

SQLite (default), PostgreSQL, or MySQL — selected at runtime from `storage.url` scheme. All three migration sets are embedded via `sqlx::migrate!()` and the correct set is chosen at startup based on the detected backend (`DbBackend` enum in `storage/mod.rs`). `DbPool` is an enum wrapping the concrete pool types (`SqlitePool`/`PgPool`/`MySqlPool`) with dispatch helpers for query execution. Tables: `workflows` (composite PK `(workflow_id, version)`), `channels` (composite PK `(channel_id, version)`), `connectors`, `packages` (promotion receipts), `traces`, `trace_dlq`, `audit_logs`; workflows, channels and connectors carry a `tags_json` column. Views: `current_workflows`, `current_channels` (latest version per ID). Triggers enforce single-draft-per-ID and active-immutability constraints. Migrations per backend in `migrations/{sqlite,postgres,mysql}/`.

**Migration rules** (details in `CONTRIBUTING.md#database-migrations`):

- Shipped migration files are **checksum-frozen** — sqlx records a checksum per applied migration, so never edit an existing `NNN_*.sql`. Add a new numbered file to **each** of the three directories.
- **The three sequences are independent.** Each backend numbers its own directory, so the same number means different things per backend (`004` is `cluster_coordination` on SQLite, `bigint_columns` on PostgreSQL, `active_immutability` on MySQL). **Name migrations rather than number them** in commits, docs and runbooks. `tests/schema_parity.rs` catches a change applied to two backends out of three.
- Write migrations **expand/contract** style: a rolling deploy briefly runs old and new binaries against one database, so a release may only *add* schema alongside code tolerating both shapes; drop the old shape a release later.

## Testing

- **Integration tests** in `crates/orion-server/tests/integration/`: one binary — each file is a module declared in `tests/integration/main.rs`. Use `common::test_app()` which creates an in-memory SQLite DB, full `AppState`, and Axum router. Tests use `tower::ServiceExt::oneshot()` (no HTTP server needed).
- **Test helpers** in `tests/integration/common/mod.rs`:
  - `test_app()` — returns a ready-to-use `Router` with in-memory DB
  - `json_request(method, uri, body)` — builds an HTTP `Request<Body>` with JSON content-type
  - `body_json(response)` — extracts and parses the response body as `serde_json::Value`
- **Pattern for new integration tests:** Clone the app, call `.oneshot(json_request(...))`, assert status, parse body with `body_json()`. See `tests/integration/admin_workflows_test.rs` for examples. Declare the new module in `tests/integration/main.rs`.
- **Other test binaries:** `tests/cluster/` (multi-node contracts), `tests/storage_postgres.rs`, `tests/storage_mysql.rs`, `tests/schema_parity.rs` (container-gated), `tests/metrics_exposition.rs` (isolated for its process-global metrics recorder), plus container-gated modules inside the integration binary listed in `.github/workflows/ci.yml` (kept in sync by `ci_filter_drift_test`).
- **Benchmarks:** `crates/orion-server/tests/benchmark/bench.sh` — 6 scenarios using `hey` HTTP load generator.
- **End-to-end:** `tests/e2e/run.sh` at the repo root — shell suites driving a real server with the CLI binary (`just e2e` locally; the `cli-e2e` CI job). Its data-driven cases split by role: scenario cases in `examples/use-cases/` (deploying the example packages, workflows referenced by file), runtime-behaviour cases in `tests/e2e/cases/`. The full suite-by-suite map of the test estate is `TESTING.md`.

## Docs and code are kept in sync by tests

A family of drift guards in the integration binary makes the code authoritative and fails
the build when a document disagrees with it. They run in the default `cargo test` — so
these edits are not optional follow-ups, they are part of the change:

| Change | Also update |
|---|---|
| A config field or its `Default` | `docs/src/reference/configuration.md` + `config.toml.example` (`config_docs_drift_test`) |
| A metric name or label key | `docs/src/reference/metrics.md` (`metrics_docs_drift_test`) |
| A task function or its input schema | `docs/src/reference/functions.md` (`functions_docs_drift_test`) |
| A route, method or query parameter | the book's `curl` examples + `docs/openapi.json` (`docs_routes_drift_test`, `openapi_test`) |
| An audit `action` / `resource_type` | `docs/src/operate/audit-logs.md` (`audit_actions_drift_test`) |
| A `FieldError` code | `orion_api::error::field_codes::ALL` (the closed registry) **and** `docs/src/reference/errors.md` — `field_codes_drift_test` also fails on a registered code nothing emits |
| A JSONLogic operator | `docs/src/reference/expressions.md` only — the vocabulary itself comes from `Engine::operator_names()` via `engine::operators::operator_names()`, so registering the operator is all the code needs. `jsonlogic_operators_test` asserts the docs and its evaluated table against the live engine in both directions |
| A workflow-shape rule (task, task group, `terminal`) | `validation/workflows.rs::validate_workflow_tasks_schema` — its catch-all is now `Workflow::validate_authored`, so a rule the engine already enforces needs no mirror here, only a better message if you want one. Any new walk over `tasks` must use `engine::walk_steps`, never `tasks.as_array()`, or it skips task groups silently |
| A cross-reference check over a definition set | `definitions/check.rs` — one pass, shared by `lint <dir>` and `package lint`; give it a stable `check` id so a pipeline can grandfather it |
| An authoring convenience (new source-form syntax) | `definitions/compile.rs` — implement `Pass`, register it in `passes()`, give it a stable id. `residue()` must mirror the rewrite **exactly**: detect more than you expand and the admin API refuses documents `compile` accepts; detect less and source form reaches the runtime. Document it under [Shared definitions](docs/src/reference/cli.md) — the same section `compile`'s per-pass report and `UNCOMPILED_SOURCE` both name |
| A crate's version | `workspace.package.version` and both `[workspace.dependencies]` requirements, together — `cargo release` does all three; a `version = "…"` line in a member manifest is a mistake |
| The formatter's style (a `STYLE` number, a key table, a layout rule) | `docs/src/reference/fmt.md` (`fmt_style_drift_test`), the fixture pair under `tests/fixtures/fmt/` the rule owns, **and** `examples/` + `tests/e2e/` reformatted in the same commit — `fmt_examples_test` fails otherwise (`just fmt` does it) |
| A function's input field table (order included) | the function's table on `docs/src/reference/functions.md` — `fmt_style_drift_test` asserts the documented order is the registry's, because `fmt` orders inputs by it |
| A clippy rule (new, or its level/summary) | `rules::ALL`, `tests/fixtures/clippy/<id>/{fires,quiet}/` (`clippy_cases_test` refuses a rule without both; `quiet` must trip *no* rule), the table **and** a `### ` section on `docs/src/reference/clippy.md` (`clippy_docs_drift_test`), and `clippy_examples_test` must stay empty. Certain-only: `explain()` must name one of the admitted proof sources (engine evaluation, an ingress fact, engine semantics read from source, the registry, structural identity, the `-c` config) and when the rule is silent; a heuristic goes under "What is not a rule" on that page with its reason, not in the registry |
| A container-gated test module | the `#[ignore]` name filters in `.github/workflows/ci.yml` (`ci_filter_drift_test`) — a module missing from those lines runs *nowhere*, silently |
| A `crate::` reference from a lower module to a higher one | nothing — `module_layering_test` refuses it. A lower layer needing something from a higher one almost always means the thing itself is in the wrong place; move it down, or add a row to that test's `ALLOWED` with the reason |

## Reading the code: item-ID comments

Around 900 comments across `src/` open with a short code — `N10`, `R13`, `F35`, `W8`,
`S15`, and the `(R26)` referenced above. These are items from the pre-1.0 audits; resolve
one with `git log --grep=N10` (needs full history — `git fetch --unshallow` on a shallow
clone). They are a historical record, safe to ignore when reading or writing code, and
**new comments do not need one**. Two rules: every comment must stand on its own without
its ID, and when a comment's claim stops being true, correct the comment.

## Configuration

See `crates/orion-server/config.toml.example`. All settings have sensible defaults. Environment variables override via `ORION_SECTION__KEY` format (e.g., `ORION_SERVER__PORT=3000`).

### CLI Commands

```bash
orion-server                              # Start server
orion-server -c config.toml               # Start with config
orion-server validate-config              # Validate config (--format summary for a short view)
orion-server migrate                      # Run migrations
orion-server migrate --dry-run            # Preview migrations
orion-server fmt ./definitions            # Format definition files to the house style (--check for CI; one style, no configuration)
orion-server lint workflow.json           # Strict-validate one workflow (--deny-warnings to fail on advisories)
orion-server clippy ./definitions         # Advisory rules beyond lint, only where certain (--list, --explain <rule>; -c for the [vars]/[secrets] rules)
orion-server lint ./definitions           # Validate a whole definition set and the references between its files
orion-server dry-run -w wf.json -i in.json --stubs s.json --metadata m.json  # Execute a workflow offline with canned connector replies
orion-server test examples/workflow-tests # Run offline *.case.json workflow regression tests (--definitions <dir> to resolve $from/use)
orion-server compile ./definitions --name p --version 1.0.0 -o dist/package.json  # Compile a definition set ($from/use resolved) into a package artifact (--format dir|bulk for POST-per-file / bulk-import shapes)
orion-server test-connectivity            # Probe DB (and Kafka if enabled)
orion-server preflight                    # Scan stored channels/workflows for 1.0 breaks
orion-server dump-openapi                 # Print the OpenAPI 3.1 spec
orion-server package <export|lint|plan|apply|diff>  # Promote a package of channels+workflows+connectors between instances
```
