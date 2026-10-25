<!-- description: Maintain an Orion instance over time: back up and restore it, upgrade it between versions, and diagnose a live problem by its symptom. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Maintain and recover

Three pages for the life of an instance after go-live. The first two are procedures you plan; the third is for the day something is wrong.

- [Back up and restore](./backup-restore.md) covers the SQLite backup endpoint, the offline restore procedure, and what PostgreSQL and MySQL rely on instead.
- [Upgrade an instance](./upgrades.md) is the order that works within Orion 2, and the move from 1.x onto a fresh database.
- [Troubleshooting](./troubleshooting.md) is indexed by symptom: what you see, why it happens, what to do, and how to confirm the fix.
