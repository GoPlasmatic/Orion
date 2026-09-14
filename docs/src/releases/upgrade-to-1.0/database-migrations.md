<!-- description: What Orion 1.0's migrations do per backend, the two renamed JSON columns, and why a production cluster may not migrate at boot. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# Database migrations

Break 6 of eleven in the 0.3.0 → 1.0.0 upgrade.

## Before you start

Read [Upgrade to 1.0.0](./index.md) first: it carries the checklist, the backup step and the `preflight` scan.

Migrations run at boot unless `storage.auto_migrate = false`, in which case run `orion-server migrate` as a deploy step. In multi-replica deployments set `auto_migrate = false`, so replicas do not race at boot.

| Backend | New since 0.3.0 | Notes |
|---------|-----------------|-------|
| SQLite | `004`–`009` (cluster coordination, trace access token, single-draft-on-update, DLQ/audit indexes, trace pagination indexes, JSON column suffixes) | Additive; `008` is the slow one — see [How long it locks](#how-long-it-locks) |
| PostgreSQL | `004`–`013` (bigint columns, active immutability, cluster coordination, trace access token, recreated current views, DLQ/audit indexes, `010`–`012` for the trace pagination indexes, and `013` for the JSON column suffixes) | See [How long it locks](#how-long-it-locks) |
| MySQL | `001` rewritten; `004`–`012` added | No 0.3.0 deployment can exist — start fresh |

> **Migration numbers are per-backend and are not comparable.** Each backend
> has its own migration directory and its own version sequence, so the same
> number means different things: `004` is `cluster_coordination` on SQLite,
> `bigint_columns` on PostgreSQL and `active_immutability` on MySQL. There is
> no shared version space, and a number alone never identifies a change.
>
> **Refer to a migration by name.** `orion-server migrate --dry-run` prints the
> backend, the number and the name together, which is the unambiguous form:
>
> ```text
> Pending migrations on postgres (2):
>   postgres 012 — drop trace created at index
>   postgres 013 — json column suffixes
> ```

**PostgreSQL: `004_bigint_columns` needs care.** It drops the
`current_workflows` and `current_channels` views, widens `integer` columns to `bigint` on `workflows`, `channels`, and `trace_dlq`, then recreates the views. The `ALTER … TYPE bigint` rewrites those tables under an `ACCESS EXCLUSIVE` lock. They hold definition rows and a failed-trace backlog rather than request volume, so this is normally quick. It does block all access while it runs.

The migration is **not idempotent**: the `DROP VIEW` statements have no `IF EXISTS`. If the connection drops midway, the database is left without its two views. sqlx does not re-run version 004, because it is already recorded. **Take a backup first.** To recover manually, recreate the two views with the `CREATE VIEW` statements at the bottom of `crates/orion-server/migrations/postgres/004_bigint_columns.sql`.

**All backends: the trace pagination indexes are the slow part.** They add `idx_traces_updated_at` and `idx_traces_created_at_id`, then drop `idx_traces_created_at`. The composite is a strict superset for every query that used it, including the retention delete's `created_at < cutoff`. On a `traces` table with millions of rows these two `CREATE INDEX` statements dominate the whole 1.0 migration. Run it in a maintenance window if your `traces` table is large, or trim it first with `trace_queue.retention_hours`.

> **PostgreSQL: `010`–`012` run outside a transaction, on purpose.** Each file
> begins with a `-- no-transaction` marker and works `CONCURRENTLY` — `010`
> and `011` `CREATE INDEX CONCURRENTLY`, `012` `DROP INDEX CONCURRENTLY` the
> index they supersede, so they do **not** lock `traces` against writes
> while the indexes build — a plain `CREATE INDEX` holds a `SHARE` lock for the
> whole build, which on a large trace table is a write outage. Two consequences:
>
> - They are **three separate migration versions**, not one, because
>   `CONCURRENTLY` also refuses the implicit transaction PostgreSQL wraps a
>   multi-statement plain query in. `orion-server migrate --dry-run` lists all
>   three; a failure part-way leaves the earlier ones applied and recorded,
>   which is correct: re-run it.
> - A `CONCURRENTLY` build that dies (connection drop, cancellation) leaves an
>   **`INVALID` index** behind: unused by the planner, still maintained on every
>   write. The migrations are `IF NOT EXISTS`, so a re-run skips an invalid
>   leftover rather than repairing it. Check for one before re-running:
>
>   ```sql
>   SELECT c.relname FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid
>   WHERE NOT i.indisvalid AND c.relname LIKE 'idx_traces%';
>   ```
>
>   Clear it with `REINDEX INDEX CONCURRENTLY <name>;` (PostgreSQL 12+) or
>   `DROP INDEX CONCURRENTLY <name>;` and re-run the migration.
>
> **MySQL** states `ALGORITHM=INPLACE LOCK=NONE`, so it fails loudly rather
> than locking the table if the engine cannot build the index online, but its
> DDL is not transactional, and it has no `CREATE INDEX IF NOT EXISTS`, so a
> part-way failure needs whichever of the two new indexes exists dropped before
> the re-run. **SQLite** has no online build and needs none: it is single-node
> and the migration runs before the listener binds.

### Two JSON columns were renamed

`workflows.tags` is now `workflows.tags_json`, and `channels.methods` is now `channels.methods_json`. On MySQL, `traces.access_token_hash` narrows from `TEXT` to `char(64)`.

**Nothing changes through the API.** The field names every workflow and channel endpoint accepts and returns are still `tags` and `methods`, and the OpenAPI document is unchanged. You will notice if anything reads Orion's tables directly. That means a Grafana panel over `workflows`, an ETL job, a reporting view, or a hand-maintained restore. Those fail with "column does not exist" the first time they run after the upgrade. Query the new names.

| Backend | Migration | What it does |
|---------|-----------|--------------|
| SQLite | `009_json_column_suffixes` | Two `ALTER TABLE … RENAME COLUMN`. SQLite rewrites dependent triggers and views itself. |
| PostgreSQL | `013_json_column_suffixes` | Drops and recreates `current_workflows` / `current_channels`, renames the two columns, replaces both `enforce_*_active_immutable()` bodies. |
| MySQL | `011_json_column_suffixes` | The same shape, plus dropping and recreating the two `trg_*_active_immutable` triggers. |
| MySQL | `012_narrow_access_token_hash` | `traces.access_token_hash` → `char(64)`. |

The extra work on PostgreSQL and MySQL is not defensive. Both store view target lists and trigger bodies as resolved text. A bare rename leaves them broken **without failing the migration**. PostgreSQL's `current_workflows` would keep publishing a column called `tags` while the table underneath has `tags_json`. Its immutability trigger would start raising `record "old" has no field "tags"` on every update of an active row. MySQL's views would stop resolving at all and its triggers would fail with `Unknown column 'tags' in 'OLD'`.

#### How long it locks

On PostgreSQL a column rename is a catalog update. It does not rewrite the table, so the cost does not scale with row count. What can hurt is *acquiring* the `ACCESS EXCLUSIVE` lock behind a long-running transaction on `workflows` or `channels`. The whole file runs in one transaction, so it lands whole or not at all. On MySQL the rename is metadata-only, but `TEXT` → `char(64)` requires `ALGORITHM=COPY`: a full rebuild of `traces` with writes blocked for the duration. On a 1.0.0 install `traces` is empty and this is immediate; against a large trace backlog, size the window or trim it first with `trace_queue.retention_hours`.

**If it fails.** Take a backup first — the same advice this section already
gives for `004_bigint_columns`. On PostgreSQL, DDL is transactional and the file carries no `-- no-transaction` marker. A failed run leaves the schema untouched and no ledger row: fix what blocked it and start again. On MySQL, every DDL statement commits implicitly. An interrupted `011` can therefore leave the columns renamed, the views and triggers not yet recreated, and nothing recorded. Put the two columns back and let it re-run from the top:

```sql
ALTER TABLE `workflows` RENAME COLUMN `tags_json` TO `tags`;
ALTER TABLE `channels`  RENAME COLUMN `methods_json` TO `methods`;
```

Every other statement in the file is `IF EXISTS`-guarded, or a `CREATE` after a matching `DROP`. Once the columns are back the migration is re-runnable, and `012` is a no-op on re-run. On SQLite, restore the file from your backup.

### A production cluster may not migrate at boot

This is now enforced, not advised. With `environment` starting `prod`, `cluster.enabled = true` together with `storage.auto_migrate = true` is a config error and the server refuses to start:

```
Error: Configuration error: cluster.enabled = true with storage.auto_migrate = true
in production: every replica would migrate at boot and race the others. Set
storage.auto_migrate = false (ORION_STORAGE__AUTO_MIGRATE=false) and run
`orion-server migrate` as a deploy step …
```

It is raised during config validation, before anything opens a connection, so `orion-server validate-config` reports it before a rollout. Previously this pairing only warned — from a log line emitted *after* the migration it warns about. The guardrail fired after the race it existed to prevent.

Set `storage.auto_migrate = false` and run `orion-server migrate` as a deploy step. The Helm chart already ships a pre-install/pre-upgrade Job (`migrateJob.enabled`, on by default), and `docker-compose.ha.yml` a one-shot `migrate` service. Both reference topologies are already in the safe shape and need no change. With `auto_migrate = false`, a replica whose schema is behind refuses to start rather than serving against a schema it does not understand.

**Unaffected:** single-node installs, and non-production clusters, which keep the warning. Cluster mode is off by default, and migrating at boot is what makes the single binary self-installing. That exemption is what lets the chart's `devStack` demo run cluster mode without a migrate Job, since its database is created by the same release.

Preview what runs before committing:

```bash
orion-server migrate --dry-run
```

---

## Related

- [Upgrade to 1.0.0](./index.md): the checklist, and every other break.
- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored ones.
- [Releases](./index.md): what changed in each version.
