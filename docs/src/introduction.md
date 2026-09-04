<!-- description: Build services at AI speed on a consistent foundation. Define business logic in lightweight JSON; Orion provides the secure, reliable runtime around it. -->
<div class="hero-logo">
  <img src="images/plasmatic-logo.png" alt="Plasmatic Logo">
  <h1>Orion</h1>
  <p>Build services at AI speed — without rebuilding the foundation every time.</p>
  <p class="hero-sub">Describe your business logic in lightweight JSON. Orion makes it a secure, reliable service.</p>
</div>

AI has made writing code incredibly fast. Work that once took weeks can now take an afternoon. But every new service still needs the same essential foundation: routing, security, resilience, connections, deployment controls, and observability. When that foundation is generated from scratch each time, small differences can become production problems.

That is why we built Orion. Orion is a declarative services runtime that gives every service the same strong foundation from the start. You define what the service should do in a lightweight JSON workflow — yourself or with the AI tool of your choice — and connect it to an endpoint with a channel. Post the definitions, activate them, and your API is live. No application server to write, build, or restart.

Orion handles everything around your business logic consistently: route and protocol matching, ingress guards, rate limiting, circuit breaking, fault tolerance, connection pooling, zero-downtime hot reload, and end-to-end observability. You build fast without generating a new version of the basics for every microservice, agent backend, stream processor, or data pipeline.

> **First time here?** [**Install & Run**](./getting-started/install.md) puts a
> server on your machine in a few steps. The next page turns it into a
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

- **No application server to build.** Post a workflow and channel definition and you have a live REST or Kafka endpoint. No Dockerfile or server code for each service.
- **Production features included.** Rate limiting, circuit breakers, timeouts, caching, and payload validation are things you configure on a channel instead of writing.
- **Safe for AI-written logic.** Draft-before-activate, dry-run, percentage rollout, and one-command rollback mean AI output cannot quietly break production.
- **Services that call services.** `channel_call` runs another workflow in-process, so composition costs no network hop and no serialization.
- **One binary, one file.** A single Rust binary with an embedded database — with PostgreSQL or MySQL waiting for when you outgrow that.
- **Published performance record.** Orion 1.0.0 measured **5.1K–5.7K workflow requests/sec** per instance with single-digit millisecond latency under its documented benchmark conditions. Read the [v1.0.0 benchmark record](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md) before applying those results to another version or workload.

## Start here

<div class="doc-cards">

- [**Quickstart: Your First Live API**](./getting-started/quickstart.md)

  Start Orion, deploy a tested service, and call its endpoint.

- [**Choose Your Use Case**](./getting-started/use-cases.md)

  Follow the path for REST, webhooks, Kafka, databases, or AI authoring.

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

- [**Build Orion Services**](./build/index.md)

  The recommended path through workflows, channels, connectors, testing, and rollout.

- [**Operate Orion**](./operate/index.md)

  Docker and Kubernetes, cluster mode, monitoring, promotion between environments.

- [**Reference Index**](./reference/index.md)

  Find APIs, schemas, functions, configuration, metrics, and errors by task.

- [**Orion Console**](./getting-started/console.md)

  The same operations, point-and-click, in the browser.

</div>

## The project

Orion is open source under Apache-2.0 and developed in the open at [GoPlasmatic/Orion](https://github.com/GoPlasmatic/Orion). Questions go to [Discussions](https://github.com/GoPlasmatic/Orion/discussions) and bugs to [Issues](https://github.com/GoPlasmatic/Orion/issues); [Support & Compatibility](./reference/support.md) states what a release guarantees. Using Orion for something? Add yourself to [ADOPTERS.md](https://github.com/GoPlasmatic/Orion/blob/main/ADOPTERS.md) — the usage reports shape the roadmap.
