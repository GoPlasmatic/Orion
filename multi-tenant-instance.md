# Orion v1.0.0 — Multi-Tenancy & Multi-Instance (HA) Change Plan

*Derived from a full code audit on 2026-07-26 (branch `v1.0.0`, based on `main@a0fc8e8`). Companion to `saas-gaps.md` §2/§3 — this document turns those gaps into concrete, file-level work items.*

## Goals

1. **Multi-tenant:** one Orion deployment serves many isolated tenants — isolated configuration (channels/workflows/connectors), isolated runtime namespaces (routes, caches, dedup, breakers, pools), isolated data (traces, audit), and per-tenant quotas.
2. **Multi-instance (HA):** N replicas of `orion-server` behind a load balancer sharing one Postgres (+ Redis) behave as a single logical system — config changes propagate to all nodes, background jobs don't double-fire, idempotency and rate limits hold globally, and rolling deploys are zero-downtime.

Backward compatibility is a hard requirement: with `tenancy.enabled = false` and `cluster.enabled = false` (the defaults), Orion must behave exactly as 0.3.x — same API paths, same config, SQLite single binary.

---

## Design decisions (to lock before implementation)

| # | Decision | Rationale |
|---|---|---|
| D1 | **Tenant identity comes from an authenticated data-plane API key**, never from a client-supplied header. `x-tenant-id` in rate-limit docs today is unverified input. Data-plane auth is therefore a *prerequisite* for tenancy, not a parallel track. | An unauthenticated tenant claim is not isolation. |
| D2 | **One shared runtime, tenant-scoped keys** — not one engine per tenant. Channel identity everywhere becomes the compound key `{tenant_id}::{channel_name}` (registry maps, engine workflow stamping, cache/dedup/breaker/pool keys). | Per-tenant engines multiply reload cost and memory; dataflow-rs matches on a string key, so compound keys are the minimal change. |
| D3 | **Implicit `default` tenant** for compatibility. Migrations backfill `tenant_id = 'default'`; single-tenant mode pins all requests to it. No API or URL changes in single-tenant mode. |
| D4 | **Coordination via the shared DB + Redis, no cluster framework.** DB: config epoch, DLQ leases, job leases. Redis: dedup, response cache, distributed rate limiting. No gossip, no leader election library, no node discovery. |
| D5 | **Cluster mode requires Postgres (or MySQL) + Redis.** Startup refuses `cluster.enabled = true` with `sqlite:` storage or with in-memory dedup on any channel. |
| D6 | **Circuit breakers and backpressure stay node-local** (documented ÷N / ×N semantics) in v1.0.0. Sharing breaker state is complexity with marginal benefit; a breaker that trips per-node still converges after `failure_threshold` failures per node. |
| D7 | **Tenant routing on the data plane:** the API key resolves the tenant; the existing path scheme (`/api/v1/data/{route}`) is unchanged. Optional `/t/{tenant}/...` prefix and host-based resolution are v1.1 candidates, not v1.0.0. |

---

## Workstream 0 — Prerequisite correctness fixes (ship first, valuable standalone)

These are pre-existing bugs confirmed in the audit; several silently break the moment a second replica starts.

- [ ] **0.1 Channel-scope the dedup key.** Today the raw idempotency header value is the cache key — tokens collide across channels (and later, tenants). Change `check_deduplication` (`src/server/routes/data.rs:440-458`) to `dedup:{channel}:{token}` (later `dedup:{tenant}:{channel}:{token}`), mirroring the response-cache key format at `data.rs:521`.
- [ ] **0.2 Auth on trace endpoints.** `GET /api/v1/data/traces` and `/traces/{id}` (`data.rs:812,836`) return full payloads unauthenticated — `admin_auth.rs:50` only guards `/api/v1/admin` + `/metrics`. Move them under admin auth now; tenant-scope them in Workstream C.
- [ ] **0.3 Dedup backend errors fail closed as 409.** `store.check_and_insert(key, window).await.unwrap_or(false)` (`data.rs:450`) treats a Redis outage as "duplicate" and rejects every request with Conflict. Decide policy (recommend: fail-open + error metric + warn) and implement.
- [ ] **0.4 Wire `queue.dlq_max_retries`.** The config value (`src/config/queue.rs:30`) is only logged (`main.rs:585`); the enqueue path hardcodes `5` (`src/queue/processing.rs:500`).
- [ ] **0.5 Write `traces.channel_id`.** The column exists but is never populated by any repo path (`traces.rs:158-164`, `:284-295`). Attribution and later tenant filtering need it.
- [ ] **0.6 Remove dead code `src/channel/dedup.rs`** (`DeduplicationStore` — only reference is its own re-export in `channel/mod.rs:17`).
- [ ] **0.7 `BackpressureConfig.queue_depth`** (`src/channel/config.rs:114-121`) is parsed but never read — implement or delete the field.
- [ ] **0.8 Trigger/constraint parity across backends.** Active-immutability triggers exist only on SQLite (`migrations/sqlite/001:136-168`); Postgres and MySQL have none. Either add them (PG plpgsql, MySQL SIGNAL) or enforce immutability in the repository layer for all three — required before tenancy migrations fork the schemas further.

---

## Workstream A — Tenant data model (schema + repositories)

- [ ] **A1. `tenants` table** (new migration `004_tenancy.sql` × 3 backends): `id` (slug PK), `name`, `status` (`active`/`suspended`), `quotas_json`, `created_at`, `updated_at`. Seed row `'default'`.
- [ ] **A2. `tenant_id` column on all 6 tables** (`workflows`, `channels`, `connectors`, `traces`, `trace_dlq`, `audit_logs`), `NOT NULL DEFAULT 'default'`, FK to `tenants`:
  - PKs become `(tenant_id, workflow_id, version)` / `(tenant_id, channel_id, version)`.
  - `connectors.name` global `UNIQUE` → `UNIQUE(tenant_id, name)`.
  - Views `current_workflows`/`current_channels`: add `tenant_id` to the `GROUP BY` and join predicates (`migrations/*/001:64-81`).
  - Single-draft enforcement: SQLite/MySQL triggers get `AND tenant_id = NEW.tenant_id`; Postgres partial unique indexes become `(tenant_id, workflow_id) WHERE status='draft'` (`migrations/postgres/001:113-117`).
  - All 22 indexes gain a leading or added `tenant_id` where they serve tenant-scoped queries (`idx_channels_name` → `(tenant_id, name)`, `idx_traces_channel` → `(tenant_id, channel)`, etc.).
- [ ] **A3. `src/storage/migration_gen.rs`** is the generator/source of truth — update `workflows_table()`, `channels_table()`, `connectors_table()`, `traces_table()`, `trace_dlq_table()`, `views_sql()`, all three trigger generators, and the index builders. Note: `audit_logs` is hand-written in the `.sql` files and missing from the generator's table list (`migration_gen.rs:669-675`) — bring it into the generator as part of this work.
- [ ] **A4. Type plumbing:** add `TenantId` to all 6 `Iden` enums in `src/storage/schema.rs`; add the field to every row model in `src/storage/models.rs` (`Workflow`, `Channel`, `Connector`, `Trace`, `TraceDlqEntry`, `AuditLogEntry`) and their `Response` DTOs.
- [ ] **A5. Repository surface:** every trait method that filters by id/name/status gains a `tenant: &TenantId` parameter — `ChannelRepository` (13 methods, `channels.rs:81-118`), `WorkflowRepository` (16, `workflows.rs:86-134`), `ConnectorRepository` (7, `connectors.rs:43-55`), `TraceRepository` (`traces.rs:51-128`), `TraceDlqRepository`, `AuditLogRepository`. All queries are sea-query builders, so this is mechanical but wide (~60 query sites). `list_active()` on channels/workflows stays global (the engine loads all tenants) but must return `tenant_id` for compound-key construction.
- [ ] **A6. `tenants` repository + admin CRUD** (`/api/v1/admin/tenants`, platform-admin only): create/suspend/delete tenant, set quotas. Suspension = channels excluded at engine build + 403 on data plane.
- [ ] **A7. Backfill/compat:** migration sets `tenant_id='default'` on existing rows; `orion-server migrate` handles it like any other migration.

---

## Workstream B — Tenant identity & auth

- [ ] **B1. `api_keys` table** (migration `005_api_keys.sql`): `id`, `tenant_id`, `key_hash` (argon2id), `key_prefix` (first 8 chars, for display/lookup), `scopes` (`data`, `admin`, `read_only`, optional channel allowlist), `expires_at`, `last_used_at`, `revoked_at`, `created_at`. Add `argon2` to `Cargo.toml`.
- [ ] **B2. Data-plane auth middleware** (new, before channel dispatch in the router stack, `src/server/mod.rs:53-179`): extract bearer/`x-api-key`, hash-lookup by prefix, verify, attach `RequestIdentity { tenant_id, key_id, scopes }` to request extensions. Gated by `tenancy.require_data_auth` (default `false` for compat; forced `true` when `tenancy.enabled`). This replaces the "identity slot" gap at `data.rs:209-223`.
- [ ] **B3. Admin auth v2:** admin keys move into `api_keys` with `admin` scope; static config keys remain as *platform-operator* bootstrap keys. `AdminPrincipal` (`admin_auth.rs:18-29`) grows `tenant_id` + `key_id`; tenant-scoped admin keys see only their tenant's resources in every admin route.
- [ ] **B4. Key lifecycle API:** `POST/GET/DELETE /api/v1/admin/api-keys` (+ rotate), plaintext shown once on create; `last_used_at` updated at most once per minute per key (avoid hot-path writes).
- [ ] **B5. Audit attribution:** `audit_logs` gains `tenant_id` (A2) and the principal becomes `key_id`/user, plus client IP and `x-request-id` (available via `src/server/request_context.rs`). Fix the fire-and-forget insert (`admin/mod.rs:77-84`) to at-least-once (retry or queue through the persistence worker).

---

## Workstream C — Tenant-aware runtime (namespacing every in-memory structure)

- [ ] **C1. `ChannelRegistry`** (`src/channel/registry.rs:105-108`): `by_name: HashMap<String, …>` keyed by `channel.name` → keyed by `(TenantId, String)` (or the `{tenant}::{name}` compound string). `get_by_name` gains a tenant argument; all callers updated (`data.rs:638`, `rate_limit.rs`, kafka consumer).
- [ ] **C2. `RouteTable`** (`src/channel/routing.rs:89-149`): one flat vector today; becomes per-tenant tables (`HashMap<TenantId, RouteTable>`). `match_route(tenant, method, path)`. Route resolution in `dynamic_handler` (`data.rs:196-223`) runs *after* B2 resolves the tenant.
- [ ] **C3. Engine channel key:** `build_engine_workflows` (`src/engine/mod.rs:280-354`) and `workflow_to_dataflow[_with_rollout]` (`workflows.rs:875,903`) stamp `channel.name` into dataflow workflows — stamp `{tenant}::{name}` instead. Dispatch sites updated to pass the compound key: sync (`data.rs:100,116,125`), Kafka (`consumer.rs:237` — topic→channel map becomes topic→`(tenant, channel)`), `channel_call` (`src/engine/functions/channel_call.rs:55-70` — **must resolve within the caller's tenant only**; carry tenant in message metadata alongside `_orion_call_depth`).
- [ ] **C4. `ConnectorRegistry`** (`src/connector/registry.rs:38-42`): `configs` keyed by `(tenant, name)`; `load_from_repo` (`registry.rs:129-205`) carries tenant. Circuit-breaker keys become `{tenant}:{channel}:{connector}` (producer at `src/engine/functions/http_call.rs:60`). Function-side connector resolution (`connector_helpers.rs:92-96`) resolves within the caller's tenant (tenant from message metadata).
- [ ] **C5. Pool caches:** `SqlPoolCache`/`MongoPoolCache`/`RedisPoolCache` keys are bare connector names (`pool_cache.rs:42`, `mongo_pool.rs:26`, `redis_pool.rs:34`) → `{tenant}:{connector}`. Same for `CachePool::get_backend`/`evict_pool` (`cache_backend.rs:235-260`) and the eviction calls in `admin/connectors.rs:33-41`.
- [ ] **C6. Cache & dedup keys:** response cache `cache:{channel}:{hash}` (`data.rs:521`) → `cache:{tenant}:{channel}:{hash}`; dedup key from 0.1 gains the tenant segment. `cache_read`/`cache_write` (`engine/functions/cache_read.rs:33`, `cache_write.rs:33`) pass user keys verbatim today — prefix with `t:{tenant}:` transparently so tenants can never read each other's cache entries even on a shared Redis.
- [ ] **C7. Traces & attribution:** `create_pending`/`store_completed` write `tenant_id` (+ `channel_id` from 0.5); trace list/get endpoints filter by the caller's tenant. Kafka and DLQ paths carry tenant through `QueueMessage` (`src/queue/mod.rs:68-81`).
- [ ] **C8. Per-tenant quotas** (from `tenants.quotas_json`, enforced in existing choke points):
  - Rate limit: per-tenant RPS bucket keyed on verified tenant (extend `rate_limit.rs` middleware; replaces trust in `x-tenant-id`).
  - Async queue memory: per-tenant byte accounting inside `TraceQueue::submit` (`queue/mod.rs:113-121`) — reject a tenant at its share before the global 100 MB cap.
  - Payload size: per-tenant `max_payload_size` override checked in the data handler (global axum body limit stays as ceiling).
  - Resource counts: max channels/workflows/connectors per tenant, enforced in create paths.
- [ ] **C9. Channel include/exclude globs** (`filter_channels`, `engine/mod.rs:222`) operate on compound keys — pattern syntax gains an optional `tenant/` prefix.
- [ ] **C10. Metrics cardinality:** do **not** add a `tenant` label to hot-path Prometheus metrics; per-tenant usage lives in the usage ledger (F2). Add `tenant` only to low-cardinality admin metrics (`admin_audit_total`).

---

## Workstream D — Multi-instance coordination (HA)

- [ ] **D1. Instance identity:** `instance_id` = UUID generated at boot (overridable `ORION_SERVER__INSTANCE_ID`), stored in `AppState`, used by DLQ leases, job leases, and log/metric decoration.
- [ ] **D2. Config-change propagation (the highest-leverage HA fix).** New `config_epoch` table (single row: `epoch BIGINT`, `updated_at`). Every mutation that today calls `audit_and_reload` (`admin/mod.rs:106-121` — status changes, deletes, rollout updates, manual reload) **and** every connector mutation (`connectors.rs:94,145,177,272`) increments the epoch in the same transaction as the write. Each node runs an epoch-watcher task: poll every `cluster.epoch_poll_interval_ms` (default 2000 ms; Postgres upgrade path: `LISTEN/NOTIFY` with poll as fallback) → on change, run `reload_engine` + `reload_connectors` + pool eviction. This also fixes the existing gap where connector edits never propagate (`reload_connectors` at `connectors.rs:25-31` is node-local and separate from engine reload).
- [ ] **D3. DLQ claim semantics.** `list_pending` (`trace_dlq.rs:116-139`) is an unguarded global scan every 30 s on every node. Add lease columns (`claimed_by`, `claimed_until`) and claim via `UPDATE … SET claimed_by = $instance, claimed_until = now()+interval WHERE id IN (SELECT … FOR UPDATE SKIP LOCKED LIMIT 20)` on PG/MySQL; SQLite keeps the simple path (single-node by definition, per D5). Expired leases are re-claimable. Also remove the hardcoded batch size 20 (`dlq_retry.rs:29`) → config.
- [ ] **D4. Distributed rate limiting.** governor's `DashMapStateStore` (`src/channel/mod.rs:22-30`) is per-process; N replicas ⇒ N× the configured limit, and per-channel limiter state resets on every engine reload (`registry.rs:150-153`). Introduce a `RateLimitBackend` trait: `local` (governor, default) and `redis` (fixed-window or GCRA via Lua `INCR`/`EXPIRE` — governor has no Redis store). Cluster mode defaults per-channel and per-tenant limits to the Redis backend; platform IP limits may stay local (documented ×N).
- [ ] **D5. Cluster-mode startup guardrails** (in `src/config/validation.rs`): `cluster.enabled = true` requires (a) non-SQLite storage, (b) `cluster.redis_url` set, (c) hard error if any *active* channel with `deduplication` configured would fall back to the in-memory backend. Also change the silent memory fallbacks in `ChannelRegistry::reload` (`registry.rs:198-233`, `:257-281`) from `warn` to channel-load *error* in cluster mode — silent per-node dedup is a correctness loss, not a degradation.
- [ ] **D6. Shared dedup/response cache by default in cluster mode:** new `[cluster] redis_url` provisions a default Redis-backed `CacheBackend` used whenever a channel doesn't name an explicit cache connector (replacing the `cache_pool.memory()` fallback). The FNV-1a cache key (`data.rs:492-522`) is already cross-process stable — keep it.
- [ ] **D7. Background-job single-flight.** Trace cleanup (`queue/mod.rs:24-65`) runs on every node (N× duplicate DELETEs). Add a `job_leases` table (`job_name PK, holder, expires_at`); cleanup and DLQ retry acquire the lease before each tick (skip if held). Cheap, no leader election.
- [ ] **D8. Kafka static membership + restart hygiene.** Set `group.instance.id = instance_id` and explicit `session.timeout.ms` in `start_consumer` (`consumer.rs:97-102`) so rolling deploys and reload-driven restarts rejoin without a full group rebalance. Epoch-driven reloads (D2) will fire on all nodes near-simultaneously — add per-node jitter (0–5 s) before `restart_kafka_consumer_if_needed` (`routes/mod.rs:231-324`) when the topic set changed.
- [ ] **D9. Rolling-deploy readiness.** `ready` is set true once (`main.rs:519`) and never cleared; `/readyz` (`routes/mod.rs:123-152`) returns 200 through the entire drain window, and the plain-HTTP path *keeps accepting new connections* for `shutdown_drain_secs` (`main.rs:750-771`). Fix: on SIGTERM set `ready = false` immediately (LB pulls the node), keep serving in-flight + newly-arrived requests during drain, and unify TLS/plain drain semantics (they differ today: `main.rs:656-673` vs `:750-771`).
- [ ] **D10. Migration/deploy separation.** Add `storage.auto_migrate` (default `true` for compat; `false` in cluster mode) so replicas don't race migrations at boot (sqlx has no SQLite lock, and PG advisory locks still serialize a thundering herd). Document expand/contract convention for all future migrations; `orion-server migrate` becomes the Helm pre-upgrade hook / init job.
- [ ] **D11. Circuit breakers & backpressure: document, don't distribute (D6 decision).** Docs state per-node semantics explicitly (`max_concurrent` × N, breaker trips per node). `POST /circuit-breakers/{key}` reset is node-local and 404s elsewhere (`admin/connectors.rs:314-333`) — piggyback breaker resets on the epoch bus (a `breaker_reset` epoch event) so one API call resets all nodes.

---

## Workstream E — Deploy artifacts & ops

- [ ] **E1. Helm chart** (`deploy/helm/orion/`): Deployment (with `readinessProbe: /readyz`, `livenessProbe: /healthz`), HPA, PDB, Service, Ingress, ConfigMap/Secret for `ORION_*` env, optional Redis + Postgres subcharts for dev, pre-upgrade migrate Job (D10). Nothing exists today beyond Dockerfile + single-node compose.
- [ ] **E2. Fill env-override gaps** (`src/config/env_overrides.rs` is a hand-maintained list): missing today and needed for pure-env K8s deploys — `ORION_QUEUE__DLQ_RETRY_ENABLED`, `ORION_QUEUE__DLQ_POLL_INTERVAL_SECS`, `ORION_QUEUE__DLQ_MAX_RETRIES`, `ORION_STORAGE__{MAX_CONNECTIONS,MIN_CONNECTIONS,IDLE_TIMEOUT_SECS}`, `ORION_CORS__ALLOWED_ORIGINS`, `ORION_KAFKA__TOPICS`, `ORION_KAFKA__DLQ__{ENABLED,TOPIC}`, `ORION_CHANNELS__{INCLUDE,EXCLUDE}`, plus all new `[tenancy]`/`[cluster]` keys.
- [ ] **E3. Backups:** `POST /api/v1/admin/backups` writes to the local node's filesystem (`admin/backups.rs:34,69,108`) — meaningless behind an LB. In cluster mode return 400 pointing at managed-DB PITR; add per-tenant export (`GET /api/v1/admin/tenants/{id}/export` — channels/workflows/connectors as an import-compatible bundle).
- [ ] **E4. HA reference compose file** (`docker-compose.ha.yml`): 2× orion + Postgres + Redis + nginx, used by the multi-node integration tests (see Test plan) and as user documentation.
- [ ] **E5. Docs:** rewrite `docs/src/features/scalability.md` and `availability.md` around cluster mode (they currently document the curl-loop reload fan-out as the workaround).

---

## Workstream F — Tenant observability & metering (minimum for v1.0.0)

- [ ] **F1. Usage ledger:** per-request usage event (tenant, channel, ts, duration_ms, bytes_in/out, status) written through the existing trace-persistence batch worker into a `usage_events` table; hourly rollup job (behind a D7 lease). This is the billing substrate — export integration itself is out of scope.
- [ ] **F2. Tenant-scoped read APIs:** traces, audit logs, usage summaries filtered by the caller's tenant; platform operator sees all. Audit list API gains filters (action/resource/principal/time range) — today it has none (`admin/audit.rs:34-52`).
- [ ] **F3. Sticky rollout bucketing:** `inject_rollout_bucket` uses `rand::random` per request (`engine/utils.rs:19-26`) — canary assignment flip-flops per call. Hash `(tenant, api_key or client ip)` → stable bucket.
- [ ] **F4. Per-instance labels:** add `instance_id` to log spans; `/metrics` stays per-node (Prometheus scrapes each pod), no aggregation needed.

---

## New configuration (sketch)

```toml
[tenancy]
enabled = false               # false = exactly today's behavior (implicit 'default' tenant)
require_data_auth = false     # forced true when enabled = true

[cluster]
enabled = false               # true = multi-replica coordination on
redis_url = ""                # required when enabled; shared dedup/cache/rate-limit
epoch_poll_interval_ms = 2000
instance_id = ""              # auto-generated UUID when empty

[storage]
auto_migrate = true           # set false in cluster deployments; run `orion-server migrate` as a job
```

---

## Test plan

- **Multi-node harness:** extend `tests/common` to build *two* `AppState`s sharing one Postgres testcontainer + one Redis testcontainer (the existing testcontainers setup in `common/backends.rs` covers PG/Redis already). Assert:
  - activate a channel via node A → node B serves it within one epoch poll (D2);
  - same idempotency key sent to A then B → second gets 409 (D6);
  - a DLQ row is retried by exactly one node (D3);
  - per-tenant rate limit of 10 rps holds at ~10 rps across both nodes (D4).
- **Tenant isolation suite:** two tenants with identical channel names/routes/connector names; assert no cross-tenant route match, cache hit, dedup suppression, connector resolution, `channel_call`, or trace/audit visibility.
- **Compat suite:** full existing integration suite (16.5k lines) must pass untouched with `tenancy.enabled=false` — that is the definition of D3's success.
- **Migration test:** 0.3.x SQLite DB fixture → run `004+` migrations → all rows land in tenant `default`, all views/triggers behave.
- **Unit-test fallout to budget for:** `make_channel` fixture (`engine/mod.rs:392-416`), registry/routing/rate-limit in-file tests, `migration_gen` golden tests (`test_all_backends_have_views` etc.).

---

## Sequencing

| Milestone | Contents | Exit criteria |
|---|---|---|
| **M1 — Hardening** | Workstream 0 (all items) | Existing suite green; dedup channel-scoped; traces authed; DLQ config honored |
| **M2 — Tenant foundation** | A1–A7, B5 (schema part) | Migrations on 3 backends; all repos tenant-parameterized; `default` tenant end-to-end; compat suite green |
| **M3 — Identity** | B1–B4 | Data-plane auth on; key lifecycle API; tenant resolved from key |
| **M4 — Tenant runtime** | C1–C10 | Tenant isolation suite green |
| **M5 — HA coordination** | D1–D11 | Multi-node harness green; rolling deploy with zero 5xx demonstrated |
| **M6 — Ops & metering** | E1–E5, F1–F4 | Helm install of 3-replica cluster passes the harness scenarios |

M2/M3 and M5 are largely independent — HA coordination (M5) can proceed in parallel with tenancy (M2–M4) since D2/D3/D7/D9 don't depend on the tenant model.

## Explicit non-goals for v1.0.0

- OIDC/SSO and a user/org model on the console (admin API keys only).
- Shared/distributed circuit-breaker state (D6 decision).
- Host-based or path-prefix tenant routing (D7 — key-based only).
- Billing-provider integration (ledger only), envelope encryption of connector secrets, Vault resolver — tracked in `saas-gaps.md` §4/§5 for the SaaS phase.
