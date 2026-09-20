<!-- description: The `correctness.sql_bind_count` advisory rule, level `deny`, scope workflow: a PostgreSQL statement's `$n` count and its `params` differ. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `correctness.sql_bind_count`

A `deny` rule, scope workflow. A PostgreSQL statement's `$n` placeholders and its literal `params` differ in number.

## Synopsis

```console
$ orion-server clippy ./definitions
deny[correctness.sql_bind_count] a PostgreSQL statement's `$n` placeholders and its literal `params` differ in number
```

## Description

A `db_read` or `db_write` whose statement's highest `$n` differs from the length of its literal `params` array. On PostgreSQL the statement's parameter count is its highest `$n`, and a bind of any other length is refused. The task therefore fails on every execution, which for a scheduled workflow can be hours after deploy. Placeholders are counted by the lexer the handlers use, so a `$1` inside a string, a comment or a dollar-quoted body does not count.

The backend must be proven, because SQLite accepts `$n` too and binds a missing value as `NULL`. Either proof is enough. One is the set's own connector: a literal `postgres://` connection string. The other is the statement itself: a `::` cast or a `$tag$` body, which neither MySQL nor SQLite can parse.

## Caveats

Silent when `params` is computed, and when the statement has no `$n`. Silent when the backend cannot be proven, as with an `env://` connection string and a statement MySQL could also parse. Silent when a number is skipped but the count matches: Orion then binds by value shape and the statement runs. Silent for a statement with an `E'…'` string or a backslash inside a string, where the lexer cannot be sure where the string ends. A `?` placeholder is never counted. A statement kept in a `.sql` file is checked the same way, and the finding names the file.

## Related

- [`orion-server sql check`](../cli/orion-server/sql-check.md): the same count, and the schema and grants, against a real database.

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
