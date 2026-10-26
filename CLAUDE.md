# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Orion is a declarative services runtime written in Rust. It exposes business logic through channels (service endpoints) and workflows (task pipelines run by dataflow-rs), managed through a REST API. The server ships as a single binary with an embedded SQLite database; `orion-cli` deploys to it and carries the offline authoring tools. Only the UI lives in a separate repo (Orion-ui).

### Crates

A cargo workspace of fifteen crates, layered bottom-up. The server re-exports each library under its pre-split `crate::` path (`crate::engine`, `crate::server`, …), so `orion::<module>` paths in tests and editions keep working.

| Crate | What it is | Re-exported as |
|---|---|---|
| `orion-api` | The wire contract: DTOs, enums, the error envelope and `codes` registry, the v2 deployment DTOs (see *Wire contract*) | `crate::errors::FieldError`, `storage::models::*`, … |
| `orion-definitions` | Static definition primitives: the order-preserving, span-carrying JSON AST, source provenance, the SQL lexer, the house formatter (operator names and field order arrive as data) | — |
| `orion-extension-api` | What an edition implements: `ChannelDriver`, `ProviderBackend`/`SourceConnectorFacet`, `ConnectionPoolCache`, `PackageVerifier`, their contract types and `Registrar`. Depends only on serde, serde_json, thiserror, async-trait and `orion-api` (`extension_api_test.rs` pins this) | — |
| `orion-common` | Shared primitives below everything: `config/` (loading, validation, `ORION_SECTION__KEY` overrides, `env://`/`[vars]`), `errors.rs`, `crypto.rs`, `canonical.rs`, `config_encryption.rs`, `signatures.rs`, `metrics.rs`, `request_context.rs`/`trace_context.rs`, `http_body.rs` (every egress reads its body under a byte cap **while streaming**), `ssrf.rs`, `version.rs` (incl. the binary's build identity, recorded by `bootstrap::assemble`, read by `/health`) | `crate::config`, `crate::errors`, … |
| `orion-storage` | The state database: `DbPool`/`DbTransaction` over three backends, the embedded `migrations/{sqlite,postgres,mysql}/`, `models/`, `repositories/` | `crate::storage` |
| `orion-engine` | The workflow engine: Orion's `dataflow_rs::Function`s and operators, `loader.rs`, `steps.rs`, `execute.rs`, `functions/`, `kit/`, the connector registry and pools, the portable data dialect (`query/`), JWT, `service.rs` (the service errors functions raise). Names nothing above it — `channel_call`, `connector_call` and `cache_invalidate` reach the node through `ChannelCallHost`, `ConnectorCallHost` and `NamespaceInvalidator`, implemented in `runtime::functions` | `crate::engine`, `crate::connector`, `crate::query`, `crate::jwt` |
| `orion-plugin-host` | WebAssembly plugins: the Wasmtime sandbox and ceilings, `plugin.toml`, `PluginFunctionHandler` (one instance per manifest function, owning its field table), the artifact store and trust check, `wit/`. Wasmtime links here only. A plugin's world imports nothing (no clock, randomness, sockets, connectors, secrets), so `dry-run` runs it for real | `crate::plugin` |
| `orion-runtime` | The serving node: `channel/`, `runtime/` (incl. `state.rs` — `AppState`), `cron/`, `queue/`, `cluster/`, `extensions/`, `auth.rs`. Names neither the HTTP layer nor `definitions`/`validation` | `crate::channel`, `crate::runtime`, …; `crate::extensions` exposes only the composition types |
| `orion-authoring` | Definition sets on disk (`definitions/`: check, compile, fmt adapter, clippy, analysis, fix) and every check a definition passes (`validation/`) | `crate::definitions`, `crate::validation` |
| `orion-http` | The HTTP layer: `build_router`, admin/v2 admin/data routes, the middleware stack, TLS serving, the OpenAPI document | `crate::server` |
| `orion-server` | `bootstrap.rs`, `main.rs`, the operator subcommands (`cli/`) | the `orion` lib |
| `orion-client` | The one HTTP transport over the contract: `OrionClient` and the `paths` module. The only crate that owns reqwest for API calls | — |
| `orion-cli` | The CLI (its own `CLAUDE.md`): the admin client and the offline authoring tools | — |
| `orion-plugin-sdk` | The guest crate a plugin author links against: the `orion:plugin` bindings, a `Plugin` trait, `export_plugin!`. Depends on nothing else here; its copy of the WIT is kept byte-identical to `orion-plugin-host/wit/` by `plugin_sdk_test.rs` | — |
| `orion-docs-check` | Never published: its test binary, `tests/drift`, holds the drift guards (see *Docs and code are kept in sync by tests*) | — |

Test-support features: `orion-common`, `orion-storage`, `orion-engine` and `orion-runtime` each have a `test-support` feature carrying the helpers other crates' tests call — notably the engine's twin records (`twin::pinned`, records under the calling crate's `tests/fixtures/function-records/<module>.json`, blessed with `ORION_BLESS=1`) and `loader::testing`.

The plugin test fixture (`crates/orion-server/tests/fixtures/plugins/guest`) and the two example guests (`examples/plugins/fixed-width`, `examples/plugins/c4-rules`) are built on the SDK. After changing any of them, the SDK or the WIT, run the `build.sh` beside them and commit the rebuilt component; the `plugin-sdk` CI job rebuilds all three and runs the ABI cases and the examples' offline cases against the fresh bytes.

### Versioning and release

- **All crates share one version**, `workspace.package.version` in the root `Cargo.toml`, inherited with `version.workspace = true` and repeated only in the `[workspace.dependencies]` requirements. Bump it with `cargo release`, never by hand in a member manifest.
- The library crates have no release cycle of their own: `crates-publish.yml` publishes them as **riders** (skip-if-present, dependency order) right before `orion-server`. Lockstep versioning is what makes skip-if-present safe. A new library crate joins the rider list in `crates-publish.yml` and `.github/scripts/publish-crates.sh`.
- `orion-server` is the only binary crate on crates.io (the `orion-cli` name there is an unrelated crate); the CLI ships through the dist installers, the Homebrew tap, GHCR and `cargo install --git`. See `RELEASING.md`.
- `default-members` is `orion-server` and `orion-docs-check`; use `--workspace` or `-p orion-cli` for the rest.

### Wire contract (`orion-api`)

Two disciplines, by surface:

- The operational DTOs that predate v2 (`dto.rs`, `engine.rs` — traces, cron, health, audit, DLQ, engine status, functions) are **tolerant**: every field defaults, unknown fields are ignored, growing vocabularies are strings, so server/CLI skew keeps working.
- The v2 admin contract (`orion_api::v2` — DTOs, the deployment manifest, `vocab.rs`) is **strict**: required fields unless absence is documented, `deny_unknown_fields` on requests, enums for closed sets, typed sub-structs for server-generated parts, `Value` only for author- or engine-owned documents. Skew is negotiated instead: the server reports `v2::API_VERSION`, and `orion-cli` refuses a server reporting another (`orion_client::check_api_version`). Bump `API_VERSION` when a v2 change could make the previous client misread an answer or send a refused request (a field removed/renamed/retyped/made required, a request field added, an enum variant an answer carries); an answer gaining a field does not.
- The `utoipa` feature (enabled only by the server side) adds the `ToSchema` derives for the OpenAPI document.

### Toolchain and dependency rules

- **Rust 2024, MSRV 1.98** (`rust-version`; the `rust:1.98-slim` images; CI's MSRV job). The MSRV tracks Wasmtime's (stable minus two), so a Wasmtime upgrade can move it.
- **sqlx 0.9:** only a `&'static str` is accepted as a query bare; anything assembled at runtime is wrapped in `sqlx::AssertSqlSafe` — a deliberate assertion: wrap only text Orion rendered (a `sea-query` builder, `query::backend::sql`) or the documented raw-SQL escape hatch (`db_read`/`db_write`), never a string with a caller value interpolated. TLS is `tls-rustls-aws-lc-rs`: the shipped binaries link one crypto provider.
- **sea-query 1.0** puts comparison operators on `ExprTrait` (`use sea_query::ExprTrait;`), which makes a bare integer `.max(…)` ambiguous — write `Ord::max(a, b)`.
- **Ask the engine before writing a check about a workflow definition.** dataflow-rs 4 parses (`def::parse` with `engine::parse_options()`), compiles (`Compiler::compile_one`, running each function's `prepare`) and reports every issue at its authored path with an `IssueCode` and `Severity`. `engine::compile_row` is the one place a stored row is parsed and compiled; `engine::quarantines` the one rule for what quarantines a channel (every `Rejected`; a `Defect` only for `INPUT_PARSE` and `LIKELY_TYPO`; never an `Advisory`; an unclassified new severity quarantines). `validation::engine_advisories` reports the rest from the same issues. `steps.rs` and `operators.rs::operator_names` are thin adapters over the engine — do not reintroduce local copies.
- **Every walk over `tasks` goes through `engine::walk_steps`/`leaf_tasks`**, handed the workflow's `loop` as well: a step carrying `tasks` is a group, any step may be `terminal`, and `loop.setup` holds steps too. `tasks.as_array()` silently skips both.
- **`map`'s `append`/`extend` modes read their target**; a clippy rule reasoning about overwrites reads `StepFacts::reads()`, not a mapping's `logic` alone.
- **Every definition is parsed strictly** — `def::parse(json, engine::parse_options())`, the v3 compat reader off. A v3 spelling (`halt_on`, a task's `continue_on_error`, the function name `validate`, pre-1.0 input aliases) or an undefined key is Rejected at create and at load. A new parse site uses `parse_options()`, and one that reports refusals passes its issues through `engine::name_v3_spellings`. The workflow-level `continue_on_error` is Orion's API field (written as `on_failure: continue`), not a v3 spelling.
- **datalogic-rs and datavalue are reached through dataflow-rs** (`use dataflow_rs::datalogic_rs;`), never pinned directly; a file-level `use` does not reach an inner `#[cfg(test)] mod tests`. Only `datetime` (and `budget`, for `engine.ops_budget`) are Cargo features; the other operator families are selected at runtime. Orion's own operators are registered in `orion-engine/src/operators.rs`.

## Build & Development Commands

```bash
cargo build                        # Build (all features included)
cargo build --release              # Release build
cargo run -- --config ./config.toml  # Run with a config file

cargo test                         # default-members: the server and the drift guards
cargo test --workspace             # Everything — what CI runs
cargo test <test_name>             # One test by name

cargo clippy --workspace --all-targets  # Lint (matches CI)
cargo fmt --all                    # Format

just e2e                           # End-to-end suite (tests/e2e): the CLI against a real orion-server
```

**`just check` is the pre-PR gate** — exactly what the CI PR jobs run: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo test --doc`, and `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --lib`. Bare `cargo test`/`cargo clippy` skip `orion-cli` and the library crates' own suites — CI does not. Other recipes: `just openapi` (regenerate the committed spec), `just test-containers` (the Docker-gated suites), `just workflow-tests`, `just docs` / `just docs-preview`, `just fmt`.

**After changing routes or request/response schemas, regenerate the committed spec** — `cargo run -- dump-openapi > docs/openapi.json` (or `just openapi`); `openapi_test` fails while it is stale.

Docker: `docker build -t orion .` for the server; `docker build -f crates/orion-cli/Dockerfile -t orion-cli .` for the CLI.

Lint suppressions are `#[expect(...)]`, so a stale one fails the build; `#[allow]` only where a lint fires in some targets and not others (test helpers shared through `#[path]`), with the reason.

## Runtime Configuration

All capabilities are compiled into the one binary; behaviour is controlled at runtime:

| Capability | Configuration | Default |
|-----------|--------------|---------|
| Database backend | `storage.url` scheme (`sqlite:`, `postgres://`, `mysql://`) | SQLite |
| OpenTelemetry | `tracing.enabled` | Disabled |
| Trace persistence mode | `trace_storage.mode` (`sync` / `async` / `batch` / `off`), per-channel override via `config.tracing` | Sync |
| Cron scheduler | `cron.enabled` (an *active* cron channel on a node with it off is quarantined and `components.cron` degrades — never a silent no-op) | Enabled |
| WebAssembly plugins | `plugins.enabled` (off = no Wasmtime engine, a plugin upload answers `400`, a deployed plugin is a `disabled` load issue that quarantines its dependants). `[plugins.trust]` makes Ed25519 signatures mandatory | Disabled |
| TLS/HTTPS | `server.tls.enabled` | Disabled |
| Swagger UI / OpenAPI spec | `server.docs.enabled` (unset = enabled outside production) | Enabled outside production |
| SQL connectors | `db_read`/`db_write`/`data_query`/`data_write` | Always available |
| Cache connector | `cache_read`/`cache_write`/… over an in-process memory backend | Always available |

## Architecture

### Where things live

- `orion-server/src/`: `main.rs` (clap entrypoint), `bootstrap.rs` (the boot phases, below), `cli.rs` + `cli/` (`validate-config`, `migrate`, `test-connectivity`, `dump-openapi`), `lib.rs` (the re-exports), `test_support.rs`.
- `orion-http/src/`: `lib.rs` (`build_router`, the layer stack, TLS), `routes/` (`admin/`, `v2_admin/`, `data/`, `openapi/`), `admin_auth.rs`, `rate_limit.rs` (`GroupLimit`), `request_context.rs` (fills `orion_common::request_context` per request, client IP under the trusted-proxy policy), `trace_context.rs` (W3C extraction).
- `orion-runtime/src/`:
  - `channel/` — the registry and `ChannelLoader`, config, routing, request guards (`guards/apply_guards`), `cron.rs` (the schedule compiler: `CronDescriptor`, `plan_pass`).
  - `runtime/` — `generation.rs`, `load_issues.rs` (what a generation could not load; `/health`, applies and `GET /engine/status` read it), `tasks.rs` (the supervisor behind `/readyz`), `reload.rs` (`reconcile_deployment_revision`), `definitions.rs`, `authority.rs`, `functions/` (the engine's hosts), `state.rs`.
  - `cron/` — the reconciler (schedules → durable occurrences) and workers (claim → guards → `execute_admitted` → settle).
  - `cluster/` — `reconcile.rs` (the one cluster loop: the v2 reconciler, which also applies peers' circuit-breaker resets) and job leases.
  - `queue/` — async trace/audit processing, DLQ retry.
  - `extensions/` — the v2 extension boundary: `CompositionBuilder` → frozen `Composition`, `deployment_store/` (`ImmutableDeploymentStore`), `reconciler.rs` (`NodeReconciler`), `foundation.rs` (the built-in providers), `workflow_provider.rs`, `source_runner.rs`, `admission.rs`. Public surface: only the composition types; integration tests reach internals through `#[doc(hidden)] test_support`.
- `orion-authoring/src/definitions/`: `check.rs` (the cross-reference pass `lint <dir>` and `compile` share), `shared.rs` + `expand.rs` (the `$from`/`use`/`$each`/`$param` catalog and its scoped expansion, descending into task groups via `engine::is_group`), `compile.rs` (the ordered `Pass` pipeline; `residue()`), `clippy/` (`Rule` + `rules::ALL`), `analysis/` (step facts, compiled expressions, `operators.rs` classifying every operator), `fix.rs` (proven rewrites), `fmt/` (the adapter supplying this build's vocabulary), `provenance.rs`.

### Startup sequence (`main.rs` → `bootstrap.rs`)

CLI args → config (TOML + `ORION_SECTION__KEY` overrides, `env://` references resolved after the parse) → `orion::run(config, CompositionRoot::community())` (an edition calls it with its own builder) → tracing, metrics → DB pool + migrations → `bootstrap::assemble`, five phases, each its own function (`boot_phases_test.rs` drives them):

1. `open_storage` — the node's identity, the cluster runtime, the repositories, the channel loader.
2. `build_serving_components` — connector registry (empty until the head builds), HTTP client, plugin sandbox (only when `plugins.enabled`), runtime handle, caches, node identity + deployment store + revocation ledger + native scheduler, the composition frozen over them (a duplicate registration fails the boot), the managed-OAuth2 runtime.
3. `publish_first_generation` — generation 1: the serving registry, no workflows or channels.
4. `start_background` — the audit writer, trace queues and workers, cleanup, DLQ retry, cron reconciler + workers (after the queues — they produce trace work), the plugin epoch ticker.
5. `build_committed_head` — hydrate the deployment store, build the committed head (`engine.fail_on_connector_load_error` refuses the boot when a connection of it did not load), record this node's status.

Then readiness → the v2 reconciler and deployment GC → (in `main`) cluster tasks, the limiter prune, the Axum server → graceful shutdown on SIGTERM/SIGINT.

### Key patterns

- **A node serves exactly its committed v2 deployment** (`extensions::deployment_store`): definitions (workflow, channel, plugin) uploaded by digest (`POST /api/v2/admin/definitions`), connection revisions, and a manifest naming them applied under compare-and-swap (`POST /deployments`; rollback is a new revision). A channel member `requires` its workflow; a workflow member `requires` the connections it calls. With no deployment a node serves nothing. A definition set on disk is compiled by `orion-cli compile` and deployed with `orion-cli deployments apply -d`; runnable sets live in `examples/packages/`.
- **Channels:** sync/async REST and HTTP endpoints, and `cron` — the one protocol with no caller: a six-field schedule and fixed payload in `transport_config`, no route, refused at the HTTP ingress and by `channel_call`; its work reaches the engine only as a claimed occurrence from `cron_occurrences` or through `POST /admin/channels/{id}/trigger`. A channel's `protocol` is a string resolved through the `ChannelDriverRegistry`; `channel::guards::Transport` names core's ingress paths and their guard rows.
- **One runtime generation:** the engine, the channel estate and everything compiled against them are **one published value**, `RuntimeGeneration`, held in an `ArcSwap` behind `RuntimeHandle` (`state.runtime`). `publish` takes every half, so there is no way to store one. **Every ingress loads it exactly once per unit of work** — one HTTP request, one cron occurrence, one queued trace, one `channel_call` — and passes it down; two loads in a request can admit on the new estate and execute on the old engine. `concurrency_test::a_reload_publishes_exactly_one_generation` counts the stores.
- **Deployment builds:** `runtime::reconcile_deployment_revision` (`runtime/reload.rs`) reads a committed revision's members, loads its Connections into the connector registry, builds both halves off to the side and publishes them in one store, serialised by `reload_lock` (the only lock in the sequence). A failure publishes nothing — the previous generation keeps serving and `/health` reports `engine_reload: degraded`. It runs on apply/rollback, at boot, and from the reconciler loop, which is how peers converge. Bootstrap's generation 1 and this build are the only `RuntimeHandle::publish*` callers.
- **Channel estate:** `ChannelSnapshot` is the channel half of a generation — per-channel runtime config (rate limiters, compiled validation, backpressure, dedup stores, response caches), a `RouteTable`, the quarantine map — an immutable value. `ChannelLoader` builds the next one from the previous, carrying unchanged channels over by `Arc`.
- **Host functions:** each implements `dataflow_rs::Function` and `functions::definition::OrionFunction` in its own `orion-engine` module. Its `DEFINITION` holds retry policy, static validation, sparse field metadata and offline stub policy; its `#[derive(FunctionConfig)]` config supplies names, field order, descriptions, requiredness and output defaults (a `String` field is a literal required at load; `Interpolated`/`Expr` fields are evaluated per call). The `functions!` inventory in `functions/mod.rs` generates registration and the schemas behind `FunctionRegistry::builtin()` — add a function there once; never recreate a field table or a name-keyed lookup. `FunctionRegistry` exists because names are checked before an engine exists; `function_schema_test.rs` pins it to a live engine. `plugin::PluginFunction` carries its own field table, so only the instance decides which fields are templates. Self-contained functions (`crypto`, `jwt_*`, plugins) run for real offline.
- **Composition (the extension boundary):** an edition describes what its node serves with `extensions::CompositionBuilder` (`with_edition`, `with_channel_driver`, `with_provider_backend`, `with_providers`, `with_resource_pool`, `with_package_verifier`) and passes it to `orion::run` / `bootstrap::assemble`. It is frozen once into `state.composition`; every driver and provider lookup goes through it. The one mutable part is the `ProviderRegistry`'s admitted layer (admitted workflow packages, by digest), which can never shadow a composed backend. Revocation is core: the `RevocationLedger` is checked by `Admission::verify_package` and before every `dispatch_attempt`.
- **Connector registry:** a lock-free `ArcSwap` of configs with secret masking on API reads, and circuit breakers per `(channel, connector)` with LRU eviction. Db connections carry per-operation gates (`operations: { read, insert, update, delete, upsert, raw_write }`, all default `true`).
- **Repositories** (`orion-storage`): trait-based with SQL implementations — cron, traces, trace_dlq, audit_logs, oauth_state, runtime_nodes, cluster, shared_state — built over sea-query and held as `Arc<dyn …>` in `state.repos`. Definitions, connections and deployments live in the deployment store, not a repository.
- **Errors:** `OrionError` (`orion-common/src/errors.rs`) implements `IntoResponse` with the envelope `{"error": {"code", "message", …}}`. A refusal that is only a status, code and readable message is `OrionError::Http`, built with its constructor (`not_found`, `unauthorized`, `forbidden`, `conflict`, `payload_too_large`, `unsupported_media_type`, `method_not_allowed`, `upgrade_required`) and matched as `OrionError::Http { status: StatusCode::NOT_FOUND, .. }`; the remaining variants carry behaviour of their own. `response_parts` is the one redaction policy. Every v2 code is in the closed `orion_api::V2Code` registry, whose kind sets the status.
- **AppState** (`orion-runtime/src/runtime/state.rs`): the shared state, with `repos` and `caches` grouped and runtime-singular fields flat. Route handlers take it through Axum's `State` — except the data plane: `dynamic_handler` extracts `DataPlane` (`FromRef<AppState>`), the narrow view a data request may touch, built once at assembly.

### Middleware

In request order (`orion-http/src/lib.rs`): set/propagate `x-request-id` → the request-context scope → security headers (CSP, X-Frame-Options, nosniff, Referrer-Policy, Permissions-Policy) → `CatchPanicLayer` → CORS → HSTS (with TLS) → OTel extraction and the trace layer (with `tracing.enabled`) → HTTP metrics → compression (when enabled) → body limit → the route group's own layers → the route.

Admin auth and the platform rate limits are not global: `routes::api_routes` attaches them to each route group when the router is built. The admin router — every admin route plus its own `/api/v2/admin` and `/api/v2/admin/{*rest}` catch-all, so an unregistered admin path is answered there and never by a root data mount — gets the admin limit outside admin auth; the self-authenticating single-trace read the admin limit only; the data plane (`/api/v2/data` and every `server.data_mounts` prefix) the data limit; operational routes and the 404/405 fallbacks the default limit, with admin auth on `/metrics` where it is served. `admin_guard_test` holds every documented operation's `security` block to what an anonymous caller is answered.

### Request flow

```
HTTP request → router → dynamic_handler (State<DataPlane>)
  → the runtime generation, loaded ONCE
  → route resolution (REST pattern → channel name)
  → ingress guards (channel/guards apply_guards: rate limit, auth, origin, validation, dedup, response cache, backpressure)
  → the engine of that same generation: channel → workflow match (condition + rollout bucket) → tasks
  → response (cache store, JSON)
```

### API structure

- **Admin** (`/api/v2/admin/`):
  - **Deployments (v2):** `POST/GET /definitions` (content-addressed; upload refuses what would not serve), `POST /definitions/test` (run a workflow definition once, nothing stored), `GET /connections`, `GET/POST /connections/{id}/revisions`, `GET /connections/{id}/revisions/{rev}` (immutable; secrets only as `secret_locators`, package dependencies as `bindings`), `POST /connections/{id}/revisions/{rev}/probes`, `POST /deployment-plans`, `GET/POST /deployments` (CAS on the head; `rollback_to`; `reviewed_manifest_digest` required), `GET /deployments/{revision}[/convergence]`, connector packages (`GET /connector-packages[/{digest}]`, `PUT …/admission`, `POST …/revocations`), authoring, `POST /edition-transition-preflight`.
  - **Channels:** `POST /channels/{id}/trigger` — a manual run of a deployed cron channel, through the same claim and singleton path.
  - **Circuit breakers:** `GET /connectors/circuit-breakers`, `POST /connectors/circuit-breakers/{key}` (fans out to every node).
  - **Functions:** `GET /functions`. **Engine:** `GET /engine/status`.
  - **Cron:** `GET /cron/occurrences[/{id}]`, `POST /cron/occurrences/{id}/retry`, `GET /cron/status`.
  - **Cache:** `POST /cache/namespaces/{namespace}/invalidate` (the workflow form is `cache_invalidate`).
  - **Traces:** `GET /traces` (admin), `GET /traces/{id}` (admin credential or the per-submission `trace_token`).
  - **Audit logs** (the `X-Orion-Change-Context` header lands in `details`), **trace DLQ**, **backups** (SQLite `VACUUM INTO`, refused in cluster mode; no restore endpoint — see `docs/src/operate/backup-restore.md`).
- **Data** (`/api/v2/data/{*path}`): REST route match or channel name; a trailing `/async` submits asynchronously.
- **Operational:** `GET /health`, `/healthz`, `/readyz`, `/metrics`. **API docs:** `/docs`, `/api/v2/openapi.json` (gated by `server.docs.enabled`).

### Database

SQLite (default), PostgreSQL or MySQL, chosen from the `storage.url` scheme; all three migration sets are embedded and the matching one runs. Each starts at `001_v2_baseline.sql`; a 1.x database is refused by name rather than upgraded. Tables: the deployment store (`deployment_revisions`, `deployment_head`, `deployment_members`, `deployment_operations`, `artifact_objects`, `connection_revisions`, `provider_packages`, `provider_admissions`), `runtime_nodes`, `traces`, `trace_dlq`, `audit_logs`, the cron ledger (`cron_schedule_state`, `cron_occurrences`, `cron_singletons` — slot 0 of every `forbid` key — and `cron_singleton_slots`), and the cluster tables (`config_epoch`, `job_leases`, `connector_oauth_state`, `cluster_kv`, `cluster_rate_windows`, `source_claims`, `source_checkpoints`), judged on the database's clock.

**Migration rules** (details in `CONTRIBUTING.md#database-migrations`):

- Shipped migrations are **checksum-frozen**: never edit an existing `NNN_*.sql`; add a new file to **each** of the three directories.
- **The three sequences are independent after `001`** — the same number can mean different things per backend, so **name migrations, don't number them** in commits and docs. `tests/schema_parity.rs` catches a change applied to two backends out of three.
- Write migrations **expand/contract**: a rolling deploy runs old and new binaries against one database, so a release only *adds* schema; drop the old shape a release later.

## Testing

- **Integration tests** (`crates/orion-server/tests/integration/`): one binary, each file a module declared in `main.rs`. `common::test_app()` builds an in-memory SQLite node through the real `bootstrap::assemble`; drive it with `tower::ServiceExt::oneshot(json_request(…))` and read with `body_json()`. Deploy fixtures through `common::deploy` / `create_connector` etc. — the v2 path, as an operator would.
- **Other server binaries:** `tests/cluster/` (multi-node), `storage_postgres.rs`, `storage_mysql.rs`, `schema_parity.rs` (container-gated), `metrics_exposition.rs` (its own process for the global recorder). Container-gated modules in the integration binary and in `orion-cli`'s `authoring` binary are selected by name in `.github/workflows/ci.yml` (kept in sync by `ci_filter_drift_test`).
- **The offline tools** are tested through the `orion-cli` binary (`crates/orion-cli/tests/authoring/`). A server test that needs one finds `orion-cli` beside its own binary (`common::orion_cli_bin`) and skips with a note when a non-workspace build lacks it.
- **Library unit tests** run per crate; large test modules sit beside their code (`tests.rs` beside a `mod.rs`, `foo_tests.rs` beside `foo.rs` via `#[path]`).
- **Benchmarks:** `crates/orion-server/tests/benchmark/bench.sh` — `hey`-driven scenarios (`baseline`, `simple`, `complex`, `multi`, `concurrency`, `reload`, `plugin`, opt-in `cluster`).
- **End-to-end:** `tests/e2e/run.sh` (`just e2e`; the `cli-e2e` CI job) — shell suites driving a real server with the CLI; scenario cases in `examples/use-cases/`, runtime cases in `tests/e2e/cases/`. An apply replaces the head, so a test needing several services deploys them as one set. `TESTING.md` maps the whole estate.

## Docs and code are kept in sync by tests

Drift guards in `crates/orion-docs-check/tests/drift/` (and a few integration tests) make the code authoritative and fail the build when a document disagrees. They run in the default `cargo test`, so these edits are part of the change:

| Change | Also update |
|---|---|
| A config field or its `Default` | the section's page under `docs/src/reference/configuration/` + `config.toml.example` (`config_docs_drift_test`) |
| A metric name or label key (`orion-common/src/metrics.rs`) | `docs/src/reference/metrics.md` (`metrics_docs_drift_test`) |
| A task function or its input schema | `docs/src/reference/functions/` — the hub's summary and retry-safety tables plus the function's own page (`functions_docs_drift_test`) |
| A route, method or query parameter | the book's `curl` examples + `docs/openapi.json` (`docs_routes_drift_test`, `openapi_test`) |
| An audit `action` / `resource_type` | `docs/src/operate/audit-logs.md` (`audit_actions_drift_test`) |
| A `FieldError` code | `orion_api::error::field_codes::ALL` **and** `docs/src/reference/errors.md` — `field_codes_drift_test` also fails on a registered code nothing emits |
| A v2 error code | `orion_api::V2Code` and `docs/src/reference/errors.md` (`v2_error_codes_drift_test`) |
| A JSONLogic operator | `docs/src/reference/expressions.md` only — the vocabulary comes from the live engine; `jsonlogic_operators_test` checks both directions |
| A workflow-shape rule (task, group, `terminal`) | `validation/workflows/mod.rs::validate_workflow_tasks_schema` — its catch-all is `engine::compile_row`, so a rule the engine enforces needs no mirror, only a better message if wanted |
| A cross-reference check over a definition set | `definitions/check.rs` — one pass shared by `lint <dir>` and `compile`; give it a stable `check` id |
| An authoring convenience (new source-form syntax) | `definitions/compile.rs` — a `Pass` registered in `passes()` with a stable id, whose `residue()` mirrors the rewrite **exactly** (detect more and the admin API refuses what `compile` accepts; detect less and source form reaches the runtime). Document it under `docs/src/reference/cli/shared-definitions.md` |
| A crate's version | `workspace.package.version` and the `[workspace.dependencies]` requirements together — `cargo release` does it |
| The formatter's style | `docs/src/reference/fmt.md` (`fmt_style_drift_test`), the fixture pair under `tests/fixtures/fmt/`, **and** `examples/` + `tests/e2e/` reformatted in the same commit (`fmt_examples_test`; `just fmt`) |
| A function's input field table (order included) | the field table on `docs/src/reference/functions/<name>.md` (`fmt_style_drift_test` — `fmt` orders inputs by it) |
| A clippy rule (new, or its level/summary) | `rules::ALL`, `tests/fixtures/clippy/<id>/{fires,quiet}/` (`clippy_cases_test`; `quiet` must trip no rule), the table on `docs/src/reference/clippy/index.md` and the rule's own page (`clippy_docs_drift_test`), and `clippy_examples_test` stays empty. A rule with a `Fix` ships a `tests/fixtures/clippy-fix/<case>/{before,after}/` pair. Certain-only: `explain()` names an admitted proof source (engine evaluation, an ingress fact, engine semantics read from source, the registry, structural identity, the `-c` config) and when the rule is silent; a heuristic goes on `docs/src/reference/clippy/not-a-rule.md` instead |
| A container-gated test module | the `#[ignore]` name filters in `.github/workflows/ci.yml` and `just test-containers` (`ci_filter_drift_test`) — a module missing from them runs nowhere |
| A cron scheduling rule (misfire policy, DST, a `transport_config` bound) | `channel/cron.rs` (pure, exhaustively unit-tested) and its authoring twin `validation/cron.rs`, pinned to each other by `cron_guards_and_validation_agree` |
| A function record (`tests/fixtures/function-records/`) | a re-bless (`ORION_BLESS=1`) is a behaviour change, reviewed like code; `function_records_test` refuses machine-specific content |
| A `crate::` reference from a lower module to a higher one | nothing — `module_layering_test` refuses it; move the thing down instead, or add an `ALLOWED` row with the reason |

## Comments

Around 900 comments open with an audit code (`N10`, `R13`, `F35`, …); resolve one with `git log --grep=N10` (full history needed). They are a historical record — **new comments do not need one**. Every comment must stand on its own without its ID, state invariants and reasons rather than history (history belongs in commits), and be corrected when its claim stops being true.

## Configuration

See `crates/orion-server/config.toml.example`. Every setting has a default; `ORION_SECTION__KEY` overrides any of them (e.g. `ORION_SERVER__PORT=3000`), derived from the config types themselves.

### CLI commands

```bash
orion-server                              # Start the server
orion-server -c config.toml               # … with a config file
orion-server validate-config              # Validate and print the effective config (--format summary)
orion-server migrate [--dry-run] [--wait 60s]  # Run (or preview) migrations; --wait retries connection failures only
orion-server test-connectivity            # Probe the state database
orion-server dump-openapi                 # Print the OpenAPI 3.1 spec

orion-cli fmt ./definitions               # Format definition files (--check for CI; one style, no configuration)
orion-cli lint workflow.json              # Strict-validate one workflow (--deny-warnings fails on advisories)
orion-cli lint ./definitions              # Validate a set and the references between its files
orion-cli clippy ./definitions            # Advisory rules, only where certain (--list, --explain <rule>, -c <server config>, --fix [--check])
orion-cli dry-run -w wf.json -i in.json --stubs s.json  # Run a workflow offline with canned connector replies
orion-cli test examples/workflow-tests    # Offline *.case.json regression tests
orion-cli compile ./definitions -o dist/deployment  # A deployment directory for `orion-cli deployments apply -d`
orion-cli sql check ./definitions         # Prepare every db_read/db_write statement as its connector's role; executes nothing
orion-cli plugin sign plugins/ --key signer.pem  # Sign components for [plugins.trust] (also digest|keygen|pubkey|verify)
orion-cli deployments apply -d dist/deployment   # Deploy a compiled directory
```
