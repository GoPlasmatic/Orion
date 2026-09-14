<!-- description: Orion's whole estate lives in one database. SQLite backups via VACUUM INTO, the offline restore procedure, and what PostgreSQL and MySQL rely on instead. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Back up and restore

Orion's whole estate lives in one database: every channel, workflow, connector, trace and audit row. Backing up Orion means backing up that database. There is no restore endpoint, because restoring replaces the database Orion is serving from; it is an offline procedure, and the steps are below.

## Before you start

You need shell access to the host, or to the database, and the ability to stop and start the server. In-product backup covers SQLite only, because that is the backend where the database is a file Orion owns:

| Backend | In-product backup | Restore |
|---------|-------------------|---------|
| SQLite | `POST /api/v1/admin/backups` (`VACUUM INTO`), single node only | Stop the server, replace the file, start it again |
| PostgreSQL | Not provided; use your snapshot and PITR tooling (`pg_dump`, `pg_basebackup`, managed automated backups) | Restore with the same tooling, then start Orion |
| MySQL | Not provided; use your snapshot and PITR tooling (`mysqldump`, binlog PITR, managed backups) | Restore with the same tooling, then start Orion |

This is a deliberate boundary rather than a gap. A managed PostgreSQL already has snapshots and point-in-time recovery that are better than anything Orion could add. Orion has no privileged view of its own storage that would make its version better.

## Back up SQLite

Create a backup and list the ones on this node:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/backups
curl -s http://localhost:8080/api/v1/admin/backups
```

The endpoint runs `VACUUM INTO`, which produces a consistent copy without stopping the server. It writes `storage.backup_dir/orion_backup_<UTC timestamp>.db`, timestamped to the millisecond so two backups in the same second are distinct files.

Bound how many you keep. Backups land on the same disk as the live database, so an unbounded set eventually takes the volume down with it:

```toml
[storage]
backup_dir = "/var/lib/orion/backups"
backup_retention_count = 7    # keep the newest 7; older ones are pruned and the prune is logged
```

Unset, every backup is kept.

> [!WARNING]
> `POST /backups` and `GET /backups` both return `400` in cluster mode. The file would land on one arbitrary replica, and cluster storage is PostgreSQL or MySQL, which `VACUUM INTO` cannot copy. Use the database's own tooling.

Copy backups off the host. A backup on the same disk as the database survives a corrupt write and a bad migration. It does not survive losing the disk.

## Restore SQLite

Stop the server, replace the file, start it again:

```bash
# 1. Stop Orion. SIGTERM drains in-flight requests first.
systemctl stop orion            # or: docker compose stop orion

# 2. Put the backup in place of the live database (the storage.url path).
cp /var/lib/orion/backups/orion_backup_20260727_101500_042.db /var/lib/orion/orion.db

# 3. Start Orion. Migrations run at boot unless storage.auto_migrate = false,
#    in which case run `orion-server migrate` first.
systemctl start orion
```

## Restore PostgreSQL or MySQL

The shape is the same, with your database's tooling in the middle:

1. **Stop every replica.** Restoring underneath a running fleet gives you a fleet serving from a database that is changing beneath it.
2. **Restore the snapshot** with `pg_restore`, `mysql`, or the managed service's point-in-time recovery.
3. **Run `orion-server migrate`** if `storage.auto_migrate = false`, which it should be in a cluster.
4. **Start the replicas.**

Redis needs no backup. Everything in it, such as dedup windows, response caches and rate-limit windows, is ephemeral state that rebuilds itself.

## Verify

Confirm the restored instance came back, and that the estate it holds built:

```bash
curl -sf http://localhost:8080/readyz
curl -s  http://localhost:8080/health | jq '{status, workflows_loaded}'
```

The second check matters more than it looks. `/readyz` says the process is serving; `workflows_loaded` says the estate you restored built. A restored database whose channels quarantine on load passes the first check and fails the second.

## Back up the estate, not only the database

A database backup restores an instance. A package export restores a *service*, into any instance:

```bash
orion-server package export -s https://prod.orion.internal \
  --tag pkg:payments --name payments --version 1.4.0 -o payments-1.4.0.json
```

Keep these in git. They are readable, reviewable and diffable in a way a database dump is not. They are how you rebuild a service on a fresh instance without restoring anything. See [Promote between environments](./promotion.md). The two cover different failures: the database backup is for "this instance broke", the package artifact is for "this service needs to exist somewhere else".

## Next steps

- [Run a cluster](../deploy/cluster.md): why the backup endpoint is refused there.
- [Promote between environments](./promotion.md): package export as the estate-level backup.
- [Upgrade an instance](./upgrades.md): where "back up first" is step one.
- [Configuration › Storage](../../reference/configuration/storage.md): the `[storage]` backup keys.
