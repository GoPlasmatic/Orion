# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Security

- **Admin credential guessing is now metered and throttled.** The middleware
  stack was registered so that admin auth ran *outside* rate limiting; since it
  returns 401 without invoking the inner service, a wrong key never reached the
  limiter. The layer order is corrected, and failed admin authentication now
  applies a per-client exponential backoff (5 free attempts, then 500 ms
  doubling to a 30 s cap, cleared on success). Failures are counted by the new
  `admin_auth_failures_total{reason}` metric instead of the shared
  `errors_total{type="auth_failure"}`.
- **A filter matching every row no longer bypasses the `data_write` safety
  guard.** `{"op":"delete","target":"t","filter":{"and":[]}}` and other
  tautological filters skipped both the `"all": true` acknowledgement and
  `write.allow_unfiltered`, deleting every row. The guard now derives from the
  lowered condition rather than the presence of a `filter` key.

### Breaking

- **The `storage` connector type is removed.** It was accepted, validated,
  persisted and listed by `GET /connectors` for the whole 0.x line with no
  handler behind it — `POST /connectors` returned 201 and every workflow
  referencing the connector failed at request time. The documentation
  advertised S3, GCS and local-filesystem support with a full field table for
  something that did not exist; that section is gone.

  `connector_type: "storage"` is now rejected at create. An existing stored row
  is reported as a connector load issue (`stage: "removed_type"`) naming the
  removal, visible on `/health` and `GET /api/v1/admin/connectors` — and fatal
  at boot when `engine.fail_on_connector_load_error = true`. **Delete or
  disable such connectors before upgrading.** Nothing that worked stops
  working: there was never a working configuration to preserve.
- **An unknown key in the config file is now a startup error.** Every config
  struct was `#[serde(default)]` with no unknown-field rejection, so
  `[server] wrokers = 4`, or a whole misspelled section, booted clean with
  defaults and no way to notice. All 24 structs now carry
  `deny_unknown_fields`, and the error names the offending key.

  This covers the **config file only**. A misspelled `ORION_*` environment
  variable is still ignored silently: overrides are read by name rather than
  deserialized, so there is nothing for serde to reject. Check spelling against
  `docs/src/configuration/reference.md`.
- **One output-field name across every function: `output`.** `http_call` and
  `channel_call` called their destination path `response_path` while the other
  eight handlers called it `output` — two names for one concept, and the
  most-touched field in the task JSON contract. Both handlers now take
  `output`. `response_path` is still accepted so 0.3.x workflows load
  unchanged; when a task carries both, `output` wins.

  | Function | Pre-1.0 | 1.0 | Default if omitted |
  |---|---|---|---|
  | `http_call` | `response_path` | `output` | response discarded |
  | `channel_call` | `response_path` | `output` | `"data"` |
  | the other eight | `output` | `output` | `"data"` |

  The differing defaults are deliberate and unchanged in this release.
- **Every metric is renamed with an `orion_` prefix** — `messages_total` is now
  `orion_messages_total`, and so on for all 33 families. The bare names were
  generic enough to collide in a shared registry (`errors_total`,
  `active_workflows`, `db_pool_size`). **Update dashboards and alert rules
  before upgrading.**
- **Histograms are now real Prometheus histograms.** Without configured buckets
  the exporter rendered all seven `*_seconds` families as *summaries with
  pre-computed quantiles*, which cannot be aggregated across replicas —
  directly at odds with cluster mode. Queries using
  `histogram_quantile()` over `_bucket` series now work; queries reading the
  old summary quantiles must be rewritten.
- **In cluster mode every metric carries an `instance` label** identifying the
  replica. Recording rules that aggregate without `by`/`without` may need
  updating.
- **Plaintext `admin_auth.api_keys` entries must be at least 32 characters.**
  Previously `api_keys = ["a"]` was a valid production credential. Shorter keys
  are a hard config error when `environment` starts with `prod`, and a warning
  otherwise. `sha256:` entries are exempt. Generate keys with
  `openssl rand -hex 32`.
- **`rate_limit.endpoints.admin_rps` now defaults to `20`** instead of being
  unset. Previously the admin plane fell back to `default_rps` (100) — the same
  budget as the anonymous data plane. Set it to `null` (or an empty string via
  the environment variable) to restore the fall-back.
- **401, 429 and recovered-panic 500 responses now carry security headers and
  `x-request-id`,** and the error envelope for them includes `request_id`.
  Clients asserting on the absence of these will see new headers and one new
  body field.
- **Browser preflight (`OPTIONS`) to `/api/v1/admin/*` is now answered by the
  CORS layer** rather than rejected with 401. Any client relying on preflight
  failing closed should note the admin API was previously unusable from a
  browser whenever `admin_auth.enabled = true`.

### Fixed

- **The shipped `config.toml.example` now loads on a clean machine.**
  Placeholder substitution runs over the raw file text before TOML parsing, so
  the `${VAR}` in the header comment that *documents* the placeholder syntax —
  and three `${CONFLUENT_API_KEY}`-style examples further down — were read as
  required variables. Copying the example and starting Orion failed with
  *"Required environment variable 'VAR' is not set"*. The comments now use the
  `$$` escape, and the drift test loads the file through the real entry point
  instead of only parsing it as TOML.
- **One unusable workflow no longer takes down the whole instance.** Task input
  parsing runs inside engine construction, after the loader has decided what to
  load, so a stored row that fails it aborted the process at boot and took every
  channel on every node down on reload — defeating the per-channel quarantine.
  Unregistered function names and malformed `channel_call` inputs are now
  detected during the load and quarantine only their own channel.
- **`channel_call` accepts a `channel_logic`-only task.** The schema, docs and
  validation rule all declare `channel` optional when `channel_logic` is given,
  but the input struct required it, so such a workflow passed admin validation
  and then failed the engine build with `missing field 'channel'`.
- Unknown or archived channels return 404 on the data plane rather than a
  generic engine error.
- `channel_call` refuses a missing target instead of failing opaquely; the
  recursion depth and cycle guards are now covered by tests.
- TTL stores and circuit-breaker cooldowns use a monotonic, pausable clock, so
  a wall-clock step no longer extends or shortens either.

### Added

- **`orion_build_info{version, git_hash, build_timestamp}`** — the standard way
  to answer "which build is each replica running?" from Prometheus. Previously
  that information existed only in `--version`, one boot log line, and the
  admin-gated `/health` body, none of which a scrape can join against.
- **`orion_admin_auth_failures_total{reason}`** — rejected admin credentials,
  split out from the shared `errors_total{type="auth_failure"}` so credential
  guessing can be alerted on without also matching `panic`, `dedup_backend`
  and a dozen other unrelated call sites.

### Changed

- CI and CodeQL now run on `release/**` and `v*` branches. The release
  workflows require a successful CI run at the tag SHA, which no commit on a
  release branch could previously have.

## [1.0.0] - 2026-07-27

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

### Security

- **Credential headers are masked before entering workflow metadata.**
  `authorization`, `cookie`, `proxy-authorization` and `x-api-key` arrive in
  `metadata.headers` as `"******"` — previously their plaintext values were
  persisted into `traces.result_json` (async) and `trace_dlq.metadata_json`.
  Header *presence* is still testable from `validation_logic`. If a channel
  used `rollout.sticky_header` with a credential header, switch to a
  non-credential header — all callers now hash to one bucket otherwise.
- **Trace reads no longer expose the submitter's request context.**
  `GET /api/v1/data/traces/{id}` strips `context.metadata` (the request
  header map) from the served message, and `GET /api/v1/data/traces` returns
  payload-free rows — `input_json`, `result_json` and `task_trace_json` are
  served only by the single-trace GET. Rows written before this release
  still hold plaintext headers at rest; the projection covers reads of them.
- **Async trace reads are scoped to the submitter.** The 202 from
  `POST /{channel}/async` now carries a one-time-shown `trace_token`;
  polling `GET /traces/{id}` requires it (`x-trace-token` header or
  `?token=`) or an admin credential. Update polling clients to pass the
  token. Sync traces and pre-upgrade rows keep the admin trust model.
  New migration adds `traces.access_token_hash` on all three backends.
- **`/health` serves topology detail only to authorized callers** when
  admin auth is enabled: anonymous callers get status, version, uptime and
  coarse per-component states; `git_hash`, `build_timestamp`,
  `workflows_loaded`, the circuit-breaker map, connector load failures and
  quarantined channels (names and reasons) require the admin key. With auth
  disabled the body is unchanged.

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
- **Removed:** the unread `cors.allowed_methods` and `cors.allowed_headers`
  channel-config fields. Only `cors.allowed_origins` was ever enforced —
  per-channel preflight is not implemented — so setting them was a silent
  no-op. Same no-migration note as above.
- **Every admin list endpoint shares one pagination envelope.**
  `GET /api/v1/admin/audit-logs` (and the new trace-DLQ list) now return the
  flat `{data, total, limit, offset}` shape the workflow/channel/connector
  lists always used, instead of a nested `{data, pagination: {…}}`.
- **Malformed admin request bodies are rejected uniformly with 400 + field
  details.** The four workflow endpoints that still surfaced axum's plain-text
  422 (`PATCH …/status`, `PUT …/rollout`, `POST …/test`,
  `POST /workflows/import`) now use the same extractor as every other admin
  route, and query-string parse failures return the standard JSON error
  envelope instead of plain text.

### Added

- HTTP connector `retry_non_idempotent` (default `false`): opt POST/PATCH back
  into the retry loop. Off by default because a timed-out POST may already
  have been applied — enable only where the endpoint honours an idempotency
  key the workflow sets in `headers`.
- Elasticsearch connector `max_response_size` (default 10 MB), matching the
  HTTP connector's cap.
- `POST /{channel}/async` responses carry `trace_token`; `GET /traces/{id}`
  accepts it via the `x-trace-token` header or a `?token=` query parameter.
- `engine.fail_on_connector_load_error` (default `false`): refuse to start when
  an enabled connector cannot be loaded, so a bad rollout fails at boot where
  the orchestrator catches it rather than at request time hours later. Startup
  only — a hot reload never takes a running process down.
- `GET /health` reports two new degraded states: `components.connectors` with
  the failures under `connectors.failed_to_load`, and `components.channels`
  with the quarantined set under `channels.quarantined`. Both keep the HTTP
  status at 200 — alert on the fields, not the status code, since a 503 would
  pull the node out of its load balancer over something nothing in flight may
  be using.
- `GET /api/v1/admin/connectors` gives every row a `load_status` (`loaded`,
  `failed`, `disabled`) plus `load_error` and `load_error_stage`.
- Environment overrides for the four settings that had none:
  `ORION_SERVER__COMPRESSION__ENABLED`,
  `ORION_ENGINE__CACHE_CLEANUP_INTERVAL_SECS`,
  `ORION_RATE_LIMIT__ENDPOINTS__ADMIN_RPS` and `…__DATA_RPS`. The two endpoint
  limits are optional, so their variables are three-state: unset keeps the
  config-file value, a number sets it, an empty string clears it.
- **Kafka SASL/TLS authentication** — `[kafka.auth]` (`security_protocol`,
  `sasl_mechanism`, `sasl_username`, `sasl_password`, `ssl_ca_location`) plus a
  `kafka.extra_config` passthrough for arbitrary librdkafka properties, applied
  to both the consumer and the producer. Orion can now connect to Confluent
  Cloud, MSK, Aiven, and any secured broker; previously PLAINTEXT was the only
  reachable configuration. Do not set `enable.auto.commit` via the passthrough —
  it would defeat the at-least-once guarantee below.
- **Message data in every connector function** — `db_read`, `db_write`,
  `cache_read`, `cache_write`, and `mongo_read` now resolve `{"var": "…"}`
  references in their `key`, `value`, `ttl_secs`, `params`, and `filter` inputs,
  so keys, bind parameters, and Mongo filters can depend on the message instead
  of being fixed constants. `data_query`/`data_write` share the same resolver.
  `connector` and raw `query` text stay literal by design.
- **Trace DLQ operator API** — `/api/v1/admin/trace-dlq` with paginated list
  (payload-free projection), get-by-id, requeue, and purge. Failed async traces
  were previously invisible and unreplayable.
- **Audit-log filtering** — `action`, `resource_type`, `resource_id`,
  `principal`, and time-range filters on `GET /api/v1/admin/audit-logs`; unknown
  query parameters are now rejected with 400 rather than silently ignored. The
  `details` column is populated (starting with `request_id`) — it was dead
  before, and writing to it produced malformed SQL.
- **Audit-log retention** — `queue.audit_retention_days` (default 90, `0` keeps
  forever) with a lease-gated cleanup job. The table previously grew forever
  with no supported way to prune it.
- **Operational metrics** — `trace_dlq_depth`, `trace_dlq_retries_total`,
  `trace_queue_rejected_total{reason}`, and `trace_persistence_failures_total`.
  The three conditions most worth alerting on were previously invisible.
- **Rate-limit proxy trust** — `rate_limit.trusted_proxies` (CIDR list, empty by
  default). Forwarded headers are honoured only from listed peers; otherwise the
  TCP peer address is the client identity.
- **Hashed admin keys** — `admin_auth.api_keys` accepts `sha256:<64-hex>`
  entries so keys need not sit in config as plaintext. Plaintext still works.
- **Bounded in-memory cache** — `engine.max_memory_cache_entries` (default
  100 000, `0` = unbounded) with LRU eviction. The dedup store and response
  cache were previously unbounded maps reachable from workflow config alone.
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

- **Channel runtime controls hold (proposal N2, N5).** Responses carrying task
  errors are no longer cached, so a transient downstream failure is not pinned
  for the full TTL and replayed to every caller. A `rate_limit.key_logic` that
  does not compile now quarantines the channel, and an evaluation failure
  rejects the request with `429` — previously both fell back to `client_ip`,
  silently turning a per-tenant limit into a per-IP one.
- **Egress correctness round (proposal F8, F10–F13, F17).** `http_call` no
  longer retries non-idempotent methods by default — a timed-out POST was
  re-sent up to 3× with no idempotency key; set the new HTTP-connector
  `retry_non_idempotent` to restore the old behaviour. Retries are also
  bounded by a deadline instead of running attempts plus backoff past the
  channel timeout. `publish_kafka` now publishes to the brokers its
  connector names rather than always the globally configured cluster.
  `db_read`/`mongo_read` enforce `query.max_limit` as a hard row cap, every
  MongoDB path honours `query_timeout_ms`, Elasticsearch responses respect a
  new `max_response_size` on the ES connector, and evicted connector pools
  are closed instead of leaking their connections on every connector edit
  and cluster epoch resync.
- **Routing and rollout truth (proposal F30, F33, R5, F32).** Channels whose
  workflows cannot be built — missing or unconvertible workflow, or rollout
  percentages that don't sum to 100 — are now quarantined with the reason on
  `/health` instead of silently serving engine errors or blackholing part of
  the traffic. Workflows with unknown functions are rejected at create;
  activation requires every referenced connector to exist. The channel
  include/exclude glob matcher gained real backtracking and boot logs the
  resolved channel list when filters are configured.
- **Queue durability round (proposal Q4–Q8, N15, D4).** A DLQ backoff shift
  overflow no longer kills the retry task (`dlq_max_retries` is now bounded
  1–16); a DB error on the "mark running" write routes the message to the DLQ
  instead of dropping it with the trace stuck `pending`; failed persistence
  writes retry (50ms/250ms) before being counted and dropped, and batch
  buffers are no longer cleared on error before that retry;
  `async_workers`/`batch_workers` > 1 now actually run in parallel (per-worker
  receivers, round-robin fan-out); `tracing.storage.batch_size` is bounded at
  1000 so batch flushes cannot exceed SQLite's bind limit; `task_trace_json`
  is capped by `queue.max_result_size_bytes` on both paths; and trace
  retention reclaims pending/running rows older than twice the retention
  window instead of leaking them forever.

- **Connectors authored the documented way never loaded.** `ConnectorConfig` is
  internally tagged on `type`, but the type lives in its own column and the API
  takes it as a sibling `connector_type` — so a config without a redundant
  `"type"` inside it failed to deserialize and the connector was silently
  skipped. That is the shape every example, the OpenAPI spec and any admin UI
  produce. The stored column is now the single source of truth.
- **A connector `GET` → edit → `PUT` round-trip persisted `"******"` as the
  credential.** Fields returned masked are now restored from the stored row,
  and a mask with no stored counterpart is a 400 naming the field instead of a
  silent credential overwrite.
- **Per-channel rate limits never applied to REST-routed channels.** The
  middleware matched the first path segment against channel names; for a REST
  channel that segment is the route prefix, so the limiter was never found and
  the channel fell through to the platform-wide limit. Channel resolution now
  mirrors the data handler exactly, including the `/async` suffix.
- **One broken channel made the instance unmanageable.** A channel whose stored
  config no longer parses used to fail *every* admin operation that triggers a
  reload — activate, archive, delete, rollout — with a 500, and stopped the
  cluster epoch watcher resyncing all nodes. Such channels are now quarantined
  individually: still refused at every ingress (with a 503 naming the reason,
  and routed to the DLQ on the Kafka path), but the rest of the reload
  succeeds. Boot no longer aborts over one bad row either.
- **Connectors that failed to load vanished without a signal.** Env
  substitution, JSON parsing, secret resolution and deserialization failures
  are now recorded and reported on `/health` and the admin list.
- **`ORION_RATE_LIMIT__ENABLED=true` with no config file failed startup
  validation.** `RateLimitConfig` and `AppConfig` derived `Default` while also
  carrying `#[serde(default = "…")]` attributes with different values, so "the
  default" depended on how the config was produced. Both now implement
  `Default` in terms of their `default_*` functions, and the config-docs drift
  test fails on any future divergence.
- **The async path and Kafka ingest bypassed every per-channel control.**
  CORS, `validation_logic`, deduplication, and backpressure lived only in the
  sync HTTP path, so appending `/async` to a URL defeated a channel's input
  contract, and `channel_call` skipped the target channel's guards entirely.
  All ingress paths now share one guard layer, and an async request holds its
  backpressure permit for the whole of processing, so `max_concurrent` bounds
  sync and async traffic together.
- **The response cache could serve one caller's data to another.** The key
  hashed only the request body, so for a REST channel with a path parameter and
  an empty body (`GET /orders/{id}`) every id collided onto one entry. Method,
  route parameters, and query string are now part of the key.
- **Kafka messages were lost on failure.** Offsets committed unconditionally,
  so with the DLQ disabled (the default) a workflow error, timeout, or unmapped
  topic silently discarded the message. Delivery is now at-least-once: offsets
  advance only on success or a confirmed DLQ write, and UTF-8-decode failures,
  empty payloads, and unmapped topics are dead-lettered instead of dropped.
- **Poison messages retried forever.** A failing async trace re-entered the DLQ
  as a fresh row at `retry_count = 0`, so `dlq_max_retries` could never be
  reached and each cycle inserted another `traces` row. The retry count now
  travels with the message and exhausts as documented.
- **The trace queue blocked instead of shedding.** A full buffer parked the
  request indefinitely; it now returns 503, as the configuration already
  documented.
- **Postgres DLQ retry and exhaustion silently failed.** Clearing a claim lease
  bound a TEXT parameter to a `timestamp` column (Postgres error 42804) and all
  three call sites discarded the error, so in cluster mode on Postgres entries
  never backed off, never exhausted, and were re-claimed forever.
- **SSRF protection was incomplete.** Redirects were followed without
  re-validation (reaching cloud metadata via a 302), the validated DNS result
  was discarded and re-resolved (rebinding), IPv6 private ranges were largely
  unchecked, and the Elasticsearch connector skipped validation entirely.
  Redirects are now followed manually with per-hop validation, connections are
  pinned to validated addresses, and the private-range coverage is complete.
- **Rate limiting was trivially bypassed.** The client identity came from
  unvalidated forwarded headers with no peer-address fallback, so direct
  clients shared one bucket and proxied clients could mint a new identity per
  request. See `rate_limit.trusted_proxies` above.
- **Channels with broken configuration served unguarded.** An unparseable
  `config_json` silently loaded a default (no rate limit, validation, dedup,
  backpressure, timeout, or cache) and an uncompilable `validation_logic` was
  dropped with a warning. Both now refuse to load the channel.
- **`db_read` turned unreadable columns into `null`.** `REAL`/`float4` and blob
  columns silently read back as null on every SQL read path; genuinely
  unsupported types now error rather than looking like a NULL value.
- **Unimplemented secret schemes were used as literal passwords.**
  `vault://…`, `aws-sm://…`, and friends passed through verbatim as the
  credential; they are now rejected at connector load.
- **Credentials embedded in URLs leaked through the admin API.** Masking was a
  flat key-name denylist that missed `url` and `brokers[]`, so
  `redis://:PASSWORD@host` was returned in full. Masking is recursive and
  strips userinfo from URL-shaped values at any depth.
- **A restart of cluster Redis broke every node permanently.** The shared
  connection never re-established, silently disabling distributed dedup,
  response caching, and rate limiting until pods restarted.
- **An open circuit breaker returned 500 `ENGINE_ERROR`** instead of the
  documented 503 `CIRCUIT_OPEN`, so callers could not distinguish shed load
  from a server fault and the DLQ retry classifier never saw it as retryable.
  Timeouts and 503s are now classified retryable.
- **Internal error detail leaked to anonymous callers.** Success bodies
  embedded raw upstream URLs, sqlx errors, and connector names; the data plane
  now returns a code, a generic message, and a request id, with full detail
  kept in the persisted trace.
- **Unbounded Prometheus label cardinality.** Rate-limit rejections were
  labelled with a spoofable client IP, and channel-labelled metrics accepted any
  attacker-supplied path segment.
- **`PUT /channels/{id}` ran no validation at all**, and `PUT /connectors/{id}`
  skipped config validation unless the type was resent. Both now validate
  against the stored record.
- **A CORS list mixing `"*"` with explicit origins passed validation and then
  panicked at router build**, killing the server at boot; `PATCH` was missing
  from the allowed methods, making the admin status and rollout endpoints
  unusable cross-origin.
- **Admin API keys were compared with an early length check**, leaking key
  length by timing.
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

- `queue.dlq_max_retries` is now validated as 1–16 and connector
  `retry.max_retries` as ≤ 16 (both are exponents in a doubling backoff);
  `tracing.storage.batch_size` is capped at 1000 (the batch INSERT binds ~11
  parameters per row against SQLite's 32 766-bind statement limit). Configs
  outside these ranges are rejected at startup instead of failing at runtime.
- Workflow create/update rejects unknown `function.name` values, and workflow
  activation requires every referenced connector to exist. Both were lint
  warnings; the workflow failed at its first request instead.
- Trace retention now also deletes `pending`/`running` rows older than twice
  `queue.trace_retention_hours` — previously they were never reclaimed.
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

[Unreleased]: https://github.com/GoPlasmatic/Orion/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/GoPlasmatic/Orion/compare/v0.3.0...v1.0.0
[0.3.0]: https://github.com/GoPlasmatic/Orion/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/GoPlasmatic/Orion/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/GoPlasmatic/Orion/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/GoPlasmatic/Orion/releases/tag/v0.1.0
