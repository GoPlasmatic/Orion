# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - 2026-07-26

Multi-instance (HA) support: N replicas of `orion-server` behind a load
balancer, sharing one Postgres/MySQL + Redis, behave as a single logical
system. With `cluster.enabled = false` (the default) behavior is unchanged.
See [Scalability](https://goplasmatic.github.io/Orion/features/scalability.html)
and [Availability](https://goplasmatic.github.io/Orion/features/availability.html)
for the cluster architecture.

> **Upgrading from 0.3.0?** Read the
> [Upgrade Guide](https://goplasmatic.github.io/Orion/getting-started/upgrading.html).
> It expands every item in Breaking below into what changed, how you'll notice,
> and what to do — with the SQL and PromQL to check your deployment first.

### Breaking

- **Rate-limit client identity is the TCP peer address.** Forwarded headers
  (`X-Forwarded-For` / `X-Real-IP`) are honored only when the peer falls inside
  the new `rate_limit.trusted_proxies` CIDR list, which is **empty by default**.
  Deployments behind a proxy, load balancer, or ingress that do not configure it
  will collapse every client into a single bucket. Applies when
  `rate_limit.enabled = true` (still `false` by default), and to per-channel
  `key_logic` expressions referencing `client_ip`. A malformed entry is a hard
  startup error even when rate limiting is disabled.
- **Metrics labels changed — dashboards and alerts will break silently.**
  `rate_limit_rejections_total` lost its unbounded `client` label and gained
  `scope` (channel name, or `admin` / `data` / `operational`). Channel-labelled
  metrics (`messages_total`, `message_duration_seconds`,
  `channel_executions_total`) now emit the literal `_unknown` for unregistered
  channels on the HTTP and queue paths. No metric was renamed or removed.
- **Channels with unparseable `config_json` or uncompilable `validation_logic`
  refuse to load, in all modes.** Previously a warning, after which the channel
  served with its validation, dedup, rate limit, cache, and backpressure guards
  silently disabled. A stored config that was quietly broken now exits the
  process at startup, and fails engine reload — plus every admin mutation that
  triggers one — with `500 CONFIG_ERROR`. Registry rebuilds are all-or-nothing,
  so a refusal leaves the running engine untouched.
- **Kafka delivery is at-least-once.** Offsets advance only on successful
  processing or a *confirmed* DLQ write. With `kafka.dlq.enabled = false` (the
  default) a poison message now blocks the consumer and retries with capped
  backoff (1s → 60s) instead of being dropped; because messages are processed
  sequentially, this halts every subscribed partition on that instance.
  **Enabling `[kafka.dlq]` is the recommended action.**
- **Data-plane error bodies are sanitized.** Entries in `errors[]` are reduced
  to a code, a fixed generic message, and an optional `task_id`; correlate via
  the top-level `request_id` (also the `x-request-id` header) and read full
  detail from the persisted trace. Cached responses store the sanitized body.
- **Response cache keys fold in method, route params, and query string.**
  Existing cached entries are orphaned — never mis-served — and expire by
  `cache.ttl_secs` (default 300s).
- **Open circuit breakers return `503 CIRCUIT_OPEN`** instead of
  `500 ENGINE_ERROR`. No `Retry-After` header. With `continue_on_error: true`
  the request still returns `200` with a sanitized `TASK_ERROR`; alert on
  `circuit_breaker_rejections_total` rather than the status code.
- **A full trace queue returns `503`** (code `SERVICE_UNAVAILABLE`, message
  `Trace queue is full …`) on the async submission path instead of blocking
  indefinitely. Sized by `queue.buffer_size` / `queue.max_queue_memory_bytes`.
- **Unimplemented secret schemes are rejected.** `vault://`, `aws-sm://`,
  `gcp-sm://`, and `azure-kv://` in connector configs were passed through and
  **used as literal passwords**; the connector is now skipped at load with an
  `ERROR` log. A connector that appeared to work was never authenticating as
  intended — rotate the credential.
- **`GET /api/v1/admin/connectors` redacts userinfo inside URL-shaped values**
  at any depth (`https://user:******@host`), which finally covers `url` and
  `brokers[]`. Credential-free URLs are still returned in full. Do not
  round-trip a connector config through `GET` → `PUT`: updates replace
  `config_json` wholesale and would persist the mask.
- **`GET /api/v1/admin/audit-logs` rejects unknown query parameters with `400`**
  instead of silently returning unfiltered results. No other endpoint changed.
- **`db_read` returns values for `float4`/`REAL` and blob columns** instead of
  `null`, and errors on genuinely undecodable columns and non-finite floats.
  Blobs stringify as UTF-8 when valid, else lowercase hex. Also affects
  `data_query` and `data_write`'s `RETURNING` path. A `null` in a result now
  means only SQL NULL.
- **Trace read endpoints require admin auth.** `GET /api/v1/data/traces` and
  `/traces/{id}` return `401` for previously-open callers when
  `admin_auth.enabled = true`. No effect when admin auth is disabled.
- **Rollout bucketing is caller-stable, not random per request** — see Added.
  A canary now exposes a stable subset of callers rather than re-drawing on
  every call; aggregate percentages are unchanged.
- **The Helm chart and HA compose default to `ORION_ENV=production` and require
  admin API keys.** `helm install` without `adminAuth.apiKeys` or
  `adminAuth.existingSecret` fails at template time by design
  (`devStack.enabled=true` is the dev escape hatch); `docker compose -f
  docker-compose.ha.yml up` aborts without `ORION_ADMIN_API_KEYS`. Note that
  `environment = "production"` also rejects the CORS wildcard, and the default
  `cors.allowed_origins` is `["*"]` — set explicit origins before flipping.
- **Removed:** the unread `backpressure.queue_depth` channel-config field.
  Stored configs still carrying the key deserialize normally (there is no
  `deny_unknown_fields`), so this needs no migration.

### Added

- **`[cluster]` config section** — `enabled`, `redis_url`,
  `epoch_poll_interval_ms`, `instance_id` (auto-generated UUID when empty).
  Cluster mode requires Postgres/MySQL storage and a shared Redis; startup
  refuses SQLite.
- **Config-change propagation** — every admin mutation (channels, workflows,
  rollout, connectors, manual reload) advances a `config_epoch` row; each node
  polls it and resyncs from the DB, so a change made through any node reaches
  all nodes. This also fixes connector edits, which previously propagated to
  no other node at all. Circuit-breaker resets fan out over the same bus.
- **Cluster-shared dedup, response cache, and rate limits** — channels without
  an explicit cache connector use the shared cluster Redis for idempotency
  dedup and response caching; per-channel rate limits enforce as a shared
  Redis fixed window (~configured rate across ALL replicas combined). In
  cluster mode, a channel whose backend would silently fall back to per-node
  memory refuses to load instead.
- **DLQ claim leases + job single-flight** — DLQ retries claim rows via
  `FOR UPDATE SKIP LOCKED` leases (each entry retried by exactly one node;
  expired leases self-recover), and trace cleanup / DLQ retry acquire a job
  lease per tick so only one node runs them. New `queue.dlq_batch_size` /
  `queue.dlq_lease_secs`.
- **`storage.auto_migrate`** (default `true`) — set `false` in multi-replica
  deployments: pending migrations become a hard startup error and
  `orion-server migrate` runs as a deploy step.
- **Kafka static membership** — cluster mode sets `group.instance.id` and
  `kafka.session_timeout_ms` so rolling restarts rejoin without a full group
  rebalance; epoch-driven consumer restarts are jittered 0–5 s.
- **Postgres/MySQL storage-backend test binaries** (`storage_postgres`,
  `storage_mysql`) and a multi-node cluster test binary (`cluster`) running
  two full nodes against Postgres + Redis testcontainers in CI.
- **Helm chart** (`deploy/helm/orion`) — cluster-mode Deployment with
  readyz/healthz probes, pre-upgrade migration Job, HPA, PDB, and an optional
  throwaway dev Postgres/Redis; validated on a 3-replica kind install.
- **HA reference compose** (`docker-compose.ha.yml`) — nginx LB → 2× Orion
  (cluster mode) → shared Postgres + Redis with a one-shot migrate service,
  plus `deploy/ha/rolling-drill.sh`, a zero-downtime rolling-deploy drill.
- **Sticky canary rollouts** — the rollout bucket is now a stable hash of the
  caller identity (`engine.rollout_sticky_header`, else the forwarded client
  IP), so the same caller gets the same version on every request and replica;
  previously assignment was random per request.
- **Per-instance observability** — `service.instance.id` OTel resource
  attribute and `instance_id` on request spans in cluster mode.
- Env overrides: `ORION_CLUSTER__*`, `ORION_STORAGE__AUTO_MIGRATE`,
  `ORION_STORAGE__{MAX,MIN}_CONNECTIONS`, `ORION_STORAGE__IDLE_TIMEOUT_SECS`,
  `ORION_QUEUE__DLQ_{RETRY_ENABLED,MAX_RETRIES,POLL_INTERVAL_SECS,BATCH_SIZE,LEASE_SECS}`,
  `ORION_KAFKA__SESSION_TIMEOUT_MS`, `ORION_SERVER__SHUTDOWN_FORCE_TIMEOUT_SECS`,
  `ORION_KAFKA__TOPICS`, `ORION_KAFKA__DLQ__{ENABLED,TOPIC}`,
  `ORION_CORS__ALLOWED_ORIGINS`, `ORION_CHANNELS__{INCLUDE,EXCLUDE}`,
  `ORION_ENGINE__ROLLOUT_STICKY_HEADER`.

### Fixed

- **TLS was unusable — `server.tls.enabled = true` panicked at boot.**
  `RustlsConfig::from_pem_file` failed with *"Could not automatically determine
  the process-level CryptoProvider"*: rustls 0.23 auto-selects a backend only
  when exactly one is enabled, and Orion's dependency graph enables both
  (`axum-server` + `reqwest` pull `rustls/aws-lc-rs`; `mongodb` + `sqlx` pull
  `rustls/ring`). The server now installs the `aws-lc-rs` provider explicitly
  before loading certificates. **If you tried HTTPS, hit the panic, and
  terminated TLS at a proxy instead, it works now.** Covered by new TLS
  integration tests — the test debt was the bug.
- **MySQL as Orion's own storage backend never worked** — the migration set
  used mysql-client `DELIMITER` directives, TEXT columns with defaults, and
  TEXT primary keys, none of which MySQL/sqlx accept. Rewritten with the
  VARCHAR/datetime idiom; covered by container tests.
- **Postgres storage was unusable at runtime** — models decode `i64` but
  columns were `INT4` (every repository read failed), and chrono timestamps
  were bound as TEXT, which Postgres rejects against timestamp columns. Both
  fixed (new `004_bigint_columns.sql`); covered by container tests.
- **Dedup idempotency keys are now channel-scoped** (`dedup:{channel}:{token}`)
  — raw tokens previously collided across channels sharing a backend — and a
  dedup-store outage now fails open (requests allowed, `dedup_backend` error
  metric) instead of rejecting everything with 409.
- **Trace read endpoints require admin auth** — `GET /api/v1/data/traces` and
  `/traces/{id}` return full payloads but were unauthenticated even with
  `admin_auth.enabled = true`.
- **Rolling-deploy drain** — on SIGTERM, `/readyz` now flips to 503
  immediately while the node keeps serving through `shutdown_drain_secs`
  (so the LB drains it gracefully), then stops accepting and bounds the
  in-flight wait with `server.shutdown_force_timeout_secs`. Previously TLS
  stopped accepting instantly and plain HTTP never withdrew readiness.
- **`queue.dlq_max_retries` is honored** (the enqueue path hardcoded 5) and
  values `< 1` are rejected at startup; `traces.channel_id` is now populated
  on every insert path; active-immutability triggers now exist on Postgres
  and MySQL, not just SQLite.

### Changed

- Filesystem backups (`/api/v1/admin/backups`) return `400` in cluster mode —
  the file would land on one arbitrary node; use managed-DB snapshots/PITR.
- `docs/src/features/scalability.md` and `availability.md` rewritten around
  cluster mode (the multi-node curl-loop reload workaround is obsolete).

### Removed

- Unread `backpressure.queue_depth` channel-config field (backpressure
  rejects immediately at `max_concurrent`; there is no wait queue).

## [0.3.0] - 2026-07-18

This release introduces the portable data dialect: backend-neutral `data_query` and
`data_write` task functions that render one declarative filter/envelope format to
SQL (SQLite/PostgreSQL/MySQL), MongoDB, and Elasticsearch — so workflows can read
and write data without embedding backend-specific queries. `db_read`/`db_write`
remain available as the raw-SQL escape hatch.

### Added

- **`data_query` portable read dialect** — declarative, backend-neutral queries
  (filter, sort, pagination, projection) rendered per connector backend: SQL,
  MongoDB `find`, and Elasticsearch. Supports an inline schema registry with
  relations, and `include` for fetching nested related records with hydration.
- **`data_write` portable write dialect** — insert/update/delete/upsert with
  SQL/MongoDB/Elasticsearch parity and a cross-backend end-to-end test suite.
- **Per-operation connector gates** — db/es connector configs accept
  `operations: { read, insert, update, delete, upsert, raw_write }` (all default
  `true`), enforced by the data handlers; e.g. set `"delete": false` to make a
  connector delete-proof.
- **One-command quickstart** (`examples/quickstart.sh`), a connector-backed
  `postgres-orders` example, and Getting Started guides (CLI setup, first
  connector, AI prompt pack). All examples are linted and deployed end-to-end in CI.
- **Docs**: Dev & Prod topology pages with interactive architecture diagrams,
  terminal recordings (GIFs + asciinema), a comparison page, and a benchmark chart.
- **AI-consumable docs**: `llms.txt` and generated `llms-full.txt` published with
  the docs site, alongside the checked-in OpenAPI 3.1 spec.
- **Security & community**: `SECURITY.md`, `CODE_OF_CONDUCT.md`, issue templates,
  CodeQL (security-extended) and cargo-audit in CI, `ADOPTERS.md`.

### Changed

- Dependency upgrades: `datalogic-rs` 5.0 → 5.1, `dataflow-rs` 3.0.1 → 3.0.2,
  `datavalue-rs` 0.2.2 → 0.2.3 (benchmarked perf-neutral), `redis` 1.2 → 1.3.
- Docker release workflow publishes to GHCR only (ACR mirror removed).

### Security

- Updated `Cargo.lock` to clear RUSTSEC-2026-0185 (`quinn-proto`).

## [0.2.0] - 2026-05-27

This release upgrades the workflow engine to dataflow-rs 3.0 / datalogic-rs 5 and
adds a large set of governance, validation, and operability features. JSONLogic
compilation now happens at engine-construction time, yielding sizeable throughput
gains (+48% on complex workflows, +120% on multi-workflow scenarios) and lower P99
latency across every benchmark scenario versus the v0.1.x baseline.

### Breaking Changes

- **Engine upgrade to dataflow-rs 3.0 + datalogic-rs 5.** JSONLogic is compiled once
  at engine build time rather than per request.
- **Connector `api_key` field removed** in favour of `api_keys`. Update any connector
  configs still using the singular field.
- **Channel/connector create & update DTOs are now strongly typed enums.** Invalid
  `channel_type`, `protocol`, or connector `type` values are rejected at
  deserialization with `400` (values remain case-insensitive; v0.1 lowercase wire
  values are still accepted).
- **Profile output is namespaced** under `_orion.profile` with `version: 1`.

### Added

- **Configurable trace storage modes** — `sync`, `async`, `batch`, or `off` — as a
  global default with per-channel override via `config.tracing`.
- **Per-request workflow profile mode** for timing/inspecting task execution.
- **Per-task execution traces** captured when a channel opts in.
- **Structured error envelope** with field-pathed `FieldError` details, plus
  collection of all protocol-required-field errors in a single response.
- **Per-function input schema validation** for workflow task functions.
- **Bulk import** for channels and connectors, with `?dry_run=true` preview.
- **Strict validation of channel `config_json` at create time.**
- **Config & connector variable substitution** — `${VAR}` / `${VAR:-default}` in
  config TOML and connector configs.
- **`env://` secret references** resolved in connector configs.
- **New CLI subcommands:** `lint`, `dry-run`, and `test-connectivity`.
- **OpenAPI coverage** for the audit, backup/restore, and functions endpoints.

### Changed

- **Performance:** roughly halved per-request CPU by sharing `AppState` via `Arc` and
  gating compression/metrics work.
- **OpenTelemetry** bumped to 0.32 / 0.33; refreshed transitive dependencies
  (`rand` 0.10.1, `tokio` 1.52, and others).
- Distributed config validation into per-struct implementations; decomposed the
  `main.rs` startup sequence; split oversized handlers and centralised admin reload,
  trace-filter, and error-mapping logic.
- Renamed `connector::types` module to `connector::config`.
- Refreshed README, `docs/`, and `tests/README`, and added v0.2.0 / v3.0.0 benchmark
  result sets alongside the v2.1.5 baseline and trace-mode comparison.

### Fixed

- Clippy lints and formatting cleaned up across the crate and test suite.

## [0.1.1] - 2026-04-11

Earlier release. See the Git history for details.

## [0.1.0]

Initial release.

[1.0.0]: https://github.com/GoPlasmatic/Orion/compare/v0.3.0...v1.0.0
[0.3.0]: https://github.com/GoPlasmatic/Orion/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/GoPlasmatic/Orion/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/GoPlasmatic/Orion/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/GoPlasmatic/Orion/releases/tag/v0.1.0
