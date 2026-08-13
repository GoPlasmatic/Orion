# Orion vs Automation Platforms

> **In one line.** Automation platforms optimise for building an integration
> quickly across dozens of SaaS apps. Orion optimises for a service that
> answers production traffic all day. Both describe work as a series of steps;
> almost nothing else about them is the same.

<div class="compare-meta">

**How it relates:** Different job

**Where they overlap:** both describe a pipeline of steps declaratively, and both work through a list

**Last reviewed:** 2026-08, against n8n 2.34

</div>

## Side by side

|  | Automation platforms | Orion |
|---|---|---|
| What it is | A builder and host for cross-app automations | A runtime that serves service definitions you send it |
| Unit of work | A scenario or flow, usually triggered on a schedule or webhook | A [channel](../concepts/channels.md) answering a request or a Kafka record |
| How you write the logic | Drag and drop in a browser | JSON, posted to a running server |
| Where state lives | In the platform, with per-run history | In the run's data context while it lasts; nothing after it unless you wrote it to a datastore |
| How a change ships | Save in the editor | One API call, versioned, hot-reloaded |
| Typical latency / cadence | Seconds; a few runs an hour to a few thousand a day | Milliseconds; thousands of requests a second |
| What it needs to run | A hosted account, or a container plus its database | [One binary](../getting-started/install.md) |

## What automation platforms are good at

- **The app catalogue.** Hundreds of pre-built connectors, each with the
  vendor's OAuth dance, pagination and quirks already handled.
- **Speed to first working thing.** A useful automation in ten minutes, in a
  browser, with no repository involved.
- **Non-developers.** Someone who will never open a terminal can build and
  maintain the flow.
- **Triggers of every shape.** Schedules, polling, mailbox watchers, form
  submissions — the ways work starts, not just HTTP.
- **Run history as a product feature.** Every execution inspectable in the UI,
  re-runnable by hand.

## What Orion does instead

- Serves production request traffic: single-digit millisecond responses at
  [thousands of requests a second](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md).
- Keeps the definition as JSON in your repository, promoted between
  environments as a [package](../concepts/packages.md).
- Versions every change, with
  [percentage rollout and one-command rollback](../build/versioning.md).
- Brings the production furniture: [circuit breakers](../operate/failure-handling.md),
  [Prometheus metrics](../operate/monitoring.md), rate limits, and per-request
  [traces](../operate/traces.md).

[Orion UI](https://github.com/GoPlasmatic/Orion-ui) adds a dashboard for
managing and visualising all of this, but the API stays the source of truth.

## Where they overlap

Both let you describe a sequence of steps without compiling anything, and both
will happily receive a webhook and write to a database. For a low-volume
internal integration, either works.

Working through a list is shared ground too. A workflow
[`loop`](../guides/workflow-patterns.md#one-call-per-element-of-an-array) runs
the task list once per sweep, so one call per element is a supported thing to
write, and `continue_on_error` lets the eighth element run after the seventh
failed — the job n8n's *continue on fail* does.

They diverge on what happens next. An automation platform is built so that flow
can be edited by hand tomorrow; Orion is built so that flow can take a
thousand requests a second, be reviewed in a pull request, and be rolled back
in one call.

## Choose an automation platform when

- The workflow runs a few times an hour and touches forty SaaS apps.
- The person who owns it is not a developer.
- You need the vendor's OAuth integration for Salesforce or HubSpot and have no
  interest in building it.
- It matters more that it exists this afternoon than that it is in version
  control.

## Choose Orion when

- The workflow *is* one of your services, on the critical path of a product.
- It has to answer in milliseconds, under sustained load, with metrics you can
  alert on.
- The definition belongs in your repository and your CI pipeline.
- You need versioned rollout and rollback rather than "undo in the editor".

## Running both

The clean split is by traffic class, not by capability. Keep the SaaS glue —
notify a channel, update a CRM record, chase a spreadsheet — on the automation
platform. Keep the endpoint your product calls in Orion, and let the automation
platform call that endpoint over HTTP when it needs the same logic. The rule
that logic lives in exactly one place is worth more than either tool.

## What Orion cannot do here

- **No app catalogue.** There are five connector types — `http`, `kafka`, `db`,
  `cache`, `es`. Reaching Salesforce means an `http_call` against its API,
  written by you.
- **No OAuth flows.** HTTP connector auth is `bearer`, `basic` or `apikey`.
  There is no authorization-code dance and no token refresh.
- **No schedules or polling triggers.** Orion runs when it is called. There is
  no cron, no mailbox watcher, and no "every 15 minutes".
- **No item-based execution model.** A platform runs every node once per input
  item and shows you the items. An Orion [`loop`](../reference/workflows.md#loop)
  is a counter you index the array with — sequential, bounded by a `max` you
  declare, and finished before the response goes out. No parallel batches, and
  nothing that survives the request.
- **No visual builder for authoring.** Workflows are JSON. The console manages
  and inspects them; it is not a drag-and-drop canvas.
- **No hosted offering.** You run the binary.
- **No re-run button.** A [trace](../operate/traces.md) records what happened;
  replaying it means sending the request again.

## Related

- [Is Orion Right for You?](../comparison.md) — the chart, and the other neighbours.
- [Orion vs Durable Execution Engines](./durable-execution.md) — where the scheduled and the long-running work goes instead.
- [Connector Types](../reference/connectors.md) — the five types, and how credentials are held.
- [Version & Roll Out Changes](../build/versioning.md) — what "rollback" means here.
- [The Console (Orion UI)](../getting-started/console.md) — the browser view, and what it is for.
