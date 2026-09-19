<!-- description: The cron ledger endpoints: listing and reading occurrences, retrying one at the same scheduled instant, the scheduler status, and manual triggers. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-20 -->

# Cron occurrence endpoints

Reading the durable record of every scheduled run, and starting one by hand.

Every scheduled instant of a [cron channel](../channel-config/cron.md) becomes a durable **occurrence** — created before anything runs, kept after it finishes. It is the answer to "did last night's job run?", and it is deliberately not the trace. Traces are observability and may be sampled, filtered or switched off. The ledger is scheduling correctness state, and is always written.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/cron/occurrences` | List occurrences, newest first, paginated (`?limit=`, `?offset=`). Filter with `?channel_id=`, `?status=`, `?since=`, `?until=`. Summaries only |
| GET | `/api/v1/admin/cron/occurrences/{id}` | One occurrence in full: the failure reason, the trace id, the executing version and the lease detail |
| POST | `/api/v1/admin/cron/occurrences/{id}/retry` | Another attempt at the same occurrence. `409` unless it is `failed`, `skipped_misfire` or `skipped_singleton` |
| GET | `/api/v1/admin/cron/status` | One row per active cron channel: its schedule, its next fire time, its last run, its backlog and its slots |
| POST | `/api/v1/admin/channels/{id}/trigger` | Run an active cron channel now. `202` with the new occurrence |

```bash
# What has this schedule been doing?
curl "http://localhost:8080/api/v1/admin/cron/occurrences?channel_id=nightly-rollup&limit=20" \
  -H "x-api-key: $ORION_API_KEY"

# What is scheduled, and when does it next fire?
curl http://localhost:8080/api/v1/admin/cron/status -H "x-api-key: $ORION_API_KEY"

# Run it now, without waiting for the schedule.
curl -X POST http://localhost:8080/api/v1/admin/channels/nightly-rollup/trigger \
  -H "x-api-key: $ORION_API_KEY"
```

**Statuses.** `pending` means materialised and waiting for a worker. Then come `claimed`, `running`, `completed` and `failed`.

`skipped_misfire` means its time passed while nothing was scheduling. One row summarises a run of them, with the count and range in `error_message`. `skipped_singleton` means every slot of its `concurrency.key` was held by a running occurrence under `policy: "forbid"`.

The field is an open string: tolerate a value you do not know.

**Slots.** A `forbid` occurrence records the key it held as `singleton_key` and the slot as `singleton_slot`, counted from `0`. Two runs of one key can share a `fencing_token` when they hold different slots, so the pair names a hold. Under `allow` both are `null`.

The status row reports the lock too. `concurrency_policy` is `allow` or `forbid`. Under `forbid`, `singleton_key` and `slots` echo the channel's settings. `slots_held` counts the live leases on the key right now, across every channel sharing it. It can exceed `slots` when another channel declares more, or shortly after `slots` was lowered.

**Retry keeps the identity.** The occurrence id and its `scheduled_for` are unchanged and `attempt` increments, because a retry is another attempt at the work that was due *then*. That is what lets a workflow use `metadata.trigger.scheduled_for` as an idempotency key — two attempts at one occurrence agree on it. Re-running finished work is a different thing and has a different endpoint: trigger the channel, which mints a new occurrence at the current instant.

**Triggering is not a side door.** A manual occurrence goes through the same claim, singleton and execution path a scheduled one does. Triggering a `forbid` channel while its scheduled run is in flight is therefore recorded as `skipped_singleton`, not run alongside it. It is an admin mutation: authenticated, rate limited and audited as `trigger` / `channel`.

**Failed occurrences are not retried automatically** and never enter the [trace DLQ](./trace-dlq.md). The next scheduled occurrence is the natural retry, and a deterministically failing job that retried itself would spin. What *is* automatic is crash recovery: an occurrence whose worker died is re-claimed once its lease expires, as a second attempt on the same row.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Cron transport](../channel-config/cron.md): the schedule these occurrences come from.
- [Cron scheduler settings](../configuration/cron.md): the capacity this node reconciles with.
- [Run work on a schedule](../../guides/patterns/scheduled-workflows.md): authoring a schedule and reading its occurrences.
