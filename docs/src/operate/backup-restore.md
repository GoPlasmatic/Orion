# Back Up & Restore

Orion's whole estate — every channel, workflow, connector, trace, and audit row
— lives in one database. Backing up Orion means backing up that database.

> [!IMPORTANT]
> **There is no restore endpoint.** Restoring replaces the database Orion is
> actively serving from, so it is an offline procedure, not an API call. The
> steps are below.

## What is provided, per backend

In-product backup covers **SQLite only**, because that is the backend where the
database is a file Orion owns.

| Backend | In-product backup | Restore |
|---------|-------------------|---------|
| SQLite | `POST /api/v1/admin/backups` (`VACUUM INTO`), single node only | Stop the server, replace the file, start it again |
| PostgreSQL | Not provided — use your snapshot/PITR tooling (`pg_dump`, `pg_basebackup`, managed automated backups) | Restore with the same tooling, then start Orion |
| MySQL | Not provided — use your snapshot/PITR tooling (`mysqldump`, binlog PITR, managed backups) | Restore with the same tooling, then start Orion |

This is a deliberate boundary rather than a gap. A managed PostgreSQL already
has snapshots and point-in-time recovery that are better than anything Orion
could add, and Orion has no privileged view of its own storage that would make
its version better.

## Back up SQLite

```bash
# Create a backup — writes storage.backup_dir/orion_backup_<timestamp>.db
curl -s -X POST http://localhost:8080/api/v1/admin/backups

# List the backups currently on this node
curl -s http://localhost:8080/api/v1/admin/backups
```

The endpoint runs `VACUUM INTO`, which produces a consistent copy without
stopping the server.

**Bound how many you keep.** Backups land on the same disk as the live database,
so an unbounded set eventually takes the volume down with it:

```toml
[storage]
backup_dir = "/var/lib/orion/backups"
backup_retention_count = 7    # keep the newest 7; older ones are pruned and the prune is logged
```

Unset, every backup is kept.

> [!WARNING]
> **`POST /backups` returns `400` in cluster mode.** The file would land on one
> arbitrary replica, and cluster storage is PostgreSQL or MySQL, which
> `VACUUM INTO` cannot copy. Use the database's own tooling.

**Copy backups off the host.** A backup on the same disk as the database
survives a corrupt write and a bad migration. It does not survive losing the
disk.

## Restore SQLite

```bash
# 1. Stop Orion. SIGTERM drains in-flight requests first.
systemctl stop orion            # or: docker compose stop orion

# 2. Put the backup in place of the live database (the storage.url path).
cp /var/lib/orion/backups/orion_backup_20260727_101500.db /var/lib/orion/orion.db

# 3. Start Orion. Migrations run at boot unless storage.auto_migrate = false,
#    in which case run `orion-server migrate` first.
systemctl start orion

# 4. Confirm it came back.
curl -sf http://localhost:8080/readyz
curl -s  http://localhost:8080/health | jq '{status, workflows_loaded}'
```

Step 4 matters more than it looks. `/readyz` says the process is serving;
`workflows_loaded` says the estate you restored actually built. A restored
database whose channels quarantine on load will pass the first check and fail
the second.

## Restore PostgreSQL or MySQL

The shape is the same, with your database's tooling in the middle:

1. **Stop every replica.** Restoring underneath a running fleet gives you a
   fleet serving from a database that is changing beneath it.
2. **Restore the snapshot** with `pg_restore`, `mysql`, or the managed service's
   point-in-time recovery.
3. **Run `orion-server migrate`** if `storage.auto_migrate = false`, which it
   should be in a cluster.
4. **Start the replicas** and check `/readyz` and `/health` on each.

Redis needs no backup. Everything in it — dedup windows, response caches,
rate-limit windows — is ephemeral state that rebuilds itself.

## Back up the estate, not just the database

A database backup restores an instance. A **package export** restores a
*service*, into any instance:

```bash
orion-server package export -s https://prod.orion.internal \
  --tag pkg:payments --name payments --version 1.4.0 -o payments-1.4.0.json
```

Keep these in git. They are readable, reviewable, and diffable in a way a
database dump is not, and they are how you rebuild a service on a fresh instance
without restoring anything. See [Promote Between Environments](./promotion.md).

The two cover different failures: the database backup is for "this instance
broke", the package artifact is for "this service needs to exist somewhere
else".

## Related

- [Cluster Mode & High Availability](./cluster.md) — why the backup endpoint is
  refused there.
- [Promote Between Environments](./promotion.md) — package export as the
  estate-level backup.
- [Upgrades](./upgrades.md) — where "back up first" is step one.
- [Configuration Reference](../reference/configuration.md#storage) — the
  `[storage]` backup keys.
