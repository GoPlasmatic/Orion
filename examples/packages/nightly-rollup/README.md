# Nightly rollup

A workflow that runs on a schedule instead of on a request.

The channel is `protocol: "cron"`: it declares a six-field expression, a time
zone and a fixed payload, and registers no route and no topic. Every scheduled
instant becomes a durable occurrence, so "did last night's job run?" is a
question the ledger answers.

Two things in it are worth copying:

**The workflow reads its payload the ordinary way.** `parse_json` from
`payload`, exactly as it would behind a REST channel. Nothing about this
workflow is schedule-specific, which is the point — it would serve a route
unchanged.

**It summarises the day the *occurrence* is for, not today.** `metadata.trigger.scheduled_for`
is the instant the work was due and is immutable across retries;
`started_at` is when this attempt actually began. A job that recovers after
downtime needs the first. Reading a clock instead would make a delayed run
summarise the wrong day.

`concurrency.policy: "forbid"` means at most one of these runs at a time across
the whole cluster. If the rollup takes longer than a day, the next occurrence is
recorded `skipped_singleton` rather than piling on.

## Deploy it

```bash
./examples/deploy.sh nightly-rollup
```

## Run it now, without waiting until 02:15

```bash
orion-cli channels trigger nightly-order-rollup
orion-cli cron list --channel-id nightly-order-rollup
```

The manual run goes through the same claim, the same singleton and the same
guards a scheduled one does, so what you see is what the schedule will do.

## Watch it

```bash
orion-cli cron status
orion-cli cron get <occurrence-id>
```

See [Run work on a schedule](../../../docs/src/guides/scheduled-workflows.md).
