<!-- description: Upgrading Orion 0.3.0 to 1.0.0: the pre-flight checklist, which backend you were on, and the eleven breaks each on its own page. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# Upgrade to 1.0.0

This page is for operators upgrading an existing Orion deployment from
**0.3.0** (the previous release) to **1.0.0**. It covers only what *breaks* or
*changes behaviour*. New capabilities are in the
[CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/CHANGELOG.md); new configuration keys are in the [Config Reference](../../reference/configuration/index.md).

Every item below is written as **what changed → how you'll notice → what to do**. Nothing here requires a workflow or channel rewrite; the changes are in the runtime's request path, deployment defaults, and operational surfaces.

The version-independent procedure — back up, preflight, validate config, migrate, roll — is on [Upgrades](../../operate/maintain/upgrades.md), along with the policy on how a renamed key fails.

Each change below is collapsed to its heading, so the eleven sections read as a list of what breaks. Open the ones that touch you; **Expand all** opens the lot.

<div class="fold-sections" data-level="3" data-default="closed"></div>

---

## Before you start

**Run this first.** It answers the database-backed rows below against your actual estate rather than in the abstract, and exits non-zero if it finds anything. Those rows are 3 and 14, the two channel-config renames, and duplicate channel names. The other rows are config- or client-side and it cannot see them:

```bash
orion-server preflight
```

It is read-only, needs only `storage.url`, and reports each finding with the checklist row it belongs to. Run it with the **1.0 binary against your 0.3.0 database**, before you start the rollout. Config-file and `ORION_*` problems are reported separately by `orion-server validate-config`.

Then work through this list. Each row links to the section with the detail.

| # | Check | Applies to you if |
|---|-------|-------------------|
| 1 | [Set `rate_limit.trusted_proxies`](./rate-limiting.md) | You run behind a proxy, LB, or ingress — **whether or not** `rate_limit.enabled` is set, if any channel declares a `rate_limit` block |
| 2 | [Update dashboards and alerts](./metrics.md) | You scrape `/metrics` — three families changed name or labels, and `/metrics` now answers 404 when metrics are off |
| 3 | [Audit stored channel configs](./stored-channel-config.md), and [remove unknown keys](./api-and-response-shape.md#unknown-keys-in-a-channel-config-are-now-refused) from them | Always — this one silently stops individual channels serving, without failing a boot or an admin call. Unknown keys include the pre-1.0 `cors` and `backpressure.max_concurrent` spellings, `queue_depth`, and typos that were silently ignored before |
| 4 | [Enable the Kafka DLQ](./kafka-delivery.md) | `kafka.enabled = true` |
| 5 | [Size every ingress against the channel's guards](./kafka-delivery.md#every-ingress-applies-the-channels-rate-limit-dedup-and-backpressure) | Any channel declaring `rate_limit`, `deduplication`, `backpressure` or `timeout_ms` is reached over Kafka, `/async`, or `channel_call` — **this one silently throttles or suppresses live traffic**. Also: the platform `[rate_limit]` budget now [stacks on the channel's own limit](./kafka-delivery.md#the-platform-limiter-and-the-channels-now-stack) instead of being bypassed by it |
| 6 | [Supply admin API keys](./deployment-defaults.md) | You deploy through the Helm chart or `docker-compose.ha.yml` |
| 7 | [Back up before migrating](./database-migrations.md), and [re-point anything reading `workflows.tags` or `channels.methods`](./database-migrations.md#two-json-columns-were-renamed) | You are on PostgreSQL, or you query Orion's tables directly — dashboards, ETL, reporting views, hand-maintained restores |
| 8 | [Stop migrating at boot in a production cluster](./database-migrations.md#a-production-cluster-may-not-migrate-at-boot) | `environment` starts `prod` **and** `cluster.enabled = true` **and** `storage.auto_migrate = true` — refused at startup now |
| 9 | [Pass `trace_token` when polling async traces](./security-and-access.md#polling-an-async-trace-now-requires-the-token-returned-with-the-202) | You submit to `/async` and poll `GET /traces/{id}` without an admin key |
| 10 | [Rename the renamed config keys](./config-keys.md) | You set `[queue]`, `[channels]`, `[tracing.storage]`, or `ORION_ENV` |
| 11 | [Delete `kafka.max_inflight`](./config-keys.md) | You set `kafka.max_inflight` in the config file or `ORION_KAFKA__MAX_INFLIGHT` in the environment — Kafka enabled or not |
| 12 | [Audit your `ORION_*` environment](./config-keys.md#misspelled-environment-overrides-now-stop-the-boot) | You set any `ORION_*` variable containing `__` that is not on the config reference page |
| 13 | [Check client URL casing](./api-and-response-shape.md#a-rest-route-matches-byte-exactly-and-decodes-parameters-once) | You call data-plane REST routes with casing that differs from the channel's `route_pattern` |
| 14 | [Declare a `schema` on every `data_query` / `data_write`](./api-and-response-shape.md#the-data-dialect-rejects-what-it-used-to-ignore), and [move the `data_write` envelope under `write`](./renames-and-removals.md#data_write-takes-its-envelope-under-write) | Any workflow uses the portable data dialect — **this one breaks every 0.x dialect task at its first request**. The pre-1.0 flat `data_write` form is refused too; `orion-server preflight` lists both |
| 15 | [Stop reading `total` from the trace list](./api-and-response-shape.md#the-trace-list-no-longer-returns-total-by-default) | You page `GET /api/v1/admin/traces` |
| 16 | [Re-point anything scraping `/docs`](./security-and-access.md#docs-and-the-openapi-spec-are-off-in-production) | You fetch `/docs` or `/api/v1/openapi.json` and run with `environment = "production"` |
| 17 | [`chown` existing data volumes](./deployment-defaults.md#the-charts-pod-defaults-are-hardened-and-the-images-are-pinned), and [set `allow_private_urls` on private db, cache and Kafka connectors](./config-keys.md#connectors-on-private-networks-need-allow_private_urls) | You upgrade a Docker or compose deployment with an existing `/app/data` mount, or any `db`, `cache` or `kafka` connector points at a private address — **which is the normal case** |
| 18 | Review the grouped changes: [renames](./renames-and-removals.md), [security](./security-and-access.md), [API shape](./api-and-response-shape.md), [runtime behaviour](./runtime-behaviour.md) | Always |

**Take a database backup before upgrading.** Migrations run automatically at
boot unless you set `storage.auto_migrate = false`.

---

## Which backend were you actually on?

This matters more than it sounds, because it decides how much of this page applies to you.

- **SQLite**: the only storage backend that fully worked in 0.3.0. Assume the
  whole page applies.
- **PostgreSQL**: the 0.3.0 schema migrated cleanly, but the Rust models
  decode `i64` while the migration created `integer` (`INT4`) columns, and
  sqlx-postgres refuses `INT4 → i64`. **Every repository read failed at
  runtime**, so a 0.3.0 Postgres deployment could exist but could not serve.
  You still have a schema to migrate. See
  [Database migrations](./database-migrations.md).
- **MySQL**: the 0.3.0 migration set could not execute through sqlx *at all*
  (MySQL-client `DELIMITER` directives, `TEXT` columns with literal defaults,
  `TEXT` primary keys without prefix lengths). No MySQL deployment can exist to
  upgrade. Treat MySQL as new in 1.0.0 and start from an empty database.

---

## Recommended after upgrading

None of these are required, but each closes a gap 1.0.0 opened up for you.

**Hash your admin API keys at rest.** Plaintext keys in `admin_auth.api_keys`
still work unchanged. You can now store a SHA-256 digest instead, and clients keep presenting the plaintext key:

```bash
printf '%s' "$ORION_ADMIN_KEY" | shasum -a 256 | awk '{print "sha256:"$1}'   # macOS
printf '%s' "$ORION_ADMIN_KEY" | sha256sum   | awk '{print "sha256:"$1}'     # Linux
```

```toml
[admin_auth]
enabled = true
api_keys = ["sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]
```

The digest is a plain SHA-256 of the raw key bytes — no salt, no iteration. Use `printf '%s'`, not `echo`, so you do not hash a trailing newline. A malformed entry fails config validation with `admin_auth.api_keys: 'sha256:' entries must be followed by the 64-character hex SHA-256 digest of the key`.

Two consequences to plan for:

- **Hashing does not change the audit `principal`,** and that is the point: the
  actor is a `key-<16 hex>` derived from the key, identical whether the entry
  is configured in plaintext or `sha256:` form, so rotating an operator between
  the two does not rename them in the trail. See
  [Audit log](./security-and-access.md#audit-log-new-actor-format-new-fields-two-new-settings) for the
  format change itself, which does break saved `?principal=` filters.
- **A plaintext key whose literal text starts with `sha256:`** is now
  interpreted as the hash-at-rest form, and fails config validation. Rotate
  it first.

**Enable admin auth** if you have not. It is what guards `/metrics`, the trace
endpoints, and therefore the full-detail error payloads.

**Enable the Kafka DLQ**. See [section 4](./kafka-delivery.md).

**Set `storage.auto_migrate = false`** in any multi-replica deployment and run
`orion-server migrate` as a deploy step. In a *production* cluster this is no longer advice. See [A production cluster may not migrate at boot](./database-migrations.md#a-production-cluster-may-not-migrate-at-boot).

---

## One fix worth retrying: TLS

**TLS was unusable in 0.3.0.** Setting `server.tls.enabled = true` panicked the
process at boot with:

```
Could not automatically determine the process-level CryptoProvider
```

rustls 0.23 auto-selects a cryptography backend only when exactly one is enabled in the dependency graph, and Orion's graph enables both. `axum-server` and `reqwest` pull `rustls/aws-lc-rs`, while `mongodb` and `sqlx` pull `rustls/ring`. The server installs the `aws-lc-rs` provider explicitly before loading certificates as of 1.0.0, and the path now has test coverage.

If you tried HTTPS, hit that panic, and terminated TLS at a proxy instead: it works now.

```toml
[server.tls]
enabled = true
cert_path = "/etc/orion/tls/server.crt"
key_path  = "/etc/orion/tls/server.key"
```

---

## The eleven breaks

| Break | What it touches |
|---|---|
| [Rate limiting behind a proxy](./rate-limiting.md) | the limiter keys on the direct peer unless `rate_limit.trusted_proxies` names your proxy. |
| [Metrics: dashboards and alerts](./metrics.md) | three metric families changed name or labels, and `/metrics` answers 404 when metrics are off. |
| [Stored channel config](./stored-channel-config.md) | a channel whose stored config no longer parses is refused at every ingress, not served with a guard missing. |
| [Kafka delivery and ingress guards](./kafka-delivery.md) | Kafka delivery is at-least-once, and every ingress applies the channel's guards. |
| [Deployment defaults](./deployment-defaults.md) | the Helm chart and `docker-compose.ha.yml` require admin keys, and the pod defaults are hardened. |
| [Database migrations](./database-migrations.md) | what the migrations do per backend, the two renamed JSON columns, and migrating at boot in a cluster. |
| [Config keys](./config-keys.md) | four sections renamed, a misspelled `ORION_` override stops the boot, and `allow_private_urls`. |
| [Renames and removals](./renames-and-removals.md) | the `data_write` envelope, `response_path`, `cors`, the backpressure keys and the trace read routes. |
| [Security and access](./security-and-access.md) | the async trace token, admin-only trace reads, sanitised error bodies and allowlist masking. |
| [API and response shape](./api-and-response-shape.md) | every admin response wrapped in `data`, changed status codes, and a stricter data dialect. |
| [Runtime behaviour](./runtime-behaviour.md) | sticky rollouts, cache keys and namespaces, trace batching, startup retries and readiness. |

## Related

- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored breaks.
- [Server configuration](../../reference/configuration/index.md): every key, with its default.
- [Releases](./index.md): what changed in each version.
