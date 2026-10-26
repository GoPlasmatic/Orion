# orion-storage

The state database of the [Orion](https://github.com/GoPlasmatic/Orion)
runtime: the `DbPool` over SQLite, PostgreSQL and MySQL, the three embedded
migration sets, the row models and the repositories (traces, the trace DLQ,
audit logs, the cron ledger, cluster state, OAuth2 token state).

This crate is an implementation detail of `orion-server`. It has no release
cycle of its own and nothing in it is covered by semver.
