<!-- description: The transport_config of a cron channel: the six-field schedule, time zone and DST rules, payload, misfire policies, concurrency, and what it may not declare. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-20 -->

# Cron transport

A `cron` channel is started by a clock instead of a caller. It declares its schedule in `transport_config`, which is ordinary definition content. That makes it versioned with the channel, content-hashed, and promoted inside a package.

## Synopsis

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
    "payload": { "window": "previous_day" },
    "misfire_policy": "latest",
    "concurrency": { "policy": "forbid" }
  },
  "config": { "timeout_ms": 1800000 }
}
```

## Description

A cron schedule adds no new top-level field and no fourth entity. It is a channel like any other, with a clock where the caller would be.

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `schedule` | string | yes | — | Six-field cron expression: second, minute, hour, day-of-month, month, day-of-week. Grammar below. |
| `timezone` | string | no | `UTC` | IANA time-zone name the expression's calendar times are read in, for example `Europe/London`. Abbreviations (`IST`, `EST`) are ambiguous and refused. |
| `payload` | object | no | `{}` | The run's input. Must be an object; at most 1 MB serialized. Secrets are refused; see [What a cron channel may not declare](#what-a-cron-channel-may-not-declare). |
| `misfire_policy` | string | no | `latest` | `skip`, `latest`, or `catch_up`. What happens to occurrences whose time passed while nothing was running. |
| `max_catch_up` | number | `catch_up` | — | Bound on a replay, 1–1000. Required when `misfire_policy` is `catch_up`. |
| `concurrency.policy` | string | no | `allow` | `allow` (occurrences may overlap) or `forbid` (at most `slots` per key at a time). |
| `concurrency.key` | string | no | the channel's `channel_id` | Literal lock name, `[A-Za-z0-9][A-Za-z0-9_.-]{0,127}`. Two channels naming the same key share its slots. |
| `concurrency.slots` | integer | no | `1` | How many occurrences of the key may run at once, 1–64. `forbid` only; refused with `allow`. |

Unknown keys are refused, as everywhere else in a channel definition. A misspelled `misfire_polcy` would otherwise leave the default in place forever with nothing to see.

**The payload arrives where a request body does.** A workflow reads it with `parse_json` from `payload`, exactly as it would behind a REST channel:

```json
{ "id": "parse", "function": { "name": "parse_json", "input": { "source": "payload", "target": "input" } } }
```

That is deliberate, and it is what makes a workflow portable between a route and a schedule with no change. What the schedule adds is `metadata.trigger`; see [What the workflow receives](#what-the-workflow-receives).

### What the workflow receives

Beyond the payload, a scheduled run carries a reserved `metadata.trigger` object. It is platform-stamped, never authored:

| Field | Meaning |
|---|---|
| `type` | `cron` for a scheduled run, `manual` for one started by [the trigger endpoint](../admin-api/cron-occurrences.md) |
| `occurrence_id` | The ledger row this run belongs to |
| `scheduled_for` | The UTC instant the work was **due**. Immutable across retries |
| `started_at` | When this attempt actually began |
| `timezone` | The channel's IANA zone, so a workflow formatting a local date need not hard-code it |
| `attempt` | `1` for a first run |
| `singleton_key` | The lock this run holds, when its channel takes one |
| `singleton_slot` | Which of the key's slots it holds, from `0`, when its channel takes one |

`scheduled_for` and `started_at` are different questions and both are answered: the first is what the work is *for*, the second is when it happened. Use `scheduled_for` as an idempotency key — two attempts at one occurrence agree on it, and no two occurrences of a channel share it.

**The expression always has six fields.** `0 15 2 * * *` is 02:15 every day. The same text read as a five-field expression would mean *every minute* between 02:00 and 02:59 on day 15 of the month. No author could see that difference in the stored document. Five-field and seven-field (trailing year) expressions are therefore both refused rather than guessed at.

An expression with no occurrence in the next five years is refused too. `0 0 0 30 2 *` is syntactically perfect and means 30 February.

### Time zones and DST

Calendar times are read in `timezone`; Orion stores the resulting UTC instants. Each occurrence therefore has an immutable `scheduled_for` in UTC, and two rules cover the transitions:

- **A local time that does not exist does not fire.** On a spring-forward day, `0 30 1 * * *` in `Europe/London` fires on the day before and the day after and not on the transition day: 01:30 never happens.
- **A local time that happens twice fires twice.** On a fall-back day the same schedule fires at 01:30 BST and again at 01:30 GMT, an hour apart. They are different instants, so they are different occurrences with different identities.

Both follow from calendar scheduling meaning what a wall clock says. If you want exactly one run regardless, schedule outside 01:00–03:00 local, or use `UTC`.

### Misfire policies

A *misfire* is an occurrence whose scheduled time passed while no healthy scheduler could start it — a node down, a database unreachable. Ordinary polling delay is not a misfire: anything inside `cron.misfire_grace_secs` is late rather than missed, and still runs.

| Policy | What runs | Use when |
|---|---|---|
| `skip` | Nothing. The misses are recorded. | The work only makes sense at its own time. |
| `latest` (default) | The newest missed occurrence. | One run brings the world up to date — a rebuild, a summary, a sync. |
| `catch_up` | The missed occurrences oldest-first, up to `max_catch_up`. | Each occurrence does distinct work that still needs doing. |

Whatever the policy, the misses are recorded as **one** occurrence row with status `skipped_misfire`. It carries the count and the range, not one row per missed instant. A per-second schedule down for a day missed 86 400 of them.

### Concurrency

`policy: "forbid"` means at most `slots` occurrences for a `key` are admitted at a time. `slots` defaults to `1`: one at a time. A contending occurrence is recorded `skipped_singleton` — visible, not dropped. `policy: "allow"` lets occurrences overlap and takes no lock at all.

The key defaults to the channel's `channel_id`, so `forbid` on its own means "one at a time, of this channel". Naming the same key on several channels deliberately shares its slots between them.

**Slots are numbered from `0`, and a run takes the lowest free one.** It holds that slot for the whole attempt and reads it as `metadata.trigger.singleton_slot`. A workflow can partition work by it, the way a set of cloned "lane" channels once did:

```json
"concurrency": { "policy": "forbid", "key": "invoice-worker", "slots": 4 }
```

An occurrence may only take a slot below its own channel's `slots`. Channels sharing a key normally agree. When they do not, a `slots: 1` channel waits for slot `0` however many higher slots are free, and the key's bound is the largest declared. `orion-server lint` warns about the mismatch as `cron.slots_mismatch`.

Lowering `slots` affects new runs only. A run already holding a higher slot finishes normally, so the status view can briefly show more held than the new bound.

**The scope is the database.** On SQLite each node has its own slots. Nodes sharing PostgreSQL or MySQL share them, so the bound holds across the cluster.

**Non-overlap is not exactly once.** A worker that loses its lease cancels, but it cannot prove that a connector call it already made did not land. Scheduled work that must not be applied twice needs an idempotent destination or an idempotency key, exactly as Kafka ingest does.

### What a cron channel may not declare

Everything about a caller, because there is not one:

| Refused | Instead |
|---|---|
| `methods`, `route_pattern`, `topic`, `consumer_group` | Nothing — a cron channel registers no route and no subscription. |
| `config.auth` | There is no caller to authenticate. |
| `config.origin_allow_list` | The check reads an HTTP header a scheduled run does not send. |
| `config.rate_limit` | The schedule already decides how often this runs. |
| `config.deduplication` | Occurrences are unique by `(channel, scheduled_for)` in the ledger, permanently rather than for a window. |
| `config.cache`, `config.request`, `config.response` | There is no request to shape and no reply to cache. |
| `config.oauth2_login` | Both legs are browser redirects. |

Each is refused at create, update and import time rather than stored and ignored. What still applies: `timeout_ms`, `validation_logic`, `backpressure` and `tracing`.

**Secrets are refused in `payload`.** The payload is definition content and is recorded verbatim as every occurrence's trace input. A credential there is a credential at rest in the `traces` table. Read secrets inside the workflow, where the engine resolves them without recording them. `env://`, `vault://`, `secret://` and `var://` strings are refused for the related reason that nothing resolves them here. They would reach the workflow as literal text.

## Related

- [Scheduled workflows](../../guides/patterns/scheduled-workflows.md): authoring a schedule and reading its occurrences.
- [Cron scheduler settings](../configuration/cron.md): the node's capacity for running schedules.
- [`orion-cli cron`](../cli/orion-cli/cron.md): inspecting and retrying occurrences.
- [Admin API › Cron occurrences](../admin-api/cron-occurrences.md): the ledger and the trigger endpoint.
- [Channel configuration](./index.md): every key, with its page.
