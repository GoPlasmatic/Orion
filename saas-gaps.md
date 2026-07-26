# Orion SaaS Readiness — Gap Analysis

*Generated from a full codebase audit on 2026-07-19 (branch `main`, commit `a0fc8e8`); revised 2026-07-26 after a second verification pass.*

> **Companion document:** [`multi-tenant-instance.md`](multi-tenant-instance.md) turns §2 (multi-tenancy) and §3 (HA) of this analysis into a concrete, file-level v1.0.0 change plan with design decisions, workstreams, and milestones.

## Verdict

Orion today is a **single-tenant, single-node, single-trust-domain appliance**. It is excellent at what it was designed for — a self-hosted declarative runtime with governance built in — but a SaaS offering requires capabilities that do not exist yet in any form: caller identity on the data plane, a tenant model, cross-replica coordination, usage metering, and secrets encryption. There is no partial scaffolding to extend; each of these is a from-scratch, cross-cutting addition.

**Strategic recommendation:** the fastest credible path to a SaaS is **dedicated-instance-per-tenant** (a control plane that provisions one isolated Orion instance + database per customer), which converts most "multi-tenancy" gaps into "platform ops" gaps and reuses the single-tenant architecture as-is. True shared multi-tenancy (one Orion serving many tenants) is a substantially larger rewrite touching every table, registry, and cache key. The phased plan at the end reflects this.

---

## 1. Identity & Access — the largest gap

### What exists
- Admin control plane: single static shared API key(s), constant-time compared, `Bearer` or custom header (`src/server/admin_auth.rs`). Keys are plaintext strings in config (`src/config/admin_auth.rs:13`). Disabled by default; production config validation refuses to run without it.
- Principal = first 8 chars of the key (`AdminPrincipal { key_prefix }`) — used only for audit logging.

### Gaps
| Gap | Severity | Evidence |
|---|---|---|
| **Data plane (`/api/v1/data/*`) is completely unauthenticated** — anyone who can reach the endpoint can invoke any channel | Critical | `admin_auth.rs:50` guards only `/api/v1/admin` + `/metrics`; `ChannelConfig` (`src/channel/config.rs`) has no `auth` field |
| **Trace read API is open** — `GET /api/v1/data/traces` returns full input/result payloads of every processed message, no auth, no ownership check | Critical | `src/server/routes/data.rs:812,836`; sits outside the admin-auth path prefix |
| No user model, roles, RBAC, scopes, or permissions — any admin key grants full control-plane access | Critical for SaaS | repo-wide: no `role`/`rbac`/`scope`/`permission` constructs |
| No API-key management: no key store, hashing (no argon2/bcrypt in deps), expiry, per-key identity, or rotation API (rotation = edit config + redeploy) | High | `config/admin_auth.rs:13` |
| No OAuth/OIDC/JWT/SSO anywhere — no login flow, token issuance, or verification | High (console/admin surface needs it) | no matching code or crates in `Cargo.toml` |
| Per-channel "auth" is only achievable by hand-rolled JSONLogic over raw headers — not real auth | High | `data.rs:270-279,405` |

### What SaaS needs
1. **Data-plane authentication**: per-channel (or per-tenant) API keys, hashed at rest, verified by middleware before channel dispatch; caller identity attached to the request context.
2. **Admin identity**: users + orgs, OIDC/SSO for the console, short-lived tokens; RBAC at minimum `owner / editor / viewer` per workspace.
3. **Key lifecycle APIs**: create/list/revoke/rotate, last-used tracking, scoped keys (per-channel, read-only, admin).
4. Put `/api/v1/data/traces*` behind auth immediately — this is a fix worth shipping even for self-hosted users.

---

## 2. Multi-Tenancy — absent by construction

### What exists
Nothing. The word "tenant" appears only as an example header (`x-tenant-id`) in rate-limit `key_logic` docs (`src/channel/config.rs:81`, `src/server/rate_limit.rs:193`) — client-supplied, unverified, and only scopes a rate-limit bucket.

### Gaps (every layer assumes one flat global namespace)
| Layer | Single-tenant assumption | Evidence |
|---|---|---|
| **Schema** | No `tenant_id`/`org_id` on any of the 6 tables; PKs are bare `(workflow_id, version)` / `(channel_id, version)`; `connectors.name` is globally `UNIQUE` | `migrations/*/001_initial.sql`; single-draft triggers/indexes key on bare IDs |
| **Routing** | One global `ChannelRegistry.by_name` map + one global `RouteTable` — two tenants cannot own the same channel name or REST path; second registration shadows the first | `src/channel/registry.rs:105-108`, `src/channel/routing.rs` |
| **Connectors** | Registry + circuit breakers + SQL/Mongo/Redis pool caches all keyed by globally-unique connector name; pools (and credentials) shared process-wide | `src/connector/registry.rs:39`, `pool_cache.rs`, `mongo_pool.rs`, `redis_pool.rs` |
| **Cache/dedup keys** | Response cache key is `cache:{channel}:{hash}` (channel-scoped, not tenant-scoped). **Dedup key is the raw idempotency header value with NO prefix at all** — idempotency tokens collide *across channels today* (pre-existing bug, see §7) | `data.rs:500-522` vs `data.rs:440-457`; shared `MemoryCacheBackend` DashMap (`cache_backend.rs:43-45`) |
| **Engine** | One global engine; all active channels+workflows flattened into one match set; rollout bucketing global | `src/server/state.rs:27`, `src/engine/mod.rs:280-354` |
| **Attribution** | Traces carry only `channel` name — no caller/IP/key/tenant column; audit `principal` is a key prefix or `"anonymous"` | `001_initial.sql` traces table; `admin/mod.rs:57-60` |
| **Quotas** | Body limit, result size, async queue memory (100 MB), worker pool — all process-global; one tenant's load exhausts them for everyone | `server/mod.rs:63`, `config/queue.rs`, `queue/mod.rs:88-135` |

### What SaaS needs (shared-multi-tenant path)
1. `tenant_id` column + composite uniqueness on all tables, views, triggers, and every repository query (three DB backends × all queries).
2. Tenant-prefixed namespaces for: channel names, REST route matching (e.g. `/t/{tenant}/...` or host-based), connector names, cache/dedup keys, circuit-breaker keys, pool-cache keys.
3. Per-tenant quotas: queue memory, worker share, payload size, channel/workflow/connector counts.
4. Tenant-scoped trace/audit queries ("list *my* traces").

> On the **dedicated-instance path**, all of §2 is replaced by: a provisioning control plane (instance per tenant, DB per tenant, DNS/routing per tenant) and §1/§3/§4 still apply.

---

## 3. Horizontal Scaling & HA — single-node architecture

### What exists (genuinely scales already)
- **Kafka ingestion**: shared consumer group, manual commits — N replicas cooperate correctly (`src/kafka/consumer.rs:97-138`).
- **Graceful lifecycle**: SIGTERM drain, ordered teardown, `/healthz` + `/readyz` K8s probes (`main.rs:686-771`, `routes/mod.rs:117-152`).
- Postgres/MySQL support with full migration parity (`migrations/{sqlite,postgres,mysql}/`).

### Gaps (multi-replica failure modes, most severe first)
| Gap | Failure mode with N replicas | Evidence |
|---|---|---|
| **No config propagation** — engine reload is node-local; no pub/sub, LISTEN/NOTIFY, polling, or config epoch | Admin activates a channel via node A; nodes B..N serve the stale engine **indefinitely**. Manual `POST /engine/reload` must be fanned out per node (awkward behind an LB) | `routes/mod.rs:156-224`, invoked only from `admin/mod.rs:106-121` + manual endpoint |
| **DLQ retry double-processing** — every node polls the same unclaimed rows; no `FOR UPDATE SKIP LOCKED`, lease, or node identity | Failed traces replayed up to N times; racy remove/record-retry | `queue/dlq_retry.rs`, `repositories/trace_dlq.rs:116-139` |
| **SQLite default with zero guardrails** — WAL is single-host; nothing warns/refuses in a multi-node setup | Divergent per-node DBs or corruption on shared FS | `config/mod.rs:122`, `storage/mod.rs:341-372` |
| **Rate limits × N** — governor DashMap state is per-process; no distributed token bucket | 100 rps configured ⇒ ~N×100 rps effective | `channel/mod.rs:22`, `rate_limit.rs:15-45` |
| **Backpressure & circuit breakers per-node** | `max_concurrent` multiplied by N; breaker opens on node A while B keeps hammering the failing upstream | `registry.rs:185-188`, `connector/registry.rs:60-101` |
| **Dedup & response cache default in-memory** — Redis backend exists but only when a channel explicitly names a Redis cache connector; all fallback paths land on the process-local DashMap | Idempotency broken across nodes; cache hit ratio ÷ N | `registry.rs:190-285`, `cache_backend.rs` |
| **Connector changes propagate to one node only** — connector CRUD calls a *separate* node-local `reload_connectors` + pool eviction, not `reload_engine`; other nodes keep stale configs and stale SQL/Mongo/Redis pools indefinitely | Rotate a DB credential via node A; nodes B..N keep using the old pool until restart | `admin/connectors.rs:25-41,94,145,177,272` |
| **Readiness never goes false during shutdown** — `ready` is set true once at boot and never cleared; plain-HTTP drain *keeps accepting new connections* for the whole `shutdown_drain_secs` window (TLS path behaves differently — stops accepting immediately) | LB keeps routing to a terminating pod for 30 s of the drain; rolling deploys serve errors | `main.rs:519` (only store), `routes/mod.rs:133`, `main.rs:750-771` vs `:656-673` |
| **Trace cleanup runs on every node** — retention DELETE fires on all replicas each interval | N× duplicate delete scans / write-lock contention (fatal on shared SQLite) | `queue/mod.rs:24-65`, `main.rs:565-569` |
| **Kafka consumer restarts trigger full group rebalances** — no `group.instance.id` (static membership); reload with a changed topic set does shutdown+rejoin, and N nodes reloading = N rebalances halting all consumption | Config change or rolling deploy stalls ingestion fleet-wide | `consumer.rs:97-102`, `routes/mod.rs:276-323` |
| **Reload resets runtime state** — per-channel rate limiters and backpressure semaphores are rebuilt from scratch on every engine reload (token buckets and in-flight accounting zeroed); circuit-breaker reset API is node-local (404s on nodes that haven't created that key) | Limits briefly over-admit after every config change; breaker reset must be fanned out manually | `registry.rs:150-153,185-188`, `admin/connectors.rs:314-333` |
| **Platform rate-limit key degrades to `"unknown"`** behind an LB that doesn't set `x-forwarded-for`/`x-real-ip` — all traffic shares one bucket | Effective global limit = one client's limit | `rate_limit.rs:48-69` |
| No clustering primitives at all — no leader election, distributed locks, node identity, config epochs | Every replica is an independent uncoordinated actor | repo-wide grep negative |
| Migrations auto-apply on boot with no opt-out (`init_pool` always migrates; no `auto_migrate` flag); no documented expand/contract strategy. sqlx serializes via advisory lock on Postgres/MySQL but has **no migration lock for SQLite** | Rolling deploys race on schema changes | `storage/mod.rs:281-311` |

### What SaaS needs
1. **Config-change propagation**: minimum viable = a `config_epoch` row + per-node poll (or Postgres LISTEN/NOTIFY; or Redis pub/sub) triggering `reload_engine`. This is the single highest-leverage scaling fix.
2. **DLQ claim semantics**: `SELECT ... FOR UPDATE SKIP LOCKED` (Postgres/MySQL) or a lease column.
3. **Redis-by-default for cross-node concerns** in SaaS mode: dedup, response cache, and a distributed rate limiter (governor has no Redis store — needs a Redis token bucket implementation).
4. **Startup guardrail**: refuse (or loudly warn) multi-replica-ambient signals with `sqlite:` storage; document Postgres as the SaaS-mode requirement.
5. Helm chart / K8s manifests (none exist today — only Dockerfile + docker-compose).
6. **Rolling-deploy correctness**: flip `ready=false` on SIGTERM (so `/readyz` pulls the node from the LB), unify TLS/plain drain semantics, and add Kafka static membership (`group.instance.id`) so restarts rejoin without a group rebalance.
7. **Background-job single-flight**: a shared `job_leases` table (or equivalent) so trace cleanup and DLQ retry run once per interval fleet-wide, not once per node.
8. Env-override coverage for K8s: `env_overrides.rs` is a hand-maintained list missing `ORION_QUEUE__DLQ_*`, `ORION_STORAGE__{MAX,MIN}_CONNECTIONS`, `ORION_CORS__ALLOWED_ORIGINS`, `ORION_KAFKA__TOPICS`, `ORION_CHANNELS__{INCLUDE,EXCLUDE}` — pure-env deployments can't set these today.

---

## 4. Metering, Billing & Quotas — nothing billable yet

### What exists
- Rich per-channel Prometheus metrics: `channel_executions_total`, `messages_total{channel,status}`, `message_duration_seconds{channel}`, connector counters, queue gauges (`src/metrics/mod.rs`).
- RPS rate limiting (global + per-channel, JSONLogic key), trace retention TTL (default 72 h) with cleanup task.

### Gaps
| Gap | Why it blocks SaaS | Evidence |
|---|---|---|
| **No durable usage ledger** — metrics are in-process Prometheus counters: reset on restart, aggregate-only, not queryable per account | Cannot invoice anyone | `metrics/mod.rs` (all counters) |
| **No per-caller/per-tenant dimension** on any success-path metric (only `rate_limit_rejections_total{client}`) | Cannot attribute usage | `metrics/mod.rs:192` |
| **No payload-byte accounting** — request/response volume never measured | Cannot do volume-based pricing | grep negative; only size *caps* exist |
| **No quota/plan/entitlement system** — only RPS throttling; no monthly volume caps, tiers, overage handling | Cannot enforce plans | grep for plan/quota/entitlement negative |
| Rollout bucketing is `rand::random` per request — not sticky per caller | Canary sees users flip-flop between versions | `engine/utils.rs:19`, `engine/mod.rs:314-349` |

### What SaaS needs
1. A **usage events pipeline**: per-request record (tenant, channel, timestamp, duration, bytes in/out, status) written async to a durable store (reuse the trace-queue pattern), aggregated hourly/daily for billing export (Stripe/Metronome/etc.).
2. Tenant-labelled metrics (careful with cardinality — aggregate per-tenant in the ledger, not in Prometheus).
3. Quota enforcement middleware: monthly request/byte caps per plan, with 429 + `X-Quota-Remaining`-style headers, and grace/overage policy.
4. Sticky rollout bucketing (hash of caller identity) for meaningful canaries.

---

## 5. Security & Compliance Hardening

### What exists
- TLS (rustls, off by default), always-on security headers, CORS with prod wildcard guard, SSRF IP blocklist with DNS resolution check, secret masking on connector API reads, `env://VAR` secret indirection with a `SecretResolver` trait designed for future `vault://`/`aws-sm://` backends, `cargo audit` + CodeQL in CI.

### Gaps
| Gap | Severity | Evidence |
|---|---|---|
| **Connector secrets stored plaintext in DB** (`connectors.config_json TEXT`) — no encryption at rest, no KMS/envelope crypto | Critical for SaaS (you'd hold customer DB credentials) | `001_initial.sql:8`, `admin/connectors.rs:85` |
| Secret masking is a **denylist by key name** — `client_secret`, `private_key`, etc. leak in cleartext via GET | High | `src/connector/masking.rs` |
| SSRF: per-connector `allow_private_urls` bypass; DNS-rebinding TOCTOU (check resolves, reqwest re-resolves); resolution failure allowed through; no IPv6 ULA/link-local; no redirect re-validation; only HTTP connectors covered | High (in SaaS, tenants author connector URLs → SSRF against *your* VPC) | `ssrf.rs`, `http_common.rs:72-81`, `connector/config.rs:90` |
| Audit log: actor = 8-char key prefix or `"anonymous"`; no IP/user-agent/request-id; `details` always `None` (no diffs); fire-and-forget writes can drop; data-plane and read-access never audited; list API has no filters | High (compliance: SOC 2 needs attributable, complete audit trails) | `admin/mod.rs:50-85`, `admin/audit.rs:34` |
| Secret resolver ships env-only — no Vault/cloud-SM backend implemented | Medium | `connector/secrets.rs:65` |
| No configurable TLS policy (min version/ciphers); admin key rotation requires redeploy | Low/Medium | `config/server.rs:81-90` |

### What SaaS needs
1. **Envelope encryption for connector configs** (per-tenant data key wrapped by a KMS master key) — prerequisite for holding customer credentials.
2. SSRF: remove/flag-gate `allow_private_urls` in SaaS mode, pin resolved IPs into the HTTP client (defeat rebinding), validate redirects, extend coverage to DB/Mongo/Redis connector URLs (a tenant pointing a "connector" at your internal Postgres is the same attack).
3. Audit v2: full actor identity, IP, request-id, before/after diffs, transactional or at-least-once writes, filterable query API, data-plane access logging.
4. Implement the `vault://` / cloud secrets-manager resolvers the trait already anticipates.

---

## 6. Tenant-Facing Operations & Platform Ops

### Gaps
| Gap | Notes | Evidence |
|---|---|---|
| Backup = SQLite-only `VACUUM INTO`, **no restore endpoint**, no Postgres/MySQL backup, no per-tenant export | SaaS needs per-tenant export (offboarding, GDPR portability) and operator PITR (use managed Postgres) | `admin/backups.rs:26-104` |
| **No operator API for the trace DLQ** — table + retry worker exist, but no route to list/inspect/replay/purge stuck traces | Support team is blind to stuck work | grep `src/server` for dlq routes: none |
| Audit-log list API: pagination only, no filtering by action/resource/principal/time | | `admin/audit.rs:34` |
| No runtime config reload (no SIGHUP/watcher) — most config changes need a restart | Acceptable if instances are cattle; painful otherwise | grep `src/config` negative |
| No Helm chart or K8s manifests; no per-tenant provisioning tooling | The dedicated-instance path lives or dies on this | repo root |
| No status page / tenant-visible health, no per-tenant trace search UI hooks | Console (Orion-ui) currently assumes single operator | — |

---

## 7. Pre-existing bugs found during this audit (fix regardless of SaaS)

1. **Dedup keys are not channel-scoped** — the raw idempotency header value is the cache key, and all in-memory-dedup channels share one DashMap, so the same token suppresses requests *across different channels*. (`data.rs:440-457` vs the properly-prefixed response-cache key at `data.rs:500-522`.)
2. **`/api/v1/data/traces` and `/traces/{id}` are unauthenticated** and return full request/response payloads. (`data.rs:812,836`; auth middleware scope `admin_auth.rs:50`.)
3. **DLQ retry double-processing** the moment anyone runs two replicas — no row claim. (`trace_dlq.rs:116-139`.)
4. **SSRF DNS-rebinding TOCTOU** — validation resolves DNS, then reqwest resolves again independently. (`ssrf.rs`, `http_common.rs:72-81`.)
5. `src/channel/dedup.rs` (`DeduplicationStore`) is confirmed dead code — the only reference outside the file is its re-export in `channel/mod.rs:17`; the live path uses `CacheBackend`.
6. **`queue.dlq_max_retries` config is ignored** — it is logged at startup (`main.rs:585`) but the enqueue path hardcodes `5` (`queue/processing.rs:500`).
7. **`traces.channel_id` is never written** — the column exists (and is indexed) but no repository path populates it (`traces.rs:158-164,284-295`).
8. **Dedup backend errors fail closed as 409** — `check_and_insert(...).unwrap_or(false)` treats a Redis outage as "duplicate", rejecting every request on that channel with Conflict. (`data.rs:450`.)
9. **Silent memory fallback for dedup/response cache** — a missing/typo'd/non-cache connector degrades to the process-local DashMap with only a `warn!`; idempotency silently stops working across restarts and replicas. (`registry.rs:198-233,257-281`.)
10. **Backend parity gap: active-immutability triggers exist only on SQLite** — Postgres and MySQL enforce nothing; an active workflow/channel can be mutated in place on those backends. (`migrations/sqlite/001:136-168` vs pg/mysql migration files.)
11. `BackpressureConfig.queue_depth` is parsed but never read anywhere. (`channel/config.rs:114-121`.)

---

## Suggested Roadmap

### Phase 0 — Hardening (valuable for self-hosted users too; ~independent fixes)
- Auth on trace endpoints; channel-scope the dedup key; DLQ `SKIP LOCKED`; SSRF rebinding fix + connector-URL coverage; audit v2 (actor, IP, diffs, filters); DLQ operator API; masking → encrypt-at-rest for connector configs; remove dead dedup module.
- From the second audit pass: wire `dlq_max_retries`; write `traces.channel_id`; fail-open (with metric) on dedup-backend errors; PG/MySQL active-immutability parity; `ready=false` on SIGTERM; implement-or-remove `backpressure.queue_depth`.

### Phase 1 — "Hosted Orion" (dedicated instance per tenant — fastest sellable SaaS)
- Control plane service: signup/org model, OIDC login, instance provisioning (K8s namespace or VM per tenant, managed Postgres per tenant), DNS/TLS automation, Helm chart.
- Per-instance: admin-key issuance/rotation API, data-plane API keys (hashed store + middleware), usage-events ledger + export for billing, plan-based quota middleware.
- Ops: Postgres-required SaaS profile, config-epoch reload propagation (needed once an instance runs >1 replica), backup via managed-DB PITR, monitoring per instance.

### Phase 2 — Shared multi-tenancy (only if unit economics demand it)
- `tenant_id` across schema + all repositories (×3 backends); tenant-prefixed routing/naming/cache/dedup/breaker/pool keys; per-tenant quotas on queue memory/workers/payloads; tenant-scoped traces/audit; distributed rate limiting; noisy-neighbor isolation testing.

### Phase 3 — SaaS polish
- Sticky canary rollouts, per-tenant export/offboarding, SOC 2 evidence automation, status page, billing-provider integration, tenant-visible metrics dashboards in the console.
