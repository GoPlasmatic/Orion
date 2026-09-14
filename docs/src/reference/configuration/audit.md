<!-- description: The [audit] settings: audit log retention and cleanup, the pending-write queue bound, and the shutdown drain timeout, with their defaults. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Audit log settings

The `[audit]` section: how long audit rows are kept, how the cleanup runs, and how the write queue behaves under load and at shutdown.

## Synopsis

```toml
[audit]
retention_days = 90
cleanup_interval_secs = 3600
max_pending = 1000
drain_timeout_secs = 5
```

## Description

**DLQ leases.** A claimed row is leased for `dlq_lease_secs`; when the lease expires another node may re-claim it. That is how work from a crashed node is recovered in cluster mode, so the value should comfortably exceed how long one retry takes.

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `audit.retention_days` | `90` | `ORION_AUDIT__RETENTION_DAYS` | Raise to satisfy a retention policy; `0` keeps rows forever. |
| `audit.cleanup_interval_secs` | `3600` | `ORION_AUDIT__CLEANUP_INTERVAL_SECS` | How often the audit cleanup job runs. |
| `audit.max_pending` | `1000` | `ORION_AUDIT__MAX_PENDING` | Audit rows accepted but not yet written. Raise on a bursty admin plane; a full queue drops rows and counts them in `orion_audit_events_dropped_total{reason="queue_full"}`. |
| `audit.drain_timeout_secs` | `5` | `ORION_AUDIT__DRAIN_TIMEOUT_SECS` | How long shutdown waits for the audit queue to drain before abandoning what is left (and saying how much). Rejected at startup if `0` — unlike the other timeouts on this page, zero is not "no bound" here, it would skip the drain. |

**Audit retention.** Every admin mutation writes an `audit_logs` row and nothing else removes them. `audit.retention_days = 0` therefore grows that table without bound. Before 1.0 these settings lived in `[queue]`, and the cleanup job borrowed the trace job's cadence. They now have their own section and their own interval.

**Audit durability.** Admin responses never wait on the audit INSERT — the row
goes onto a bounded queue that one background writer drains in order. That queue is drained at shutdown, bounded by `audit.drain_timeout_secs`, so a mutation accepted moments before `SIGTERM` is still recorded. Before 1.0 the write was a detached task and that row was lost. Any row that does not make it is counted in `orion_audit_events_dropped_total` and logged at `error`. Alert on that counter being non-zero at all rather than on a threshold.

## Related

- [Audit logs](../../operate/run/audit-logs.md): what an audit row records, and every action.
- [Monitor and alert](../../operate/run/monitoring.md): alerting on dropped audit events.
- [Secure an instance](../../operate/run/security.md): the audit trail as a control.
- [Server configuration](./index.md): every section, by what you are configuring.
