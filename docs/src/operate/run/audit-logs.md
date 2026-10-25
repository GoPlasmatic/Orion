<!-- description: Every Orion admin mutation writes an audit row: who, what, which entity, and when. How to filter them, and how to label a multi-step change. -->
<!-- type: guide -->
<!-- last_verified: 2026-10-25 -->

# Audit logs

Every admin mutation writes an audit row: who did it, what they did, to which entity, and when. Nothing else writes to that table, and nothing but the retention job removes from it. This guide reads the trail, labels a multi-step operation, and bounds what is kept.

## Before you start

You need an admin credential for the instance, or `orion-cli` configured with one. The endpoint is `GET /api/v2/admin/audit-logs`; the CLI takes the same filters as flags.

## Read the trail

Filter by action, resource, principal or time:

```bash
curl -s http://localhost:8080/api/v2/admin/audit-logs

curl -s "http://localhost:8080/api/v2/admin/audit-logs?action=v2_deployment_apply&resource_type=deployment"

curl -s "http://localhost:8080/api/v2/admin/audit-logs?resource_id=nightly-order-rollup&start_time=2026-07-01T00:00:00Z"
```

The CLI renders a table from the same filters:

```bash
orion-cli audit-logs list

orion-cli audit-logs list --action v2_deployment_apply --resource-type deployment

orion-cli audit-logs list --resource-id nightly-order-rollup --start-time 2026-07-01T00:00:00Z
```

Each entry carries the principal, the action, the resource type and id, a `details` object, and a timestamp. Filters are applied in the database and combine with AND. `action`, `resource_type`, `resource_id` and `principal` match exactly; `start_time` (inclusive) and `end_time` (exclusive) are RFC 3339; `offset` and `limit` page.

> [!NOTE]
> An unrecognized parameter is rejected with `400`. A mistyped filter can never come back as an unfiltered `200` that looks like a clean answer. If you are scripting against this endpoint, that refusal is the feature: a compliance query cannot silently widen.

Because `action` and `resource_type` match exactly, a filter is only as good as the vocabulary behind it. This is all of it:

| `action` | `resource_type` | Written by |
|---|---|---|
| `create` | `backup` | `POST /backups` |
| `reset` | `circuit_breaker` | `POST /connectors/circuit-breakers/{key}` |
| `invalidate` | `cache_namespace` | `POST /cache/namespaces/{namespace}/invalidate`. A workflow's `cache_invalidate` is not an admin action and writes no row |
| `purge`, `requeue` | `trace_dlq` | The [trace DLQ](../../reference/admin-api/trace-dlq.md) endpoints |
| `retry` | `cron_occurrence` | `POST /cron/occurrences/{id}/retry` |
| `cancel` | `cron_occurrence` | `POST /cron/occurrences/{id}/cancel` |
| `trigger` | `channel` | `POST /channels/{id}/trigger` — a manual run of a cron channel |
| `v2_package_upload` | `connector_package` | `POST /connector-packages`; `resource_id` is the package digest |
| `v2_package_admit` | `connector_package` | `POST /connector-packages/{digest}/admit` |
| `v2_package_revoke` | `connector_package` | `POST /connector-packages/{digest}/revoke` |
| `v2_definition_upload` | `definition` | `POST /definitions`; `resource_id` is the definition digest |
| `v2_definition_test` | `definition` | `POST /definitions/test`, before the run; `resource_id` is the digest the workflow definition would have |
| `v2_connection_create` | `connection` | `POST /connections` — one row per connection revision; `resource_id` is `{connection_id}@{revision}` |
| `v2_connection_probe` | `connection` | `POST /connections/{id}/probe` — written before the probe connects, since it reaches a real backend with the connection's credentials; `resource_id` is `{connection_id}@{revision}` |
| `v2_deployment_apply` | `deployment` | `POST /deployments`; `resource_id` is the committed revision |
| `v2_deployment_rollback` | `deployment` | `POST /deployments/rollback`; `resource_id` is `{target_revision}->{committed_revision}` |

Reads are not recorded: only mutations, and `POST /definitions/test`, which reaches live backends and so behaves like one.

## Group a multi-step operation

A deploy is many API calls. Send the same `X-Orion-Change-Context` header on each one, and every row it produces carries it under `details.change_context`:

```bash
curl -s -X POST http://localhost:8080/api/v2/admin/definitions \
  -H 'X-Orion-Change-Context: ticket=OPS-4412' \
  -H 'Content-Type: application/json' --data @definition.json
```

The CLI sends the same header from `--change-context`, or from `ORION_CHANGE_CONTEXT`. Export it once and every command in a deploy script is labelled without touching the script:

```bash
export ORION_CHANGE_CONTEXT='ticket=OPS-4412'
orion-cli deployments apply -d dist/deployment

orion-cli audit-logs list --start-time 2026-07-01T00:00:00Z --output json \
  | jq '.data[] | select((.details | fromjson).change_context == "ticket=OPS-4412")'
```

The value is free-form and truncated at 256 bytes. Use it for a change ticket, a release name, or an operator's identity, whatever your audit questions are phrased in. There is no server-side filter on it. It is stored inside `details`, so narrow with the indexed filters first and match the context client-side, as above.

## Bound retention

Set a retention window; nothing else trims the table:

```toml
[audit]
retention_days = 90            # 0 keeps rows forever
cleanup_interval_secs = 3600
```

The cleanup job runs on its own cadence and deletes rows older than `retention_days`. In cluster mode it is lease-gated, so one replica performs the delete. `retention_days = 0` is a legitimate choice for an estate with a retention obligation; know that nothing else trims the table.

## Do not lose events

Every row goes through a bounded queue, so admin responses never wait on the INSERT. A full queue drops rows rather than blocking the admin request that produced them:

```toml
[audit]
max_pending = 1000           # rows accepted but not yet written
drain_timeout_secs = 5       # how long shutdown waits for the queue
```

Every drop is counted in `orion_audit_events_dropped_total{reason="queue_full"}`.

> [!WARNING]
> Alert on that counter existing at all, not on a rate. A dropped audit event is a hole in the trail, and no later query can tell you what was in it. Raise `max_pending` if a bursty admin plane, such as a fleet-wide deploy, is enough to fill it.

`drain_timeout_secs` bounds how long shutdown waits for the queue to empty. It is rejected at startup if set to `0`. Unlike the other timeouts, zero here would mean "skip the drain" rather than "wait forever". Losing the last few rows on every restart is not a default worth offering.

## Verify

Make a change with a context, then find it:

```bash
orion-cli --change-context "ticket=TEST-1" cache invalidate orders
orion-cli audit-logs list --action invalidate --output json \
  | jq '.data[0].details | fromjson | .change_context'
```

The newest `invalidate` row carries `"ticket=TEST-1"`.

## What is not in the audit log

- **Data-plane requests.** Calls to `/api/v2/data/**` are recorded as [traces](./traces.md), not audit rows. The audit log is about who changed the service, not who used it.
- **Reads.** Listing deployments is not a mutation and does not write a row.
- **The content of a change.** The audit row names what changed. The deployment history holds what each revision deployed, and `GET /deployments/{revision}` reads it back.

## Next steps

- [Admin API › Audit logs](../../reference/admin-api/audit-logs.md): the endpoint, its parameters, and the response shape.
- [Traces and async processing](./traces.md): the data-plane counterpart.
- [Deployment endpoints](../../reference/admin-api/deployments.md): the deploy calls these rows record.
- [Configuration › Audit log retention](../../reference/configuration/audit.md): every `[audit]` key.
