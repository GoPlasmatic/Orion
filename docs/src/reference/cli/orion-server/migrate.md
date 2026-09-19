<!-- description: orion-server migrate applies the database migrations without starting the server; --dry-run previews them and --wait waits for the database. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `orion-server migrate`

Runs database migrations against the configured `storage.url` without starting the server.

## Synopsis

```bash
orion-server migrate [--dry-run] [--wait <duration>]
```

## Description

A cluster runs `migrate` as its own deploy step, often while the state database is still starting. Without `--wait`, `migrate` retries the connection for `storage.connect_retry_secs` (60 s by default), the window the server itself boots with. Every failure is retried, and each retry prints a line on stderr.

`--wait` sets the window for this run and narrows what is retried to failures that mean the database is not accepting connections *yet*:

- A refused or reset connection, an unreachable address, or a host name that does not resolve yet is retried.
- A database that is starting up, shutting down or out of connections is retried.
- A wrong password, an unknown database, a TLS failure or a malformed URL stops at once. None of them heals by waiting.

A failed migration is never retried, with or without the flag. SQLite accepts `--wait` and ignores it, because a bad path or permission does not heal by waiting either.

## Options

| Flag | Description |
|------|-------------|
| `--dry-run` | Preview pending migrations without applying them. |
| `--wait <duration>` | Keep retrying the connection for at most this long: `60` (seconds), `30s`, `5m`. `0` is one attempt. Overrides `storage.connect_retry_secs` for this run. |

## Output

Progress goes to stderr, so the migration report on stdout stays unchanged. Each retry prints the reason and the time waited so far:

```console
$ orion-server migrate --wait 60s
waiting for the state database (not accepting connections within 3s) … 3s
waiting for the state database (not accepting connections within 3s) … 7s
Applying 2 migration(s) on postgres...
```

A refused connection reads as `not accepting connections within <n>s`, because the driver retries it inside each attempt for `storage.acquire_timeout_secs`. A progress line never prints the URL.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Migrations applied, or nothing was pending. |
| `1` | The database was still unreachable when the wait ran out (`state database not reachable after <n>s`, followed by the last error), a failure that is not retried, or a failed migration. |

## Examples

```bash
orion-server -c config.toml migrate --dry-run
orion-server -c config.toml migrate --wait 60s
```

A container entrypoint that used to loop around `migrate` reduces to one line:

```bash
orion-server migrate --wait 60s && exec orion-server
```

A binary older than the flag refuses `--wait` as an unexpected argument, so an entrypoint and its image move together.

## Related

- [Server configuration › Storage](../../configuration/storage.md): the `storage.url` the migrations run against.
- [Upgrade an instance](../../../operate/maintain/upgrades.md): where the migration step sits in a rolling upgrade.
- [Deploy with Kubernetes](../../../operate/deploy/kubernetes.md): the pre-upgrade migration Job the chart runs.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
