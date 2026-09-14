<!-- description: Orion vs n8n, Zapier, Make and Node-RED: app catalogue and drag-and-drop speed versus production request traffic, versioning and per-request guarantees. -->
<!-- type: concept -->
<!-- last_verified: 2026-09-14 -->

# Orion and automation platforms

Automation platforms optimize for building an integration in minutes across dozens of SaaS apps. Orion optimizes for a service that answers production traffic all day. Both describe work as a series of steps; almost nothing else about them is the same.

<div class="compare-meta">

**How it relates:** Different job

**Where they overlap:** both describe a pipeline of steps declaratively, and both work through a list

**Last reviewed:** 2026-08, against n8n 2.34

</div>

## Side by side

|  | Automation platforms | Orion |
|---|---|---|
| What it is | A builder and host for cross-app automations | A runtime that serves service definitions you send it |
| Unit of work | A scenario or flow, usually triggered on a schedule or webhook | A [channel](../../concepts/channels.md) answering a request, a Kafka record or a scheduled occurrence |
| How you write the logic | Drag and drop in a browser | JSON, posted to a running server |
| Where state lives | In the platform, with per-run history | In the run's data context while it lasts; nothing after it unless you wrote it to a datastore |
| How a change ships | Save in the editor | One API call, versioned, hot-reloaded |
| Typical latency | Seconds; a few runs an hour to a few thousand a day | Milliseconds; thousands of requests a second |
| What it needs to run | A hosted account, or a container plus its database | [One binary](../install.md) |

## What automation platforms are good at

- **The app catalogue.** Hundreds of pre-built connectors, each with the vendor's OAuth dance, pagination and quirks already handled.
- **Speed to first working thing.** A useful automation in ten minutes, in a browser, with no repository involved.
- **Non-developers.** Someone who never opens a terminal can build and maintain the flow.
- **Triggers of every shape.** Polling, mailbox watchers and form submissions: the ways work starts, beyond HTTP and a clock.
- **Run history as a product feature.** Every execution inspectable in the UI, re-runnable by hand.

## What Orion does instead

- Serves production request traffic: single-digit millisecond responses at [thousands of requests a second](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md).
- Keeps the definition as JSON in your repository, promoted between environments as a [package](../../concepts/packages.md).
- Versions every change, with [percentage rollout and one-command rollback](../../guides/author/versioning.md).
- Brings the production furniture: [circuit breakers](../../operate/run/failure-handling.md), [Prometheus metrics](../../operate/run/monitoring.md), rate limits, and per-request [traces](../../operate/run/traces.md).

The [Orion Console](../../guides/patterns/console.md) adds a browser view for managing and inspecting all of this, but the API stays the source of truth.

## Where they overlap

Both let you describe a sequence of steps without compiling anything, and both receive a webhook and write to a database. For a low-volume internal integration, either works.

Working through a list is shared ground too. A workflow [`loop`](../../guides/patterns/workflow-patterns.md#one-call-per-element-of-an-array) runs the task list once per sweep, so one call per element is a supported thing to write. `continue_on_error` lets the eighth element run after the seventh failed, which is the job n8n's *continue on fail* does.

They diverge on what happens next. An automation platform is built so that flow can be edited by hand tomorrow. Orion is built so that flow can take a thousand requests a second, be reviewed in a pull request, and roll back in one call.

## Choose an automation platform when

- The workflow runs a few times an hour and touches forty SaaS apps.
- The person who owns it is not a developer.
- You need the vendor's OAuth integration for Salesforce or HubSpot and have no interest in building it.
- It matters more that it exists this afternoon than that it is in version control.

## Choose Orion when

- The workflow *is* one of your services, on the critical path of a product.
- It has to answer in milliseconds, under sustained load, with metrics you can alert on.
- The definition belongs in your repository and your CI pipeline.
- You need versioned rollout and rollback rather than "undo in the editor".

## Running both

The clean split is by traffic class, not by capability. Keep the SaaS glue on the automation platform: notify a channel, update a CRM record, chase a spreadsheet. Keep the endpoint your product calls in Orion, and let the automation platform call that endpoint over HTTP when it needs the same logic. The rule that logic lives in exactly one place is worth more than either tool.

## What Orion cannot do here

- **No app catalogue.** There are seven connector types: `http`, `kafka`, `db`, `cache`, `es`, `smtp` and `storage`. Reaching Salesforce means an `http_call` against its API, written by you.
- **No interactive OAuth.** An `http` connector's `oauth2` auth manages the token lifecycle itself, for client-credentials and refresh-token grants. There is no authorization-code dance: a grant that begins in a browser is completed elsewhere, and Orion is seeded with its refresh token.
- **No polling triggers.** A cron channel runs a workflow on a schedule, so "every 15 minutes" is covered. A mailbox watcher or a poll of a SaaS API is not: work still has to arrive as a request, a record or a scheduled instant.
- **No item-based execution model.** A platform runs every node once per input item and shows you the items. An Orion [`loop`](../../reference/workflows.md#loop) is a counter you index the array with: sequential, bounded by a `max` you declare, and finished before the response goes out.
- **No visual builder for authoring.** Workflows are JSON. The Console manages and inspects them; it is not a drag-and-drop canvas.
- **No hosted offering.** You run the binary.
- **No re-run button.** A [trace](../../operate/run/traces.md) records what happened; replaying it means sending the request again.

## Related

- [Is Orion right for you?](./is-orion-right-for-you.md): the chart, and the other neighbours.
- [Orion and durable execution engines](./vs-durable-execution.md): where the long-running work goes instead.
- [Connector types](../../reference/connectors/index.md): the seven types, and how credentials are held.
- [Version and roll out changes](../../guides/author/versioning.md): what "rollback" means here.
- [Use the Orion Console](../../guides/patterns/console.md): the browser view, and what it is for.
