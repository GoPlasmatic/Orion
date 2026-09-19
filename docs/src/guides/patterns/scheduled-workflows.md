<!-- description: Run a workflow on a schedule with a cron channel — the six-field expression, time zones and DST, misfire policies, non-overlapping runs, and seeing what ran. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-20 -->

# Run work on a schedule

A cron channel binds a schedule to a workflow the way a REST channel binds a route to one. Same workflow, same guards, same traces; the trigger is a clock instead of a caller. Every scheduled instant becomes a durable *occurrence*, a row kept before and after the work, so "did last night's job run?" has an answer.

## Before you start

You need a running Orion server with `cron.enabled` (the default), and an active workflow to run. Anything that works behind a REST channel works here unchanged.

## 1. Write the schedule

A cron channel declares its schedule in `transport_config`. Nothing else about it is special:

```json
{
  "channel_id": "nightly-order-rollup",
  "name": "Nightly order rollup",
  "channel_type": "async",
  "protocol": "cron",
  "workflow_id": "order-rollup",
  "transport_config": {
    "schedule": "0 15 2 * * *",
    "timezone": "Asia/Kolkata",
    "payload": { "window": "previous_day" }
  },
  "config": { "timeout_ms": 1800000 }
}
```

`0 15 2 * * *` is 02:15 every day, read in Kolkata. The expression always has six fields: second, minute, hour, day-of-month, month, day-of-week. Five-field expressions are refused rather than guessed at. The same text means something different under each reading, and no author could tell from the stored document which they got.

Create and activate it like any other channel:

```bash
orion-cli channels create -f nightly-rollup.json
orion-cli channels activate nightly-order-rollup
```

## 2. Read the payload in the workflow

The authored `payload` arrives exactly where an HTTP request body does, so the workflow reads it exactly as it would behind a route:

```json
{
  "id": "parse",
  "function": { "name": "parse_json", "input": { "source": "payload", "target": "input" } }
}
```

That is the point: a workflow moves between a route and a schedule with no change. What the schedule adds is `metadata.trigger`:

```json
{
  "type": "cron",
  "occurrence_id": "01a070e1-5e6a-7552-99d3-66dd70c1feff",
  "scheduled_for": "2026-09-04T20:45:00+00:00",
  "started_at": "2026-09-04T20:45:01.153+00:00",
  "timezone": "Asia/Kolkata",
  "attempt": 1
}
```

`scheduled_for` and `started_at` answer different questions: what the work was *for*, and when it ran. A job that recovers after downtime needs the first, not the second. Read `metadata.trigger.scheduled_for` when the run needs to know which day it is summarizing.

## 3. Choose what happens after downtime

If a node is down at 02:15, that occurrence is a *misfire*. What happens when the scheduler comes back is `misfire_policy`:

| Policy | What runs | Choose it when |
|---|---|---|
| `skip` | Nothing | The work only makes sense at its own time, such as a market-open snapshot |
| `latest` (default) | The newest missed occurrence | One run brings the world up to date: a rebuild, a summary, a sync |
| `catch_up` | Missed occurrences oldest-first, up to `max_catch_up` | Each occurrence does distinct work that still needs doing |

`catch_up` must declare its bound. Without one, a schedule restored after a fortnight floods the engine with a fortnight of work.

Whatever the policy, the misses are recorded as one occurrence with status `skipped_misfire`, carrying the count and the range. A per-second schedule down for a day missed 86,400 of them. Writing 86,400 rows to say so would turn an outage into a second outage. Ordinary polling delay is not a misfire. Anything within `cron.misfire_grace_secs` is late and still runs.

## 4. Stop runs overlapping

By default occurrences may overlap: if the work takes eleven minutes and the schedule fires every ten, two run at once. When that is not acceptable:

```json
{ "transport_config": { "concurrency": { "policy": "forbid" } } }
```

`forbid` admits at most one occurrence for a key at a time, across the whole cluster. A contending occurrence is recorded `skipped_singleton`: visible in the ledger, not silently dropped. A schedule that is consistently outrunning its own work shows up as a growing count rather than as mysterious load.

The key defaults to the channel's id. Naming the same key on several channels serializes them with each other:

```json
{ "concurrency": { "policy": "forbid", "key": "order-pipeline" } }
```

### Run a bounded number at once

A queue-draining worker wants more than one run but fewer than unbounded. Give the key `slots`:

```json
{ "concurrency": { "policy": "forbid", "key": "invoice-worker", "slots": 4 } }
```

At most four runs of `invoice-worker` are admitted at a time, and a fifth is recorded `skipped_singleton`. Each run holds one slot, numbered `0` to `3`, and reads it as `metadata.trigger.singleton_slot`. A workflow can use it to take its own share of the queue, which is what a set of cloned "lane" channels did before. One channel replaces them.

`orion-cli cron status` shows the slots held as `held/slots`. On SQLite the bound is per node. Nodes sharing PostgreSQL or MySQL share it across the cluster.

> [!WARNING]
> Upgrade every node before activating a channel that sets `slots`. An older node refuses the unknown field and quarantines the channel. Any occurrence of it that the node claims fails as `channel_unavailable`. To roll back, remove `slots` first.

> [!NOTE]
> Non-overlap is not exactly once. A worker that loses its lease cancels, but it cannot recall a connector call already in flight. Scheduled work that must not be applied twice needs an idempotent destination or an idempotency key. `metadata.trigger.scheduled_for` is a good one, because every attempt at an occurrence agrees on it.

## 5. Run one now

Testing a schedule by waiting for it is miserable. Trigger it:

```bash
orion-cli channels trigger nightly-order-rollup
```

That creates an occurrence at the current instant and returns at once. It is not a bypass. It takes the same singleton, applies the same guards and writes the same kind of trace, so what you observe is what the schedule does.

To re-attempt one that failed, keep its identity instead:

```bash
orion-cli cron retry 01a070e1-5e6a-7552-99d3-66dd70c1feff
```

A retry is another attempt at the work that was due *then*: same occurrence id, same `scheduled_for`, `attempt` incremented. Re-running finished work is a different thing, and is what `trigger` is for.

## Verify

Read the schedule's state and its ledger:

```bash
orion-cli cron status
orion-cli cron list --channel-id nightly-order-rollup
orion-cli cron get 01a070e1-5e6a-7552-99d3-66dd70c1feff
```

`status` says what is scheduled and when it next fires; `list` says what has been happening; `get` says why one occurrence failed. Each occurrence carries the id of the trace its run wrote, so `orion-cli traces get <id>` shows the tasks. Scheduled runs write `mode = "cron"`.

The signal worth alerting on is `orion_cron_schedule_lag_seconds`: how late occurrences are starting. Every component can be healthy while this climbs, which is why it needs an alert rather than a dashboard. `orion_cron_pending_occurrences` says the same thing from the other side. `/health` reports `components.cron`. It goes `degraded` when the reconciler has not completed a pass for long enough that occurrences are being missed. It also goes `degraded` when the scheduler is off while cron channels are stored active.

## What a cron channel cannot do

Everything about a caller, because there is not one. `auth`, `origin_allow_list`, `rate_limit`, `deduplication`, `cache`, `request`, `response` and `oauth2_login` are all refused at create time rather than stored and quietly ignored. A cron channel is also not reachable over HTTP or by `channel_call`. Running it that way would execute the workflow outside the ledger and outside its lock.

Secrets are refused in `payload`. It is definition content and is recorded as every occurrence's trace input. Read secrets inside the workflow, where the engine resolves them without recording them.

## What "ran once" means

Occurrences are durable and at-least-once. A node that dies mid-run leaves a claim that expires, and a peer re-attempts the same occurrence; you see `attempt: 2`. A `forbid` singleton is non-overlapping for as long as the shared database is reachable. Neither of those makes side effects exactly once. Design the workflow so a second attempt is harmless, and the rest follows.

## Next steps

- [Cron transport](../../reference/channel-config/cron.md): every field, and the DST rules.
- [Cron occurrences](../../reference/admin-api/cron-occurrences.md): the ledger API.
- [Scheduled channels configuration](../../reference/configuration/cron.md): scheduler capacity.
- [Monitor and alert](../../operate/run/monitoring.md): what to alert on.
