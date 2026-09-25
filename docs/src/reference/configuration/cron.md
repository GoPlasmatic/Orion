<!-- description: The [cron] settings: scheduler capacity on this node, poll interval, workers, claim batch and lease, heartbeat, misfire grace and catch-up ceiling. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# Cron scheduler settings

Capacity for `protocol: "cron"` channels. How often this node looks for due work, how much it takes at once, and how long a claim is good for. The *schedules* are not here: each one lives in its channel's [`transport_config`](../channel-config/cron.md).

## Synopsis

```toml
[cron]
enabled = true
poll_interval_ms = 1000
workers = 4
claim_batch_size = 20
claim_lease_secs = 60
heartbeat_interval_secs = 15
misfire_grace_secs = 5
max_catch_up = 100
default_timeout_ms = 3600000
shutdown_timeout_secs = 30
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `cron.enabled` | `true` | `ORION_CRON__ENABLED` | Turn off on a node that should serve requests but run no schedules. See the warning below — this is not a quiet no-op. |
| `cron.poll_interval_ms` | `1000` | `ORION_CRON__POLL_INTERVAL_MS` | The floor on how late a run can be. Raise to cut database chatter on an instance whose finest schedule is hourly. Minimum `100`. |
| `cron.workers` | `4` | `ORION_CRON__WORKERS` | Occurrences this node runs at once. Deliberately separate from `trace_queue.workers` so a catch-up cannot starve `/async` work. |
| `cron.claim_batch_size` | `20` | `ORION_CRON__CLAIM_BATCH_SIZE` | Occurrences claimed per poll. Raise to drain a backlog faster. |
| `cron.claim_lease_secs` | `60` | `ORION_CRON__CLAIM_LEASE_SECS` | How long after a node dies before its in-flight occurrences, and their singleton slots, are recovered by a peer. A running attempt renews its lease every heartbeat, however long its channel's timeout. This is also how long a database outage a running attempt survives. |
| `cron.heartbeat_interval_secs` | `15` | `ORION_CRON__HEARTBEAT_INTERVAL_SECS` | How often a running attempt renews its claim and singleton. Must be **below** `claim_lease_secs`; startup refuses otherwise. |
| `cron.misfire_grace_secs` | `5` | `ORION_CRON__MISFIRE_GRACE_SECS` | How late is "late" rather than "missed". Must cover `poll_interval_ms`, or every occurrence reports a misfire. |
| `cron.max_catch_up` | `100` | `ORION_CRON__MAX_CATCH_UP` | This instance's ceiling on a `catch_up` replay. The effective bound is `min(channel.max_catch_up, this)`. Maximum `1000`. |
| `cron.default_timeout_ms` | `3600000` | `ORION_CRON__DEFAULT_TIMEOUT_MS` | Deadline for an occurrence whose channel sets no `timeout_ms`. A default, not a ceiling: a channel may ask for longer, unlike on Kafka and `/async`. |
| `cron.shutdown_timeout_secs` | `30` | `ORION_CRON__SHUTDOWN_TIMEOUT_SECS` | How long shutdown waits for in-flight occurrences before cancelling them and leaving their claims to expire. |

**`cron.enabled = false` is not a quiet no-op.** An *active* cron channel on a node with the scheduler off is **quarantined**: refused at load, listed under `channels.quarantined` on `/health`, and reported as `components.cron: degraded`. Activating one is refused outright, naming the setting. This is deliberate — a schedule that is stored, active, and silently never fires is the one failure an operator has no way to notice. Drafts, imports, exports and reads are unaffected, so an instance with the scheduler off is still a place to author and promote schedules.

**Sizing the lease.** A running attempt holds its claim and its singleton slot for `claim_lease_secs` at a time, renewed every `heartbeat_interval_secs`, whatever its channel's `timeout_ms`. If its node dies, or shutdown cancels it after `shutdown_timeout_secs`, a peer recovers the occurrence and its slot one lease later. Earlier releases sized the lease from the channel's timeout, so a two-hour channel held its key for two hours after a crash.

A live attempt that cannot renew stops itself before its lease can run out, so a peer that takes over never runs beside it. It stops after `claim_lease_secs` minus one heartbeat without a successful renewal: 45 seconds at the defaults. A database outage longer than that cancels running attempts, and they are attempted again once the database is back. Raise `claim_lease_secs` to ride out longer outages, at the cost of slower recovery from a dead node. To free a dead node's slots at once, cancel its occurrences: `POST /api/v1/admin/cron/occurrences/{id}/cancel`.

## Related

- [Scheduled workflows](../../guides/patterns/scheduled-workflows.md): authoring a schedule and reading its occurrences.
- [Channel configuration › Cron transport](../channel-config/cron.md): where the schedule itself lives.
- [`orion-cli cron`](../cli/orion-cli/cron.md): inspecting and retrying occurrences.
- [Server configuration](./index.md): every section, by what you are configuring.
