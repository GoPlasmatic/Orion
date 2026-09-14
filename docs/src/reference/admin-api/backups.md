<!-- description: The two backup endpoints: creating a SQLite backup with VACUUM INTO and listing what is in the backup directory; both refuse in cluster mode. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Backup endpoints

Creating and listing SQLite backups, and why there is no restore endpoint.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/admin/backups` | Create a database backup (SQLite only — `VACUUM INTO` a timestamped file in `storage.backup_dir`) — `400` in cluster mode |
| GET | `/api/v1/admin/backups` | List backup files currently in `storage.backup_dir` — `400` in cluster mode |

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Back up and restore](../../operate/maintain/backup-restore.md): the procedure, per backend.
- [Storage settings](../configuration/storage.md): `backup_dir` and the retention count.
- [Deploy a cluster](../../operate/deploy/cluster.md): why these refuse in cluster mode.
