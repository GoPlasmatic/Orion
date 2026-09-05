<!-- description: What changes when upgrading Orion to 1.7.0 — cron channels, three expand-only migrations, and the new [cron] section. -->
# Upgrading to 1.7.0

**Page type:** Operations · **Audience:** Platform operators

1.7.0 adds **cron channels**: a fourth channel protocol that runs a workflow on a schedule instead of on a request. Nothing existing changes behaviour. If you do not create a cron channel, the only visible difference is three new tables and a new config section with working defaults.

## Before you start

- Read [Run work on a schedule](../guides/scheduled-workflows.md) if you intend to use the feature.
- Back up as usual ([Back Up & Restore](./backup-restore.md)). The migrations are additive, but a rollback plan is a rollback plan.

## 1. Three migrations, expand-only

One new migration per backend adds `cron_schedule_state`, `cron_occurrences` and `cron_singletons`. Nothing existing is altered or dropped, so a rolling deploy is safe: an older binary ignores the new tables entirely.

```bash
orion-server migrate --dry-run   # see what will apply
orion-server migrate
```

The tables stay empty until a cron channel is activated.

## 2. `[cron]` is on by default, and that is deliberate

The new section defaults to `enabled = true` with a one-second poll. On an instance with no cron channels that costs one indexed query per second and nothing else — the reconciler short-circuits when there is nothing scheduled.

Turning it off is a real choice with a visible consequence:

```toml
[cron]
enabled = false
```

An **active** cron channel on a node with the scheduler off is **quarantined**: refused at load, listed under `channels.quarantined` on `/health`, and reported as `components.cron: degraded`. Activating one is refused outright. That is on purpose — a stored, active schedule that silently never fires is the one failure an operator has no way to notice.

Drafts, imports, exports and reads are unaffected, so an instance with the scheduler off is still a place to author and promote schedules. See [Scheduled Channels](../reference/configuration.md#scheduled-channels-cron) for every setting.

## 3. Occurrences age out with traces

Terminal occurrences are deleted on the `trace_queue.retention_hours` schedule, alongside trace cleanup — one retention decision, not two. An occurrence and the trace its run wrote expire together, so you never read an occurrence whose trace is gone.

Occurrences that are still `pending` are never deleted, however old: that is a backlog, not history.

## 4. New surface, all additive

| Added | Notes |
|---|---|
| `protocol: "cron"` | A fourth `ChannelProtocol`. Older clients that pattern-match on the enum should already tolerate unknown values; `orion-api` deserialization is tolerant by design |
| `mode: "cron"` on traces | An open string, like `kafka`. Filter with `?mode=cron` |
| `GET/POST /api/v1/admin/cron/...` | The occurrence ledger. See [Cron occurrences](../reference/admin-api.md#cron-occurrences) |
| `POST /api/v1/admin/channels/{id}/trigger` | A manual run of a cron channel |
| `orion-cli cron`, `orion-cli channels trigger` | The CLI side of both |
| `components.cron` on `/health` | Present only when the node has something to say about schedules |
| Seven `orion_cron_*` metrics | [Scheduling](../reference/metrics.md#scheduling) |

Nothing is removed and no existing field changes meaning.

## 5. If you deploy in a cluster

Every node runs its own reconciler and workers. Coordination is entirely through the three tables: occurrence identity is `(channel_id, scheduled_for)`, claims are leased with the database clock, and a `forbid` singleton is a row that one occurrence holds at a time. There is no leader to elect and nothing new to configure.

Make sure every node agrees on `cron.enabled`. A mixed cluster will quarantine the channel on the nodes that have it off and run it on the rest — which works, but is not what anyone meant.

## Related

- [Run work on a schedule](../guides/scheduled-workflows.md)
- [Cron transport](../reference/channel-config.md#cron-transport)
- [Cluster Mode & High Availability](./cluster.md)
