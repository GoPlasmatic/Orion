<!-- description: Orion is a declarative services runtime. A service is one JSON document — logic, connectors, endpoint — live a second after you post it. No rebuild, no restart. -->
<div class="hero-logo">
  <img src="images/plasmatic-logo.png" alt="Plasmatic Logo">
  <h1>Orion</h1>
  <p>The declarative runtime for AI agents, workflows, microservices, and event processing.</p>
  <p class="hero-sub">Safe enough to let an AI write your services. Fast enough to run them in production.</p>
</div>

Orion is a declarative services runtime. A service is one JSON document holding the logic, the connectors it reaches, and the endpoint it answers on. Post it to a running server and it is live a second later. No rebuild, no restart, no downtime.

Everything around that logic is the runtime's job, and it works the same way for every service you put on it: route and protocol matching, ingress guards, rate limiting, circuit breaking, fault tolerance, connection pooling, zero-downtime hot reload, and end-to-end observability. That is the glue you would otherwise write again for every microservice, agent backend, stream processor, and data pipeline.

> **First time here?** [**Install & Run**](./getting-started/install.md) puts a
> server on your machine in about a minute. The next page turns it into a
> service.

## What you can build

Orion carries the same infrastructure across five kinds of service:

<div class="doc-cards">

- [**Microservices**](./guides/worked-examples.md)

  One channel and one workflow make a service, and Orion answers the request in-process — nothing you built sits in the path. Four complete examples here, from an order classifier to a notification router, each deployed with a single command.

- [**AI Agent Tools**](./ai/claude-code.md)

  An agent calls your channels as tools over HTTP. With the [Orion agent skill](./ai/skills.md) and `orion-cli`, an assistant drafts, dry-runs, activates, and rolls back those workflows itself, inside Orion's lifecycle rules.

- [**Business Rules & Decision APIs**](./build/workflows.md)

  Pricing tiers, eligibility checks, routing decisions. Write the rules as JSONLogic conditions over the request, branch between them, and return the result as the response body.

- [**Kafka Event Consumers**](./guides/kafka-channels.md)

  A topic is the ingress: consume records, transform and enrich them as they arrive, publish results onward, and send poison messages to a dead-letter topic instead of letting one stall the partition.

- [**Webhook & Data Ingestion**](./build/connectors.md)

  Normalize payloads from Stripe, GitHub or Shopify, then read and write across PostgreSQL, MySQL, SQLite, MongoDB and Elasticsearch through one portable dialect. Credentials stay on the connector, so the workflow JSON is safe to commit.

</div>

## What you get

- **No service to build.** Post a JSON document and you have a live REST or Kafka endpoint. No Dockerfile, no CI pipeline, no server code.
- **Production features included.** Rate limiting, circuit breakers, timeouts, caching, and payload validation are things you configure on a channel instead of writing.
- **Safe for AI-written logic.** Draft-before-activate, dry-run, percentage rollout, and one-command rollback mean AI output cannot quietly break production.
- **Services that call services.** `channel_call` runs another workflow in-process, so composition costs no network hop and no serialization.
- **One binary, one file.** A single Rust binary with an embedded database — with PostgreSQL or MySQL waiting for when you outgrow that.
- **Measured, not claimed.** **5.1K–5.7K workflow requests/sec** per instance with single-digit millisecond latency, on the published [v1.0.0 benchmark record](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md) — run conditions and all.

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

- [**Build with Claude Code**](./ai/claude-code.md)

  The fastest route from a sentence to a live endpoint.

- [**Example Packages**](./getting-started/examples.md)

  Deployable packages, from a threshold check to Kafka ingress.

</div>

## Then go deeper

<div class="doc-cards">

- [**Authoring Workflows**](./build/workflows.md)

  Workflows, channels, connectors, offline testing, and versioned rollout.

- [**Production Operations**](./operate/production-checklist.md)

  Docker and Kubernetes, cluster mode, monitoring, promotion between environments.

- [**Admin API Reference**](./reference/admin-api.md)

  Every endpoint, the workflow schema, every task function, the full config surface.

- [**Orion Console**](./getting-started/console.md)

  The same operations, point-and-click, in the browser.

</div>

## The project

Orion is open source under Apache-2.0 and developed in the open at [GoPlasmatic/Orion](https://github.com/GoPlasmatic/Orion). Questions go to [Discussions](https://github.com/GoPlasmatic/Orion/discussions) and bugs to [Issues](https://github.com/GoPlasmatic/Orion/issues); [Support & Compatibility](./reference/support.md) states what a release guarantees. Using Orion for something? Add yourself to [ADOPTERS.md](https://github.com/GoPlasmatic/Orion/blob/main/ADOPTERS.md) — the usage reports shape the roadmap.
