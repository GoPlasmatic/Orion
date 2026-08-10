# Is Orion Right for You?

Orion sits in a specific spot: **your business logic, running as fast,
production-ready APIs**. It lives below full workflow orchestrators, above API
gateways, and beside your application code rather than inside it. This page maps
the neighbours honestly. Several of these tools are the better choice on their
home turf, and pairing them with Orion is often the right answer.

## The short answer

| If you need to... | Orion? | Reach for |
|---|:-:|---|
| Turn business logic into live REST/Kafka services | **Yes** | — |
| Let AI generate and manage business logic safely | **Yes** | — |
| Replace a handful of single-purpose microservices | **Yes** | — |
| Manage services from a browser dashboard | **Yes** | [Orion UI](https://github.com/GoPlasmatic/Orion-ui), the admin dashboard |
| Orchestrate long-running jobs (hours/days), human approvals | No | Temporal, Airflow, BPMN engines |
| Run a full API gateway with a plugin ecosystem | No | Kong, Envoy — Orion runs happily behind them |
| A RETE rule engine with complex fact networks | No | Drools |
| Embed a workflow engine inside your app | No | [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs), the library Orion is built on |
| General-purpose compute (ML, media processing) | No | Your own services / serverless |

**Use Orion for:** request/response services and event-stream processing whose
logic is parse → validate → enrich → call systems → transform → respond.
**Use something else for:** durable multi-day execution, bespoke algorithms, and
heavy computation.

The trade is worth stating plainly: your logic must be expressible as a pipeline
of Orion's task functions and JSONLogic. When it is not, `http_call` to a real
service you wrote is the intended escape hatch.

## vs. Temporal / Airflow / BPMN engines

Orchestrators own **durable, long-running, stateful** execution: workflows that
sleep for days, survive restarts mid-run, and wait on humans. Orion workflows
are **stateless request pipelines**. They execute in milliseconds inside a
request or event, and durability lives in your databases and topics rather than
in the engine.

Choose an orchestrator for saga patterns, human-in-the-loop approvals, and
scheduled batch DAGs. Choose Orion for request-response services and
event-stream processing, where an orchestrator's latency and operational weight
are the wrong fit.

They compose: a Temporal activity can call an Orion channel, and an Orion
workflow can start a Temporal run through `http_call`.

## vs. Kong / Envoy / API gateways

A gateway **proxies and polices** traffic on its way to your services; it
deliberately does not implement them. Orion **is** the service: the request
terminates in a workflow that executes your logic. The overlap — rate limiting,
payload validation, deduplication, origin allow-lists — exists because Orion
channels police their own ingress.

> [!WARNING]
> **Plan for authentication before you expose a channel.** The admin plane
> authenticates by configuration; a data channel authenticates only if it
> declares an `auth` block (API key or HMAC signature). There is no built-in
> JWT/OIDC verification and no mTLS termination. If your channels are reachable
> by anything you do not control, front them with a gateway, service mesh, or
> reverse proxy — see [Security](./features/security.md) for what to configure.
> <!-- TODO(docs2): operate/security.md becomes the owner in Phase 3 (T3.5). -->

For a fleet, a gateway still earns its place — with Orion as an upstream that
needs fewer of the gateway's compensating features.

## vs. Drools and RETE rule engines

Orion evaluates [JSONLogic](https://jsonlogic.com) conditions. They are compiled
at engine build time: fast, deterministic, and easy for an LLM to write. What
they are not is a RETE engine doing incremental matching over a working memory
of thousands of interdependent facts.

**Use Drools for:** "which of 10,000 rules fire as facts accumulate".
**Use Orion for:** "does this request satisfy these conditions — then transform
and route it". The second model is simpler to write, review, version, and roll
back.

## vs. n8n / Zapier / Make

Visual automation tools optimise for building an integration quickly: large app
catalogues, drag-and-drop, hosted convenience. Orion optimises for production
service traffic: thousands of requests per second, single-digit millisecond
latency, versioned rollouts, circuit breakers, Prometheus metrics, and JSON
definitions that live in your repository. [Orion UI](https://github.com/GoPlasmatic/Orion-ui)
adds a dashboard for managing and visualising them, but the API stays the source
of truth.

**Use an automation tool for:** a workflow that runs a few times an hour and
touches 40 SaaS apps. **Use Orion for:** a workflow that *is* one of your
services.

## vs. embedding dataflow-rs

Orion is the **runtime** built on the
[dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) engine. If you want
workflow execution *inside* your own Rust application — no server, no admin API,
no lifecycle management — embed dataflow-rs directly.

Orion is what you deploy when you want that engine plus channels, connectors,
versioning, governance, and an admin API as a standing service.

---

Convinced, or at least curious?
[Install & Run](./getting-started/install.md) takes about a minute, and
[Your First Service](./getting-started/first-service.md) is four calls after
that.
