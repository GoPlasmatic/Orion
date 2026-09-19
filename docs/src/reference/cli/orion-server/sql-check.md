<!-- description: orion-server sql check prepares every db_read and db_write statement of a set against a real database, as the connector's role, executing nothing. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-20 -->

# `orion-server sql check`

Prepares every `db_read` and `db_write` statement of a definition set against a real database, as the role each connector connects as. It catches a schema change that broke a statement, or a missing grant, before the next request does.

## Synopsis

```bash
orion-server [-c config.toml] sql check <dir>
    [--connector NAME=URL]... [--skip-connector NAME]...
    [--schema DIR [--database URL] [--role NAME=ROLE]...]
    [--format text|json]
```

## Description

`lint` sees a statement's shape and never the schema it runs on. `sql check` finds every statement a set ships, task groups included, and prepares it on the database it runs on. Each connector is resolved as the server resolves it: references, `[vars]` from `-c`, and the same endpoint rules. A connector the server would refuse to dial is a finding here.

Nothing is executed. What each backend proves differs, and the report says which:

| Backend | Proves | How |
|---|---|---|
| PostgreSQL 16+ | Schema and the role's grants | A prepare, then `EXPLAIN (GENERIC_PLAN)`, which checks table and column privileges without running the statement |
| PostgreSQL before 16 | Schema | A prepare resolves every table, column and function; grants are checked only at execution |
| MySQL | Schema | A server-side prepare |
| SQLite | Schema | A prepare; SQLite has no roles |

A PostgreSQL session is `READ ONLY` and rolled back, and each statement runs in its own savepoint, so one failure does not hide the rest. A SQLite file is opened read-only and never created. Every failure is reported, not only the first.

A statement whose `params` binds a different number of values than it takes is an error. On SQLite it is a warning, because SQLite binds a missing value as `NULL`. A PostgreSQL `$n` the server cannot infer a type for, as with a skipped `$1`, is a warning. At run time the handler falls back to binding by value.

`lint` runs first. When it reports an error, no statement is checked.

## Options

| Flag | Description |
|------|-------------|
| `<dir>` | A directory of definitions. |
| `--connector NAME=URL` | Check this connector's statements against `URL` instead of its own connection string, as in CI. The endpoint rules do not apply to a URL you typed. |
| `--skip-connector NAME` | Do not check this connector's statements; list them as unchecked, as a warning. A connector that is not in the set must be named here or with `--connector`. |
| `--schema DIR` | Build a scratch schema from the `*.sql` files in `DIR`, applied in filename order, and check against it. Without `--database`, a SQLite schema in memory. |
| `--database URL` | The PostgreSQL server the scratch schema is built on. |
| `--role NAME=ROLE` | With `--schema`: the role a connector's statements run as. It defaults to the user of the connector's URL. |
| `--format json` | One JSON object per finding on stdout, then one `summary` object. |
| `--requires-*`, `--plugin-dir`, `--model-dir` | As `lint` takes them. |
| `-c FILE` (global) | The serving config, for `[vars]` a connector references. |

## A scratch schema

With `--schema DIR --database URL`, the migrations are applied to the PostgreSQL server in **one transaction that is always rolled back**. Every connector's statements are then checked in it, as their role. The migrations may create the roles and grants they check. Nothing persists: not the tables, not the roles.

A migration that would leave the transaction is refused, naming the file, before anything is sent. That covers `COMMIT`, `BEGIN`, `VACUUM`, `CREATE INDEX CONCURRENTLY`, `CREATE DATABASE` and `ALTER SYSTEM`. MySQL commits DDL implicitly, so `--schema` is refused there; point `--connector` at a prepared database instead.

```bash
orion-server sql check ./definitions --schema ./migrations \
  --database "$ADMIN_DATABASE_URL" --role orders-gate=gate
```

## Returns

| Exit code | Meaning |
|---|---|
| `0` | Every statement was checked, with no error. |
| `1` | A statement failed, or a connector could not be resolved, reached or switched to. |
| `2` | The path is not a set, the set has `lint` errors, or a usage error. |

## Examples

```
$ orion-server sql check ./definitions
checking 135 statement(s) on 2 connector(s)
  orders-db    postgres 16.4  role orders  103 ok  grants proven
  orders-gate  postgres 16.4  role gate    30 ok, 2 failed  grants proven
definitions/settle.json:14:22: error: [sql.check] workflow 'settle' task 'claim' at tasks[2].function.input.query: on 'orders-gate' as role 'gate': permission denied for table orders (SQLSTATE 42501)
2 of 135 statement(s) failed
```

## Related

- [`orion-server lint`](./lint.md): the offline gate that runs first.
- [`db_read`](../../functions/db_read.md) and [`db_write`](../../functions/db_write.md): the statements this checks.
- [`correctness.sql_bind_count`](../../clippy/correctness-sql-bind-count.md): the offline half of the parameter-count check.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
