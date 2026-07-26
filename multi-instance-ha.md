# Orion v1.0.0 — Multi-Instance (HA) Change Plan

*Derived from a full code audit on 2026-07-26 (branch `v1.0.0`, based on `main@a0fc8e8`). This is the v1.0.0 planning document: HA work items (Workstreams 0/A/B/C) plus the security & ops hardening backlog (Workstream H) carried over from the retired `saas-gaps.md` (deleted; full analysis preserved in git history at `947401e`/`f52e7ed`).*

> **Scope note:** Orion is **single-tenant and self-hosted — by product decision (2026-07-26)**. There is no SaaS and no multi-tenancy, permanently. The cloud story is **cloud-marketplace images** (AWS/Azure/GCP) that users run in their own accounts. "Multi-instance" means N identical replicas of `orion-server` serving *one* deployment behind a load balancer — HA for a single installation, nothing more.

## Goal

N replicas of `orion-server` behind a load balancer, sharing one Postgres (+ Redis), behave as a single logical system:

- config changes made through any node propagate to all nodes,
- background jobs (DLQ retry, trace cleanup) don't double-fire,
- idempotency, response caching, and rate limits hold globally,
- rolling deploys are zero-downtime.

Backward compatibility is a hard requirement: with `cluster.enabled = false` (the default), Orion must behave exactly as 0.3.x — same API, same config, SQLite single binary.

---

## Design decisions (to lock before implementation)

| # | Decision | Rationale |
|---|---|---|
| D1 | **Coordination via the shared DB + Redis, no cluster framework.** DB: config epoch, DLQ leases, job leases. Redis: dedup, response cache, distributed rate limiting. No gossip, no leader-election library, no node discovery. | Orion's source of truth is already the shared DB; leases and epochs cover every coordination need here. |
| D2 | **Cluster mode requires Postgres (or MySQL) + Redis.** Startup refuses `cluster.enabled = true` with `sqlite:` storage or with in-memory dedup on any active channel. | SQLite is single-host by construction; in-memory dedup across replicas is silent correctness loss. |
| D3 | **Circuit breakers and backpressure stay node-local** in v1.0.0, with documented ×N semantics. | Sharing breaker state is complexity with marginal benefit; a per-node breaker still converges after `failure_threshold` failures per node. |
| D4 | **Node identity is ephemeral** — a boot-time UUID, not a registered cluster member. Nothing depends on a stable node list. | Instances are cattle; leases expire, epochs are absolute. |

---

## Workstream 0 — Prerequisite correctness fixes (ship first, valuable standalone)

Pre-existing bugs confirmed in the audit; several silently break the moment a second replica starts.

- [x] **0.1 Channel-scope the dedup key.** Today the raw idempotency header value is the cache key — tokens collide across channels. Change `check_deduplication` (`src/server/routes/data.rs:440-458`) to `dedup:{channel}:{token}`, mirroring the response-cache key format at `data.rs:521`.
- [x] **0.2 Auth on trace endpoints.** `GET /api/v1/data/traces` and `/traces/{id}` (`data.rs:812,836`) return full payloads unauthenticated — `admin_auth.rs:50` only guards `/api/v1/admin` + `/metrics`. Move them under admin auth.
- [x] **0.3 Dedup backend errors fail closed as 409.** `store.check_and_insert(key, window).await.unwrap_or(false)` (`data.rs:450`) treats a Redis outage as "duplicate" and rejects every request with Conflict. Decide policy (recommend: fail-open + error metric + warn) and implement.
- [x] **0.4 Wire `queue.dlq_max_retries`.** The config value (`src/config/queue.rs:30`) is only logged (`main.rs:585`); the enqueue path hardcodes `5` (`src/queue/processing.rs:500`).
- [x] **0.5 Write `traces.channel_id`.** The column exists (and is indexed) but is never populated by any repo path (`traces.rs:158-164`, `:284-295`).
- [x] **0.6 Remove dead code `src/channel/dedup.rs`** (`DeduplicationStore` — only reference is its own re-export in `channel/mod.rs:17`).
- [x] **0.7 `BackpressureConfig.queue_depth`** (`src/channel/config.rs:114-121`) is parsed but never read — implement or delete the field.
- [x] **0.8 Trigger/constraint parity across backends.** Active-immutability triggers exist only on SQLite (`migrations/sqlite/001:136-168`); Postgres and MySQL have none. Either add them (PG plpgsql, MySQL SIGNAL) or enforce immutability in the repository layer — the three backends must enforce identical invariants, and cluster mode makes Postgres the primary backend.


> **M1 implementation notes (2026-07-26):** all eight items landed on `v1.0.0`. Two findings beyond the audit, both fixed under 0.8: (a) `migrations/mysql/001` could never execute through sqlx (DELIMITER directives, TEXT defaults, TEXT keys, TIMESTAMP↔NaiveDateTime) — rewritten with the VARCHAR/datetime idiom, checksum-safe since it never applied anywhere; (b) Postgres was unusable at runtime (models decode i64, columns were INT4) — fixed by `postgres/004_bigint_columns.sql`. New `tests/storage_postgres.rs` / `tests/storage_mysql.rs` binaries run Orion's own storage on real containers in CI. 0.3 chose fail-open + `dedup_backend` error metric + warn. 0.7 deleted the field. Async-path dedup (none exists today) deferred to A6.

---

## Workstream A — Cluster coordination

- [x] **A1. Instance identity.** `instance_id` = UUID generated at boot (overridable via `ORION_SERVER__INSTANCE_ID`), stored in `AppState`, used by DLQ leases, job leases, Kafka static membership, and log/metric decoration.
- [x] **A2. Config-change propagation (the highest-leverage HA fix).** New `config_epoch` table (single row: `epoch BIGINT`, `updated_at`). Every mutation that today calls `audit_and_reload` (`admin/mod.rs:106-121` — status changes, deletes, rollout updates, manual reload) **and** every connector mutation (`connectors.rs:94,145,177,272`) increments the epoch in the same transaction as the write. Each node runs an epoch-watcher task: poll every `cluster.epoch_poll_interval_ms` (default 2000 ms; Postgres upgrade path: `LISTEN/NOTIFY` with poll as fallback) → on change, run `reload_engine` + `reload_connectors` + pool eviction. This also fixes the existing gap where connector edits never propagate (`reload_connectors` at `connectors.rs:25-31` is node-local and separate from engine reload).
- [x] **A3. DLQ claim semantics.** `list_pending` (`trace_dlq.rs:116-139`) is an unguarded global scan every 30 s on every node. Add lease columns (`claimed_by`, `claimed_until`) and claim via `UPDATE … SET claimed_by = $instance, claimed_until = now()+interval WHERE id IN (SELECT … FOR UPDATE SKIP LOCKED LIMIT n)` on PG/MySQL; SQLite keeps the simple path (single-node by definition, per D2). Expired leases are re-claimable. Also lift the hardcoded batch size 20 (`dlq_retry.rs:29`) into config.
- [x] **A4. Distributed rate limiting.** governor's `DashMapStateStore` (`src/channel/mod.rs:22-30`) is per-process; N replicas ⇒ N× the configured limit, and per-channel limiter state resets on every engine reload (`registry.rs:150-153`). Introduce a `RateLimitBackend` trait: `local` (governor, default) and `redis` (fixed-window or GCRA via Lua `INCR`/`EXPIRE` — governor has no Redis store). Cluster mode defaults per-channel limits to the Redis backend; platform IP limits may stay local (documented ×N).
- [x] **A5. Cluster-mode startup guardrails** (in `src/config/validation.rs`): `cluster.enabled = true` requires (a) non-SQLite storage, (b) `cluster.redis_url` set, (c) hard error if any *active* channel with `deduplication` configured would fall back to the in-memory backend. Also change the silent memory fallbacks in `ChannelRegistry::reload` (`registry.rs:198-233`, `:257-281`) from `warn` to channel-load *error* in cluster mode — silent per-node dedup is a correctness loss, not a degradation.
- [x] **A6. Shared dedup/response cache by default in cluster mode:** `[cluster] redis_url` provisions a default Redis-backed `CacheBackend` used whenever a channel doesn't name an explicit cache connector (replacing the `cache_pool.memory()` fallback). The FNV-1a cache key (`data.rs:492-522`) is already cross-process stable — keep it.
- [x] **A7. Background-job single-flight.** Trace cleanup (`queue/mod.rs:24-65`) runs on every node (N× duplicate DELETEs). Add a `job_leases` table (`job_name PK, holder, expires_at`); cleanup and DLQ retry acquire the lease before each tick (skip if held). Cheap, no leader election.
- [x] **A8. Kafka static membership + restart hygiene.** Set `group.instance.id = instance_id` and explicit `session.timeout.ms` in `start_consumer` (`consumer.rs:97-102`) so rolling deploys and reload-driven restarts rejoin without a full group rebalance. Epoch-driven reloads (A2) will fire on all nodes near-simultaneously — add per-node jitter (0–5 s) before `restart_kafka_consumer_if_needed` (`routes/mod.rs:231-324`) when the topic set changed.
- [x] **A9. Rolling-deploy readiness.** `ready` is set true once (`main.rs:519`) and never cleared; `/readyz` (`routes/mod.rs:123-152`) returns 200 through the entire drain window, and the plain-HTTP path *keeps accepting new connections* for `shutdown_drain_secs` (`main.rs:750-771`). Fix: on SIGTERM set `ready = false` immediately (LB pulls the node), keep serving in-flight + newly-arrived requests during drain, and unify TLS/plain drain semantics (they differ today: `main.rs:656-673` vs `:750-771`).
- [x] **A10. Migration/deploy separation.** Add `storage.auto_migrate` (default `true` for compat; `false` in cluster mode) so replicas don't race migrations at boot (sqlx has no SQLite migration lock, and PG advisory locks still serialize a thundering herd). Document expand/contract convention for all future migrations; `orion-server migrate` becomes the Helm pre-upgrade hook / init job.
- [x] **A11. Circuit breakers & backpressure: document, don't distribute (D3 decision).** Docs state per-node semantics explicitly (`max_concurrent` × N, breaker trips per node). `POST /circuit-breakers/{key}` reset is node-local and 404s elsewhere (`admin/connectors.rs:314-333`) — piggyback breaker resets on the epoch bus (a `breaker_reset` epoch event) so one API call resets all nodes.


> **M2 implementation notes (2026-07-26):** all eleven items landed on `v1.0.0`. Deviations from the sketch: the epoch bump is a separate statement after the successful write + inline reload (full tx plumbing through ~15 repo methods was judged higher-risk than the tiny post-commit crash window, which self-heals on the next mutation or manual reload); `instance_id` lives under `[cluster]` (`ORION_CLUSTER__INSTANCE_ID`), matching the config sketch rather than the A1 prose; breaker resets ride `breaker_epoch`/`breaker_key` columns on the `config_epoch` row (two resets of different keys inside one poll window coalesce to the last — acceptable for a human-driven op). A4 uses a Redis fixed window (INCR+EXPIRE per-second keys, no Lua), limit = rps + burst. The multi-node harness is a separate `tests/cluster` binary (the storage backend is pinned per process via a OnceLock); all four test-plan assertions pass against real Postgres + Redis containers, and the A9 drain drill runs in the integration binary over real sockets. Implementing A3/A4 surfaced and fixed two more pre-existing backend bugs: Postgres rejected the repos' string-bound timestamps (42804), and Postgres INT4 columns could not decode into the models' i64 fields — PG storage had never worked at runtime.

---

## Workstream B — Deploy artifacts & ops

- [x] **B1. Helm chart** (`deploy/helm/orion/`): Deployment (with `readinessProbe: /readyz`, `livenessProbe: /healthz`), HPA, PDB, Service, Ingress, ConfigMap/Secret for `ORION_*` env, optional Redis + Postgres subcharts for dev, pre-upgrade migrate Job (A10). Nothing exists today beyond Dockerfile + single-node compose.
- [x] **B2. Fill env-override gaps** (`src/config/env_overrides.rs` is a hand-maintained list): missing today and needed for pure-env K8s deploys — `ORION_QUEUE__DLQ_RETRY_ENABLED`, `ORION_QUEUE__DLQ_POLL_INTERVAL_SECS`, `ORION_QUEUE__DLQ_MAX_RETRIES`, `ORION_STORAGE__{MAX_CONNECTIONS,MIN_CONNECTIONS,IDLE_TIMEOUT_SECS}`, `ORION_CORS__ALLOWED_ORIGINS`, `ORION_KAFKA__TOPICS`, `ORION_KAFKA__DLQ__{ENABLED,TOPIC}`, `ORION_CHANNELS__{INCLUDE,EXCLUDE}`, plus all new `[cluster]` keys.
- [x] **B3. Backups in cluster mode.** `POST /api/v1/admin/backups` writes to the local node's filesystem (`admin/backups.rs:34,69,108`) — meaningless behind an LB, and SQLite-only anyway. In cluster mode return 400 with guidance to use managed-DB PITR/snapshots; document the operator runbook.
- [x] **B4. HA reference compose file** (`docker-compose.ha.yml`): 2× orion + Postgres + Redis + nginx, used by the multi-node integration tests (see Test plan) and as user documentation.
- [x] **B5. Docs:** rewrite `docs/src/features/scalability.md` and `availability.md` around cluster mode (they currently document the curl-loop reload fan-out as the workaround).
- [ ] **B6. Cloud-marketplace packaging** (the cloud-native distribution path): AWS AMI / Azure VM image / GCP image + container-marketplace listings. Per-cloud glue: cloud-init to wire a managed Postgres (RDS / Cloud SQL / Azure DB) and managed Redis, Terraform + CloudFormation reference templates, hardened base image, TLS bootstrap. Depends on H3 (cloud secrets-manager resolvers) so instances never store credentials in plaintext config, and on H1 (data-plane auth) before any image defaults to a public endpoint.

---

## Workstream C — Cluster observability

- [x] **C1. Sticky rollout bucketing.** `inject_rollout_bucket` uses `rand::random` per request (`engine/utils.rs:19-26`) — canary assignment flip-flops per call *and* per node. Hash a stable caller identity (client IP, or a configurable header) → stable bucket across requests and replicas.
- [x] **C2. Per-instance labels.** Add `instance_id` to log spans; `/metrics` stays per-node (Prometheus scrapes each pod), no aggregation needed.


> **M3 implementation notes (2026-07-26):** B1–B5 and C1–C2 landed on `v1.0.0`. B1: `deploy/helm/orion` validated on a kind cluster — a 3-replica install passed the harness scenarios (cross-pod epoch propagation, shared-Redis dedup 409 across two pods, backups 400); the install caught a Service-selector bug (devStack pods matched the orion Service) fixed via `app.kubernetes.io/component=server`. B4: `docker-compose.ha.yml` + `deploy/ha/rolling-drill.sh` — drill run against a locally built image: 2372 requests through nginx during a SIGTERM roll, zero non-2xx (the drill surfaced that `proxy_connect_timeout` must sit well below client timeouts, since a vanished container IP fails by timeout, not RST). C1: bucket = FNV-1a of `engine.rollout_sticky_header` value, else forwarded client IP, random fallback; applied on sync and async paths. C2: `service.instance.id` OTel resource attribute + `instance_id` on request spans in cluster mode. B3: 400 + managed-DB PITR guidance; runbook in scalability.md. A10's expand/contract convention documented in CONTRIBUTING.md and availability.md.

---

## Workstream H — Security & ops hardening backlog (carried over from `saas-gaps.md`)

Not HA work, but the items from the retired SaaS analysis that remain valuable for a single-tenant, self-hosted product — especially once marketplace images put instances on cloud networks. Unscheduled backlog except where B6 depends on them.

- [ ] **H1. Data-plane authentication (optional, off by default).** `/api/v1/data/*` is completely unauthenticated today — anyone who can reach the endpoint can invoke any channel (`admin_auth.rs:50` guards only `/api/v1/admin` + `/metrics`; `ChannelConfig` has no `auth` field; per-channel "auth" is only hand-rolled JSONLogic over raw headers, `data.rs:270-279`). Add per-channel API keys hashed at rest (argon2), verified by middleware before dispatch, with a key-lifecycle API (create/list/revoke/rotate, last-used tracking) — this also gives admin-key rotation without redeploy (today: edit config + restart, `config/admin_auth.rs:13`). Fine for a trusted LAN; required before B6 images default to public endpoints.
- [ ] **H2. SSRF hardening.** DNS-rebinding TOCTOU: validation resolves DNS, then reqwest re-resolves independently (`ssrf.rs`, `http_common.rs:72-81`) — pin the validated IPs into the HTTP client. Also: re-validate redirects, cover IPv6 ULA/link-local ranges, decide the resolution-failure policy (currently allowed through), and extend URL validation to DB/Mongo/Redis connector URLs (pointing a "connector" at an internal service is the same attack).
- [ ] **H3. Secrets handling.** (a) Connector configs are stored plaintext in `connectors.config_json` (`001_initial.sql:8`) — add optional encryption at rest (key from config or cloud KMS). (b) Secret masking is a denylist by key name — `client_secret`, `private_key`, etc. leak in cleartext via GET (`src/connector/masking.rs`); switch to an allowlist. (c) Implement the `vault://` / `aws-sm://` / cloud secrets-manager resolvers the `SecretResolver` trait already anticipates (`connector/secrets.rs:65`) — **prerequisite for B6** so marketplace instances pull credentials from the cloud's secret store.
- [ ] **H4. Audit v2.** Actor is an 8-char key prefix or `"anonymous"`; no IP/user-agent/request-id; `details` always `None` (no before/after diffs); fire-and-forget writes can drop entries (`admin/mod.rs:50-85`); list API has no filters (`admin/audit.rs:34`). Enterprise self-hosters need attributable, complete, filterable audit trails for their own compliance.
- [ ] **H5. Operator APIs.** (a) Trace DLQ has no operator surface — table + retry worker exist but no route to list/inspect/replay/purge stuck entries. (b) Backup is SQLite-only `VACUUM INTO` with no restore endpoint (`admin/backups.rs:26-104`) — add restore, and document the PG/MySQL story (B3 covers the cluster-mode angle).

---

## New configuration (sketch)

```toml
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
  - activate a channel via node A → node B serves it within one epoch poll (A2);
  - same idempotency key sent to A then B → second gets 409 (A6);
  - a DLQ row is retried by exactly one node (A3);
  - a per-channel limit of 10 rps holds at ~10 rps across both nodes combined (A4).
- **Rolling-deploy drill:** SIGTERM one of two nodes under load → `/readyz` goes 503 immediately, zero 5xx observed at the LB (A9).
- **Compat suite:** full existing integration suite (16.5k lines) must pass untouched with `cluster.enabled = false`.
- **Unit-test fallout to budget for:** rate-limit in-file tests (`server/rate_limit.rs:224-389`), registry tests, `migration_gen` golden tests if A10/0.8 touch the generator.

---

## Sequencing

| Milestone | Contents | Exit criteria |
|---|---|---|
| **M1 — Hardening** | Workstream 0 (all items) | Existing suite green; dedup channel-scoped; traces authed; DLQ config honored |
| **M2 — Coordination** | A1–A11 | Multi-node harness green; rolling deploy with zero 5xx demonstrated |
| **M3 — Ops & polish** | B1–B5, C1–C2 | Helm install of a 3-replica cluster passes the harness scenarios |
| **M4 — Marketplace & hardening** | B6, H1–H5 (H1 + H3 gate B6) | Marketplace image boots against managed Postgres/Redis with secrets from the cloud secret store and data-plane auth on by default |

## Explicit non-goals

- **Multi-tenancy and SaaS in any form** — permanent product decision (2026-07-26). Orion is a single-tenant, self-hosted product; the cloud offering is marketplace images users run in their own accounts (B6). No hosted control plane, no usage metering/billing pipeline, no per-customer provisioning.
- Shared/distributed circuit-breaker state (D3 decision).
- OIDC/SSO and a user/role model — static + managed API keys only (H1); console identity is the operator's concern.
- Leader election frameworks, gossip, node registries (D1/D4 decisions).
