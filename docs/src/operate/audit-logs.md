# Audit Logs

Every admin mutation writes an audit row: who did it, what they did, to which
entity, and when. Nothing else writes to that table, and nothing but the
retention job removes from it.

## Read the trail

```bash
curl -s http://localhost:8080/api/v1/admin/audit-logs

curl -s "http://localhost:8080/api/v1/admin/audit-logs?action=status_active&resource_type=workflow"

curl -s "http://localhost:8080/api/v1/admin/audit-logs?resource_id=wf-orders&start_time=2026-07-01T00:00:00Z"
```

Each entry carries the principal, the action, the resource type and id, a
`details` object, and a timestamp.

Filters are applied in the database and combine with AND: `action`,
`resource_type`, `resource_id`, and `principal` match exactly; `start_time`
(inclusive) and `end_time` (exclusive) are RFC 3339; `offset` and `limit` page.

> [!IMPORTANT]
> **An unrecognised parameter is rejected with `400`.** A mistyped filter can
> never come back as an unfiltered `200` that looks like a clean answer. If you
> are scripting against this endpoint, that refusal is the feature — it means a
> compliance query cannot silently widen.

Recorded actions: `create`, `update`, `delete`, `import`, `update_rollout`,
`status_active`, `status_archived`, `status_draft`, `reload`, and `backup`.

## Group a multi-step operation

A promotion is many API calls. Send the same `X-Orion-Change-Context` header on
each one and every row it produces carries it under `details.change_context`:

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/workflows \
  -H 'X-Orion-Change-Context: ticket=OPS-4412' \
  -H 'Content-Type: application/json' --data @workflow.json
```

The value is free-form and truncated at 256 bytes. Use it for a change ticket, a
release name, or an operator's identity — whatever your audit questions are
phrased in.

`orion-server package apply` sets it automatically to
`package=<name>@<version>`, so a promotion's rows filter back into the promotion
that caused them without you doing anything. Imports additionally write one row
per entity written, alongside the batch summary row.

## Bound retention

```toml
[audit]
retention_days = 90            # 0 keeps rows forever
cleanup_interval_secs = 3600
```

The cleanup job runs on its own cadence and deletes rows older than
`retention_days`. In cluster mode it is lease-gated, so one replica performs the
delete. `retention_days = 0` is a legitimate choice for an estate with a
retention obligation — just know that nothing else trims the table.

## Do not lose events

Audit rows are written through a bounded queue, and a full queue **drops** rows
rather than blocking the admin request that produced them:

```toml
[audit]
max_pending = 1000           # rows accepted but not yet written
drain_timeout_secs = 5       # how long shutdown waits for the queue
```

Every drop is counted in `orion_audit_events_dropped_total{reason="queue_full"}`.

> [!WARNING]
> **Alert on that counter existing at all, not on a rate.** A dropped audit
> event is a hole in the trail, and no later query can tell you what was in it.
> Raise `max_pending` if a bursty admin plane — a large import, a fleet-wide
> promotion — is enough to fill it.

`drain_timeout_secs` bounds how long shutdown waits for the queue to empty. It
is rejected at startup if set to `0`: unlike the other timeouts, zero here would
mean "skip the drain" rather than "wait forever", and losing the last few rows
on every restart is not a default worth offering.

## What is not in the audit log

- **Data-plane requests.** Calls to `/api/v1/data/**` are recorded as
  [traces](./traces.md), not audit rows. The audit log is about who changed the
  service, not who used it.
- **Reads.** Listing workflows is not a mutation and does not write a row.
- **The old content of an updated entity.** The audit row names what changed;
  the entity's own version history holds what it was. Both are needed to
  reconstruct a change, which is one more reason active versions are immutable.

## Related

- [Admin API › Audit Logs](../reference/admin-api.md#audit-logs) — the endpoint,
  its parameters, and the response shape.
- [Traces & Async Processing](./traces.md) — the data-plane counterpart.
- [Promote Between Environments](./promotion.md) — where
  `X-Orion-Change-Context` is set for you.
- [Configuration Reference](../reference/configuration.md#audit-log-retention) —
  every `[audit]` key.
