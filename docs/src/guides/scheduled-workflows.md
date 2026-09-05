<!-- description: Run a workflow on a schedule with a cron channel — the six-field expression, time zones and DST, misfire policies, non-overlapping runs, and seeing what ran. -->
# Run work on a schedule

**Page type:** Guide · **Audience:** Developers and platform operators

A cron channel binds a schedule to a workflow the way a REST channel binds a route to one. Same workflow, same guards, same traces — the trigger is a clock instead of a caller.

Every scheduled instant becomes a durable **occurrence**: a row written before the work starts and kept after it finishes. That is what makes "did last night's job run?" a question with an answer, rather than an inference from whatever traces happen to still exist.

## Before you start

- A running Orion server with `cron.enabled` (the default).
- An active workflow to run. Anything that works behind a REST channel works here unchanged.

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

`0 15 2 * * *` is 02:15 every day, read in Kolkata.

**The expression always has six fields** — second, minute, hour, day-of-month, month, day-of-week. Five-field expressions are refused rather than guessed at, because the same text means something wildly different under each reading and no author could tell from the stored document which they got.

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

That is the point: a workflow moves between a route and a schedule with no change.

What the schedule *adds* is `metadata.trigger`:

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

`scheduled_for` and `started_at` answer different questions — what the work was *for*, and when it actually ran — and a job that recovers after downtime needs the first, not the second. Read `metadata.trigger.scheduled_for` when the run needs to know which day it is summarising.

## 3. Choose what happens after downtime

If a node is down at 02:15, that occurrence is a **misfire**. What happens when the scheduler comes back is `misfire_policy`:

| Policy | What runs | Choose it when |
|---|---|---|
| `skip` | Nothing | The work only makes sense at its own time — a market-open snapshot. |
| `latest` (default) | The newest missed occurrence | One run brings the world up to date: a rebuild, a summary, a sync. |
| `catch_up` | Missed occurrences oldest-first, up to `max_catch_up` | Each occurrence does distinct work that still needs doing. |

`catch_up` must declare its bound. Without one, a schedule restored after a fortnight floods the engine with a fortnight of work.

Whatever the policy, the misses are recorded as **one** occurrence with status `skipped_misfire`, carrying the count and the range. A per-second schedule down for a day missed 86 400 of them, and writing 86 400 rows to say so would turn an outage into a second outage.

Ordinary polling delay is not a misfire. Anything within `cron.misfire_grace_secs` is simply late and still runs.

## 4. Stop runs overlapping

By default occurrences may overlap: if the work takes eleven minutes and the schedule fires every ten, two run at once. Often that is fine. When it is not:

```json
{ "transport_config": { "concurrency": { "policy": "forbid" } } }
```

`forbid` admits at most one occurrence for a key at a time, **across the whole cluster**. A contending occurrence is recorded `skipped_singleton` — visible in the ledger, not silently dropped, so a schedule that is consistently outrunning its own work shows up as a growing count rather than as mysterious load.

The key defaults to the channel's id. Naming the same key on several channels serialises them with each other:

```json
{ "concurrency": { "policy": "forbid", "key": "order-pipeline" } }
```

> **Non-overlap is not exactly-once.** A worker that loses its lease cancels, but it cannot recall a connector call already in flight. Scheduled work that must not be applied twice needs an idempotent destination or an idempotency key — `metadata.trigger.scheduled_for` is a good one, because every attempt at an occurrence agrees on it.

## 5. Watch it

```bash
# What is scheduled, and when does it next fire?
orion-cli cron status

# What has actually been happening?
orion-cli cron list --channel-id nightly-order-rollup

# Why did that one fail?
orion-cli cron get 01a070e1-5e6a-7552-99d3-66dd70c1feff
```

Each occurrence carries the id of the trace its run wrote, so `orion-cli traces get <id>` shows the tasks. Scheduled runs write `mode = "cron"`.

The signal worth alerting on is `orion_cron_schedule_lag_seconds` — how late occurrences are starting. Every component can be healthy while this climbs, which is exactly why it needs an alert rather than a dashboard. `orion_cron_pending_occurrences` says the same thing from the other side.

`/health` reports `components.cron`. It goes `degraded` when the reconciler has not completed a pass for long enough that occurrences are being missed, and — the case worth knowing about — when the scheduler is off while cron channels are stored active.

## 6. Run one now

Testing a schedule by waiting for it is miserable. Trigger it:

```bash
orion-cli channels trigger nightly-order-rollup
```

That creates an occurrence at the current instant and returns immediately. It is not a bypass: it takes the same singleton, applies the same guards and writes the same kind of trace, so what you observe is what the schedule will do.

To re-attempt one that failed, keep its identity instead:

```bash
orion-cli cron retry 01a070e1-5e6a-7552-99d3-66dd70c1feff
```

A retry is another attempt at the work that was due *then* — same occurrence id, same `scheduled_for`, `attempt` incremented. Re-running finished work is a different thing and is what `trigger` is for.

## What a cron channel cannot do

Everything about a caller, because there is not one. `auth`, `origin_allow_list`, `rate_limit`, `deduplication`, `cache`, `request`, `response` and `oauth2_login` are all refused at create time rather than stored and quietly ignored. A cron channel is also **not** reachable over HTTP or by `channel_call` — running it that way would execute the workflow outside the ledger and outside its lock.

Secrets are refused in `payload`: it is definition content and is recorded as every occurrence's trace input. Read secrets inside the workflow, where the engine resolves them without recording them.

## What "ran once" actually means

Occurrences are **durable and at-least-once**. A node that dies mid-run leaves a claim that expires, and a peer re-attempts the same occurrence — you will see `attempt: 2`. A `forbid` singleton is non-overlapping for as long as the shared database is reachable.

Neither of those makes side effects exactly-once. Design the workflow so a second attempt is harmless, and the rest follows.

## Related

- [Cron transport](../reference/channel-config.md#cron-transport) — every field, and the DST rules
- [Cron occurrences](../reference/admin-api.md#cron-occurrences) — the ledger API
- [Scheduled Channels config](../reference/configuration.md#scheduled-channels-cron) — scheduler capacity
- [Monitoring & Alerts](../operate/monitoring.md) — what to alert on
