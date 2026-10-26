<!-- description: The [storage] settings: the storage.url that selects SQLite, PostgreSQL or MySQL, pool sizing, encryption at rest, backups and auto_migrate. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# Storage settings

The `[storage]` section: the database URL, which selects the backend at runtime, the connection pool, encryption at rest, backups, migration at boot, and how long the deployment store keeps what nothing references.

## Synopsis

```toml
[storage]
url = "sqlite:orion.db"
max_connections = 50
min_connections = 5
busy_timeout_ms = 5000
acquire_timeout_secs = 3
idle_timeout_secs = 300
connector_encryption_key = ""
backup_dir = "./backups"
# backup_retention_count = …   # no default
auto_migrate = true
connect_retry_secs = 60
artifact_gc_interval_secs = 3600
artifact_gc_grace_secs = 86400
deployment_retain_revisions = 20
```

## Description

The database backend is selected at runtime from the `storage.url` scheme, with no rebuild:

| Backend | URL Format | Example |
|---------|------------|---------|
| **SQLite** | `sqlite:` | `sqlite:orion.db` or `sqlite::memory:` |
| **PostgreSQL** | `postgres://` | `postgres://user:pass@host/db` |
| **MySQL** | `mysql://` | `mysql://user:pass@host/db` |

```bash
# SQLite (default)
orion-server

# PostgreSQL
ORION_STORAGE__URL="postgres://user:pass@localhost/orion" orion-server
```

Migrations for all backends are embedded in the binary and the correct set is selected automatically at startup.

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `storage.url` | `"sqlite:orion.db"` | `ORION_STORAGE__URL` | Point at PostgreSQL or MySQL for anything multi-replica or high-write. |
| `storage.max_connections` | `50` | `ORION_STORAGE__MAX_CONNECTIONS` | **Size against the database's own limit**; see [Sizing the pool](#sizing-the-pool). |
| `storage.min_connections` | `5` | `ORION_STORAGE__MIN_CONNECTIONS` | Raise to keep more connections warm under bursty traffic; `0` keeps none. |
| `storage.busy_timeout_ms` | `5000` | `ORION_STORAGE__BUSY_TIMEOUT_MS` | SQLite only — raise under heavy concurrent writes. Ignored by other backends. |
| `storage.acquire_timeout_secs` | `3` | `ORION_STORAGE__ACQUIRE_TIMEOUT_SECS` | How long a request waits for a free pooled connection before failing. Lower it to shed load faster; raise it only if brief pool exhaustion is expected and acceptable. |
| `storage.idle_timeout_secs` | `300` | `ORION_STORAGE__IDLE_TIMEOUT_SECS` | Lower it when a proxy (PgBouncer, RDS Proxy) closes idle connections sooner; `0` never closes them. |
| `storage.connector_encryption_key` | `""` | `ORION_STORAGE__CONNECTOR_ENCRYPTION_KEY` | Encrypt `connector_oauth_state.state_json`, the OAuth2 tokens a connection obtains, at rest (AES-256-GCM). Empty = plaintext. 64-hex key (`openssl rand -hex 32`); prefer the env var. Pre-existing plaintext rows keep loading and re-encrypt on their next write. |
| `storage.backup_dir` | `"./backups"` | `ORION_STORAGE__BACKUP_DIR` | Where `POST /api/v2/admin/backups` writes. SQLite only. |
| `storage.backup_retention_count` | — | `ORION_STORAGE__BACKUP_RETENTION_COUNT` | Keep only the newest N backups, pruning older ones after each successful backup. Unset keeps every backup — they accumulate on the same disk as the live database. Set the variable to an empty string to clear it. |
| `storage.auto_migrate` | `true` | `ORION_STORAGE__AUTO_MIGRATE` | **Set `false` for multi-replica deployments** and run `orion-server migrate` as a deploy step — required in a production cluster. |
| `storage.connect_retry_secs` | `60` | `ORION_STORAGE__CONNECT_RETRY_SECS` | How long startup keeps retrying an unreachable database before giving up (`0` = fail fast). Sized so a pod rides out a Postgres/MySQL failover instead of crash-looping through it. Ignored for SQLite, whose connect failures are not transient; the `auto_migrate = false` pending-migration check stays fail-fast regardless. [`migrate --wait`](../cli/orion-server/migrate.md) overrides it for one run. |
| `storage.artifact_gc_interval_secs` | `3600` | `ORION_STORAGE__ARTIFACT_GC_INTERVAL_SECS` | How often the [artifact collector](#artifact-collection) runs; `0` never collects. An unreferenced artifact is marked draining on one pass and its bytes deleted on the next, so it leaves the database between one and two intervals after its last reference does. |
| `storage.artifact_gc_grace_secs` | `86400` | `ORION_STORAGE__ARTIFACT_GC_GRACE_SECS` | An artifact written or revived within this many seconds, on the database clock, is never collected. Raise it above the longest gap you expect between `POST /definitions` and the `POST /deployments` that names what it uploaded. |
| `storage.deployment_retain_revisions` | `20` | `ORION_STORAGE__DEPLOYMENT_RETAIN_REVISIONS` | Newest deployment revisions the collector keeps whole, besides the head and any revision a node reports holding. An older revision is compacted: its artifacts become collectable and a rollback to it answers `ROLLBACK_TARGET_COMPACTED`. Must be `>= 1`. |

**Use SQLite for:** a single instance — a development or design-time node, an appliance install, anything where one process owns the database file. It is the default, provisions nothing, and creates the file on first boot. It is also the only backend with an [in-product backup](../../operate/maintain/backup-restore.md#before-you-start).

**Use PostgreSQL or MySQL for:** more than one replica, write-heavy estates, and every deployment that needs [cluster mode](../../operate/deploy/cluster.md#before-you-start). Cluster mode refuses to start against a `sqlite:` URL, because a file is single-host. Backup and restore become your snapshot or PITR tooling's job; Orion provides neither for these backends.

### Sizing the pool

`max_connections` is per process. With N replicas, N × `max_connections` must stay below the server's own `max_connections`, or replicas fail to connect under load. PostgreSQL's default is 100, and superuser slots and other clients come out of that budget. The default of 50 suits a single node against a dedicated database. Three replicas against a stock Postgres want roughly 25 each, less whatever else connects.

### `auto_migrate` in a cluster

With `auto_migrate = true`, every replica tries to migrate at boot and they race. The intended shape is `auto_migrate = false` plus `orion-server migrate` as a pre-deploy job. Startup then fails fast if migrations are still pending, instead of serving against a schema it does not understand.

`cluster.enabled = true` with `auto_migrate = true` is **a startup error in production**: a guardrail that fires after the race is no guardrail. Outside production it stays a warning. That is what lets a throwaway cluster boot without a migrate step. The Helm chart's `devStack` is one: its database is created by the same release, so a pre-install hook cannot migrate it. A single-node install is unaffected either way. `cluster.enabled` is `false` by default, and migrating at boot is what makes `orion-server` a single-binary install.

The migrate step already exists in both reference deployments. The Helm chart runs a pre-install/pre-upgrade `orion-server migrate` Job, and `docker-compose.ha.yml` a one-shot `migrate` service the replicas depend on.

### Artifact collection

Every definition, connection configuration and connector-package file a deployment can name is stored once, by digest, in the state database. A background job collects the ones nothing can still serve from. It runs every `artifact_gc_interval_secs`; in a cluster the node holding the `deployment_gc` job lease runs it, and the pass is one transaction behind the same head lock an apply takes, so it never interleaves with a deployment.

It never collects an artifact that is:

- named by a retained revision: the head, the `deployment_retain_revisions` newest, and any revision a node reports holding. That covers each member's definition, each provider package and its files, and the provider lock;
- a connection revision's configuration;
- a connector package that is not revoked, admitted or still awaiting admission, or one of its files;
- written or revived within `artifact_gc_grace_secs`, on the database clock.

Anything else is marked draining on one pass, keeping its bytes, and deleted on the next. A draining artifact that something references again is put back. Uploading a deleted artifact again revives it with a fresh grace window. `orion_deployment_artifacts_collected_total` counts both phases, and `orion_job_last_success_timestamp_seconds{job="deployment_gc"}` goes stale when passes stop succeeding.

## Related

- [Back up and restore](../../operate/maintain/backup-restore.md): the per-backend procedure.
- [Deploy a cluster](../../operate/deploy/cluster.md): sizing the pool across replicas, and `auto_migrate`.
- [`orion-server migrate`](../cli/orion-server/migrate.md): the pre-deploy migration step.
- [Server configuration](./index.md): every section, by what you are configuring.
