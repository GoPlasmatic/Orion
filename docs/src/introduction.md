<div class="hero-logo">
  <img src="images/plasmatic-logo.png" alt="Plasmatic Logo">
  <h1>Orion</h1>
  <p>Turn business logic into APIs your AI can write and your ops team can trust.</p>
  <p class="hero-sub">No new service to build. No deploy to wait for.</p>
</div>

Orion is a declarative services runtime. You describe a service as JSON — the
logic and the endpoint it answers on — and send it to a running Orion server
over its API. A second later it is live. Change the JSON and the endpoint
changes with it: no rebuild, no restart, no downtime.

Open a small internal microservice and count the lines. HTTP server setup,
connection pools, a Prometheus exporter, OpenTelemetry wiring, retry loops, a
circuit breaker, health checks, a Dockerfile, a deploy manifest. Somewhere in
the middle sits the logic you actually cared about, and it is maybe fifty lines
long. **Orion runs that middle part and provides everything around it, the same
way, for every service.**

> **First time here?** [**Install & Run**](./getting-started/install.md) puts a
> server on your machine in about a minute. The next page turns it into a
> service.

## What you get

**No service to build:** idea to live REST or Kafka endpoint in seconds. No
Dockerfile, no CI pipeline, no server code.

**Production features included:** rate limiting, circuit breakers, timeouts,
caching, and payload validation on every endpoint. You configure them instead of
writing them.

**Safe for AI-written logic:** models generate JSON reliably. Validation,
draft-before-activate, dry-run, percentage rollout, and one-call rollback mean
AI output cannot quietly break production.

**Measured, not claimed:** **5.1K–5.7K workflow requests/sec** per instance with
single-digit millisecond latency, on the published
[v1.0.0 benchmark record](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md)
— run conditions and all. Built on Tokio and Axum.

**Services that call services:** `channel_call` runs another workflow
in-process, so composition costs no network hop and no serialization.

**One binary, one file:** a single Rust binary with an embedded database.
Nothing to containerize and nothing to provision — with PostgreSQL or MySQL
waiting for when you outgrow that.

## What people build with Orion

<div class="doc-cards">

- [**Replace single-purpose microservices**](./guides/worked-examples.md)

  The pricing rule, the fraud check, the routing table: each becomes a
  workflow and a channel on one instance, still versioned and rolled back
  independently.

- [**Normalize webhooks**](./guides/worked-examples.md#normalizing-webhook-payloads)

  Turn Stripe, GitHub and Shopify payloads into one internal schema without
  a service per provider.

- [**Process Kafka events**](./guides/kafka-channels.md)

  Consume a topic, transform and enrich each record, route the result, and
  give poison messages somewhere safe to land.

- [**Let an AI own the logic**](./ai/claude-code.md)

  Describe the service in a sentence; Claude drafts it, dry-runs it, and
  activates it through the MCP server, inside Orion's lifecycle rules.

</div>

## Start here

<div class="doc-cards">

- [**Install & Run**](./getting-started/install.md)

  A server on your machine in about a minute.

- [**Your First Service**](./getting-started/first-service.md)

  A workflow and a channel, end to end, in four API calls.

- [**How Orion Works**](./concepts/how-orion-works.md)

  Channels, workflows and connectors, in one page.

- [**Is Orion Right for You?**](./comparison.md)

  The neighbouring tools mapped honestly, including where they win.

- [**Build a Service with Claude Code**](./ai/claude-code.md)

  The fastest route from a sentence to a live endpoint.

- [**Run the Examples**](./getting-started/examples.md)

  Deployable packages, from a threshold check to Kafka ingress.

</div>

## Then go deeper

<div class="doc-cards">

- [**Author Workflows**](./build/workflows.md)

  The build estate: workflows, channels, connectors, offline testing, and
  versioned rollout.

- [**Production Checklist**](./operate/production-checklist.md)

  The operate estate: Docker and Kubernetes, cluster mode, monitoring,
  promotion between environments.

- [**Admin API**](./reference/admin-api.md)

  The reference estate: every endpoint, the workflow schema, every task
  function, the full config surface.

- [**The Console (Orion UI)**](./getting-started/console.md)

  The same operations, point-and-click, in the browser.

</div>

## The project

Orion is open source under Apache-2.0 and developed in the open at
[GoPlasmatic/Orion](https://github.com/GoPlasmatic/Orion). Questions go to
[Discussions](https://github.com/GoPlasmatic/Orion/discussions) and bugs to
[Issues](https://github.com/GoPlasmatic/Orion/issues);
[Support & Compatibility](./reference/support.md) states what a release
guarantees. Using Orion for something? Add yourself to
[ADOPTERS.md](https://github.com/GoPlasmatic/Orion/blob/main/ADOPTERS.md) — the
usage reports shape the roadmap.
