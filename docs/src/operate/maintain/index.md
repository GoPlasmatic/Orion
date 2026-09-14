<!-- description: Maintain an Orion instance over time: promote definitions between environments, back up and restore, upgrade, and diagnose a live problem by symptom. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Maintain and recover

Four pages for the life of an instance after go-live. The first three are procedures you plan; the fourth is for the day something is wrong.

- [Promote between environments](./promotion.md) moves a service as one versioned package through `export`, `lint`, `plan`, `apply` and `diff`, and explains what a mid-apply failure leaves behind.
- [Back up and restore](./backup-restore.md) covers the SQLite backup endpoint, the offline restore procedure, and what PostgreSQL and MySQL rely on instead.
- [Upgrade an instance](./upgrades.md) is the order that works for a version change: back up, preflight, validate the config, migrate, roll the fleet.
- [Troubleshooting](./troubleshooting.md) is indexed by symptom: what you see, why it happens, what to do, and how to confirm the fix.
