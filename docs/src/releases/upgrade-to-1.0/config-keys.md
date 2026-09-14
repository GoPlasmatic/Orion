<!-- description: Four config sections were renamed in Orion 1.0, a misspelled ORION_ override now stops the boot, and private connectors need allow_private_urls. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# Config keys

Break 7 of eleven in the 0.3.0 → 1.0.0 upgrade.

## Before you start

Read [Upgrade to 1.0.0](./index.md) first: it carries the checklist, the backup step and the `preflight` scan.

**What changed.** Four sections and one environment variable were renamed. Audit-log retention moved out of `[queue]` into its own section, with its own cleanup cadence.

| Pre-1.0 | 1.0 |
|---|---|
| `[queue]` | `[trace_queue]` |
| `queue.trace_retention_hours` | `trace_queue.retention_hours` |
| `queue.trace_cleanup_interval_secs` | `trace_queue.cleanup_interval_secs` |
| `queue.audit_retention_days` | `audit.retention_days` |
| *(none)* | `audit.cleanup_interval_secs` — new, default `3600` |
| `[channels]` | `[channel_filter]` |
| `[tracing.storage]` | `[trace_storage]` |
| `ORION_ENV` | `ORION_ENVIRONMENT` |
| `kafka.max_inflight` | *removed* — see [One key was removed outright](#one-key-was-removed-outright) |

Every other `[queue]` key keeps its name under `[trace_queue]`, and every `[tracing.storage]` key keeps its name under `[trace_storage]`. Environment variables follow: `ORION_QUEUE__*` → `ORION_TRACE_QUEUE__*`, `ORION_CHANNELS__*` → `ORION_CHANNEL_FILTER__*`, `ORION_TRACING__STORAGE__*` → `ORION_TRACE_STORAGE__*`.

**Why.** Each name was wrong in a way that cost a paragraph to explain.
`[queue]` only ever configured the async trace queue. `[channels]` selects
*which* channels an instance loads and configures none of them.
`queue.trace_cleanup_interval_secs` drove the audit cleanup job too, so the docs had to say so in three places. `[tracing]` is OpenTelemetry export while `[tracing.storage]` is Orion's own `traces` rows. They are unrelated concerns nested under one name. `ORION_ENV` was the only variable not derived from its field path.

### One key was removed outright

That key is `kafka.max_inflight` (and
`ORION_KAFKA__MAX_INFLIGHT`). It was configured, validated and logged, and it did nothing. The consumer acquired a permit and then awaited each message inline, so concurrency was always exactly 1 whatever the value said. That sequential behaviour is load-bearing for the [at-least-once contract](./kafka-delivery.md). Committing an offset implicitly commits every earlier offset on the partition. In-consumer concurrency would therefore let a fast later message commit past a failed earlier one, and lose it. Rather than ship a knob that lies, 1.0 removes it. Nothing about runtime behaviour changes; delete the key and the variable. To increase throughput, run more Orion instances in the same consumer group (`kafka.group_id`) — Kafka spreads the partitions across them.

**How you'll notice.** Both halves fail loudly:

- **Config file**: a retired key is rejected by `deny_unknown_fields`
  (see the next section) and the error names it.
- **Environment**: a retired `ORION_*` name is a startup error listing every
  offender and its replacement:

  ```
  Error: Configuration error: these environment variables were renamed or
  removed in 1.0 and are no longer read (see
  docs/src/operate/upgrading-to-1.0.md):
    ORION_ENV -> ORION_ENVIRONMENT
    ORION_QUEUE__WORKERS -> ORION_TRACE_QUEUE__WORKERS
  ```

  A removed variable names its reason rather than a replacement, so
  `ORION_KAFKA__MAX_INFLIGHT` reports `removed in 1.0 (K4): Kafka messages are
  processed strictly sequentially per consumer …`.

  This is deliberate rather than convenient. Overrides are matched by name, not
  deserialized, so nothing would otherwise notice that `ORION_QUEUE__WORKERS`
  had stopped applying. For `ORION_ENV` specifically, silence would be a security regression. Falling back to `development` turns the production admin-auth and wildcard-CORS checks from startup errors back into warnings.

**What to do.** Rename the keys in your config file and your deployment
manifests, then confirm with:

```bash
orion-server validate-config -c config.toml
```

The Helm chart and `docker-compose.ha.yml` were updated in this release. If you templated your own manifests from them, `ORION_ENV` is the one to grep for first.

**One behaviour change beyond the renames:** audit cleanup now runs on
`audit.cleanup_interval_secs` instead of borrowing the trace job's interval. If you had tuned `queue.trace_cleanup_interval_secs` to control *both* jobs, set both new keys to that value to preserve the old behaviour.

### Misspelled environment overrides now stop the boot

A misspelled override used to be ignored in silence. Overrides are matched by name rather than deserialized, so `ORION_SERVER__PORTT=3000` did exactly nothing and you found out from a port number in a log line. It is now a startup error naming every offender at once and suggesting the nearest real key:

```
Error: Configuration error: these ORION_* environment variables are not Orion
settings and would be silently ignored:
  ORION_SERVER__PORTT (did you mean ORION_SERVER__PORT?)
```

**What is affected is narrow.** Only names carrying the `__` section separator are checked, plus near-misses of `ORION_ENVIRONMENT`. That is the one setting whose path has a single segment. A name without a `__` is not a setting name and is left alone, because `ORION_` is not Orion's to claim. So this does **not** affect:

- Kubernetes service links. A namespace with a Service called `orion` gives
  every pod `ORION_SERVICE_HOST`, `ORION_PORT`, `ORION_PORT_8080_TCP_ADDR` and
  more unless the PodSpec sets `enableServiceLinks: false`. The chart now sets
  it, but you do not need it: nothing in that block can be refused.
- `orion-cli`'s `ORION_SERVER_URL` / `ORION_API_KEY`, even exported in the
  shell you start the server from.
- Compose interpolation such as `${ORION_VERSION}`, which `docker compose`
  resolves in your shell — it never reaches the server.

Before upgrading, list the `__`-carrying `ORION_*` names everywhere your deployment sets them. That means a Deployment's `env:`/`envFrom:`, a Compose `environment:` block, a systemd unit and the shell you launch the binary from. Check each against the [configuration reference](../../reference/configuration/index.md):

```bash
env | grep -oE '^ORION_[A-Z0-9_]+' | grep '__'
```

Two escape hatches for names Orion should not interpret:

- Reference them from your config file with `${VAR}` — substitution reads them
  on Orion's behalf, so they are allowed.
- Or put them under `ORION_SECRET_*`, which is never read as configuration.
  This is the namespace for `env://` connector secrets and for `${VAR}` inside
  a connector `config_json`, since connectors live in the database and cannot
  be enumerated while the config loads. Only a name that
  *could* be a misspelled override needs moving — that is, one carrying the
  `SECTION__KEY` separator. A connector holding
  `"token": "env://ORION_DB__PASSWORD"` needs that variable renamed to
  `ORION_SECRET_DB_PASSWORD` (or out of the prefix entirely) and its `env://`
  reference updated to match; a single-underscore name like
  `ORION_API_TOKEN` is left alone and needs no change.

Everything is reported in one pass, so a single restart confirms a whole manifest.

One caveat is worth knowing. A setting typed with a *single* underscore, `ORION_SERVER_PORT` for `ORION_SERVER__PORT`, is byte-for-byte the shape of a service link, so it is ignored rather than reported. Type the double underscore.

**If you copied `ORION_ADMIN_AUTH__API_KEY` from the deployability page,** the
correct name is `ORION_ADMIN_AUTH__API_KEYS` (plural, comma-separated). The singular form was never read, so admin auth was enabled with no keys loaded. The page is fixed, and the singular name is now a startup error rather than a silent one.

---

### Connectors on private networks need `allow_private_urls`

**What changed.** SSRF protection used to cover the `http` connector and the Elasticsearch helper, and nothing else. No `db`, `cache`, `mongo` or `kafka` path checked its endpoint at all. A connector holding `postgres://…@169.254.169.254/…` was accepted and dialled. Now every connector type is checked twice. A **scheme allow-list** applies when it is created or updated, and a **private-address check** when the connection is first opened.

**How you'll notice.** Two different ways, and only the first is loud:

- On create/update, a connector whose scheme cannot belong to its backend is
  refused with `400` and a message naming the allowed schemes. `db` accepts
  `postgres`, `postgresql`, `mysql`, `mariadb`, `sqlite`, `mongodb`,
  `mongodb+srv`; `cache` (Redis) accepts `redis`, `rediss`; `es` accepts
  `http`, `https`; Kafka `brokers` must be bare `host:port`, not URLs.
  **Existing stored connectors are not re-validated**: you meet this the next
  time you edit one.
- At runtime, the **first request** through a connector pointed at a private
  address fails. The response is generic (the data plane is anonymous), but the
  trace carries the full message, naming the address and the flag.

**What to do.** Set `allow_private_urls: true` on every `db`, `cache` and `kafka` connector whose target is intentionally on a private network. That is the normal case for a database or a cache:

```json
{
  "connector_type": "db",
  "config": {
    "type": "db",
    "connection_string": "postgres://orion:…@postgres.internal:5432/orion",
    "allow_private_urls": true
  }
}
```

The flag is not a workaround; it is the point. A database on `10.x` is expected, and stating it keeps the *unstated* case — a workflow-authored connector reaching `169.254.169.254` — refused by default. Nothing here is skipped for `sqlite:` connection strings or `backend: "memory"` caches, because neither opens a socket.

Because the driver re-resolves the hostname when it dials, this is a guard rather than a guarantee. Keep network-level egress policy where the difference matters.

---

## Related

- [Upgrade to 1.0.0](./index.md): the checklist, and every other break.
- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored ones.
- [Releases](./index.md): what changed in each version.
