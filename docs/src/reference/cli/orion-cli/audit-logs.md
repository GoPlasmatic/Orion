<!-- description: orion-cli audit-logs lists admin actions with exact-match filters on action, resource type, resource id, principal and time, applied in the database. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-cli audit-logs`

`list` shows audit log entries of admin actions. Alias: `audit`.

## Synopsis

```bash
orion-cli audit-logs list [filters]
```

## Description

Because the matches are exact, a filter is only as good as the vocabulary behind it. The full `action` × `resource_type` table is in [Audit logs](../../../operate/run/audit-logs.md). An unrecognized filter name is rejected with a `400` rather than answered with unfiltered rows, so a mistyped compliance query cannot silently widen.

Pair it with `--change-context` on the writing side: label a promotion's
commands with `--change-context ticket=OPS-4412`, then read them back as one
operation.

## Options

Filters combine with AND; all of them are applied in the database:

| Flag | Description |
|------|-------------|
| `--action <a>` | Exact match on the action, for example `create`, `status_active`, `update_rollout`. |
| `--resource-type <t>` | Exact match on the resource type: `workflow`, `channel`, `connector`, `engine`, `backup`, `circuit_breaker`, `trace_dlq`, `package`. |
| `--resource-id <id>` | Exact match on the resource id. |
| `--principal <p>` | Exact match on the acting principal — the admin key id, or `anonymous` when admin auth is off. |
| `--start-time <ts>` | Inclusive lower bound on `created_at`, RFC 3339. |
| `--end-time <ts>` | Exclusive upper bound on `created_at`, RFC 3339. |

## Examples

```bash
orion-cli audit-logs list --action status_active --resource-type workflow --start-time 2026-07-01T00:00:00Z
```

## Related

- [Audit logs](../../../operate/run/audit-logs.md): the complete action and resource-type vocabulary.
- [Admin API › Audit logs](../../admin-api/audit-logs.md): the endpoint and its filters.
- [Global flags](./global-flags.md): `--change-context`, the label this command reads back.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
