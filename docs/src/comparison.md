# Is Orion Right for You?

Orion occupies a specific spot: **declarative business logic served as
governed, high-performance services** — below full workflow orchestrators,
above API gateways, beside (not inside) your application code. This page maps
the neighbors honestly; several of these tools are the better choice for their
home turf, and pairing them with Orion is often the right answer.

## The short answer

| If you need to... | Orion? | Reach for |
|---|:-:|---|
| Turn business rules into live REST/Kafka services | **Yes** | — |
| Let AI generate and manage business logic safely | **Yes** | — |
| Replace a handful of single-purpose microservices | **Yes** | — |
| Orchestrate long-running jobs (hours/days), human approvals | No | Temporal, Airflow, BPMN engines |
| Run a full API gateway with a plugin ecosystem | No | Kong, Envoy (Orion runs happily behind them) |
| A RETE rule engine with complex fact networks | No | Drools |
| Visual drag-and-drop automation | Not yet | n8n, Make — or pair the [CLI](https://github.com/GoPlasmatic/Orion-cli) with your own UI |
| Embed a workflow engine inside your app | No | [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs), the library Orion is built on |
| General-purpose compute (ML, media processing) | No | Your own services / serverless |

## vs. writing another microservice

The baseline alternative. A new microservice means an HTTP server, connection
pools, metrics exporters, tracing integration, retry loops, a Dockerfile, a CI
pipeline, and a deployment — before the first line of business logic. In
Orion the business logic is a JSON document deployed with one API call, and
rate limiting, circuit breakers, validation, versioning, and observability are
runtime guarantees rather than per-service chores.

The trade: your logic must be expressible as a pipeline of Orion's task
functions and JSONLogic. Parse → validate → enrich → call systems → transform
→ respond fits naturally; bespoke algorithms and heavy computation do not
(that's what [`http_call`](./reference/functions.md) to a real service is
for).

## vs. Temporal / Airflow / BPMN engines

Orchestrators own **durable, long-running, stateful** execution: workflows
that sleep for days, survive restarts mid-run, and wait on humans. Orion
workflows are **stateless request pipelines** — they execute in milliseconds
inside a request or event, and durability lives in your databases and topics,
not in the engine.

Choose an orchestrator for saga patterns, human-in-the-loop approvals, and
scheduled batch DAGs. Choose Orion for request-response services and
event-stream processing where those engines' latency and operational weight
are the wrong fit. They compose: a Temporal activity can call an Orion
channel, and an Orion workflow can kick off a Temporal run via `http_call`.

## vs. Kong / Envoy / API gateways

A gateway **proxies and polices** traffic on its way to your services; it
deliberately doesn't implement them. Orion **is** the service: the request
terminates in a workflow that executes your logic. The overlap (rate
limiting, auth, CORS) exists because Orion channels are self-sufficient — but
in front of a fleet, a gateway still earns its place, with Orion as an
upstream that happens to need far fewer of the gateway's crutches.

## vs. Drools and RETE rule engines

Orion evaluates [JSONLogic](https://jsonlogic.com) conditions — compiled at
engine build time, fast, deterministic, and trivially generatable by LLMs.
What it is not: a RETE engine doing incremental matching over a working
memory of thousands of interdependent facts. If your problem is "which of
10,000 rules fire as facts accumulate," use Drools. If it's "does this
request satisfy these conditions, then transform and route it," Orion's model
is simpler to write, review, version, and roll back.

## vs. n8n / Zapier / Make

Visual automation tools optimize for **person-builds-integration-quickly**:
huge app catalogs, drag-and-drop, hosted convenience. Orion optimizes for
**production service traffic**: thousands of requests per second, P99 in
milliseconds, versioned rollouts, circuit breakers, Prometheus metrics — and
an API-first design where definitions are JSON in your repo, not boxes in a
canvas (a visual builder is [on the roadmap](https://github.com/GoPlasmatic/Orion/issues/233)).
If a workflow runs a few times an hour and touches 40 SaaS apps, use the
automation tools. If it *is* one of your services, run it on Orion.

## vs. embedding dataflow-rs

Orion is the **runtime** built on the
[dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) engine. If you want
workflow execution *inside* your own Rust application — no server, no admin
API, no lifecycle management — embed dataflow-rs directly. Orion is what you
deploy when you want the engine plus channels, connectors, versioning,
governance, and an admin API as a standing service.

---

Convinced, or at least curious? [Install & ship your first service](./tutorials/cli-setup.md)
in a couple of minutes.
