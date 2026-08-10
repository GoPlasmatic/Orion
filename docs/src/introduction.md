<div class="hero-logo">
  <img src="images/plasmatic-logo.png" alt="Plasmatic Logo">
  <h1>Orion</h1>
  <p>Turn business logic into APIs your AI can write and your ops team can trust.</p>
  <p class="hero-sub">No new service to build. No deploy to wait for.</p>
</div>

Every piece of business logic tends to turn into its own microservice: a repo, an
HTTP server, a Dockerfile, a CI pipeline, metrics, retries, a deployment — all
before you reach the logic itself.

**Orion works the other way around.** You write the logic as JSON, by hand or by
asking an AI, and send it to a running Orion server. A second later it is a live
API. Change the logic and the endpoint changes with it: no rebuild, no restart,
no downtime.

Everything you would normally build around that logic is already running — rate
limits, retries, caching, metrics, tracing, input validation, versioning, and
rollback.

> **First time here?** [**Install & Run**](./getting-started/install.md) puts a
> server on your machine in about a minute. The next page turns it into a
> service.

## Why Orion?

Open a small internal microservice and count the lines. HTTP server setup,
connection pools, a Prometheus exporter, OpenTelemetry wiring, retry loops, a
circuit breaker, health checks, a Dockerfile, a deploy manifest. Somewhere in
the middle sits the logic you actually cared about, and it is maybe fifty lines
long.

Orion runs that middle part and provides everything around it, the same way, for
every service.

* **⚡ No service to build:** idea to live REST or Kafka endpoint in seconds. No
  Dockerfile, no CI pipeline, no server code.
* **🛡️ Production features included:** rate limiting, circuit breakers,
  timeouts, caching, and payload validation on every endpoint. You configure
  them instead of writing them.
* **🤖 Safe for AI-written logic:** models generate JSON reliably. Validation,
  draft-before-activate, dry-run, percentage rollout, and one-call rollback mean
  AI output cannot quietly break production.
* **🦀 Rust speed:** built on Tokio and Axum. **5,100–5,700 requests/sec** per
  instance (measured, v1.0.0), single-digit millisecond latency, a small memory
  footprint.
* **🧩 Services that call services:** `channel_call` runs another workflow
  in-process, so composition costs no network hop and no serialization.

Orion ships as a single Rust binary with an embedded database. There is nothing
to containerize and nothing to provision.

## Where to start

- **New to Orion?** [Install & Run](./getting-started/install.md), then
  [Your First Service](./getting-started/first-service.md).
- **Deciding whether it fits?** [Is Orion Right for You?](./comparison.md) maps
  the neighbouring tools honestly, including where they win.
- **Want the mental model first?** [How Orion Works](./concepts/how-orion-works.md)
  explains channels, workflows, and connectors in one page.
- **Here to build with an AI assistant?**
  [Build a Service with Claude Code](./ai/claude-code.md) is the fastest route
  from a sentence to a live endpoint.
