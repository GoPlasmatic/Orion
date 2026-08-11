# Architectural Comparison & Use Cases

Orion is a declarative runtime designed for AI agents, workflows, microservice orchestration, event processing, and analytics data pipelines. Operating between edge API gateways and heavy workflow orchestrators, Orion executes request-response pipelines, continuous event streams, and agentic tool invocations without requiring process restarts or binary rebuilds.

This guide outlines where Orion fits within modern system architecture, compares it against alternative tools, and describes key deployment patterns across its five core workload pillars.

## The short answer

| Workload / Requirement | Recommended Tool | Architectural Fit |
|---|:-:|-|
| **AI Agents & MCP Tool Execution** | **Orion** | Safe agent execution engine with staged drafts, MCP tools, and instant rollbacks |
| **Workflow Automation & Logic** | **Orion** | Declarative JSON task pipelines with JSONLogic rules and validation |
| **Microservice Orchestration** | **Orion** | In-process zero-overhead composition via `channel_call`, consolidating microservices |
| **Event & Stream Processing** | **Orion** | High-throughput Kafka consumer groups, event routing, and poison-message isolation |
| **Analytics & Data Pipelines** | **Orion** | Webhook payload normalization and multi-backend data envelope across SQL, Mongo, and ES |
| Browser-based management dashboard | [Orion UI](https://github.com/GoPlasmatic/Orion-ui) | Web management console for Orion Admin API |
| Multi-day stateful sagas, human approvals | Temporal, Airflow | Stateful durable execution engines for long-running processes |
| Full API Gateway with plugin ecosystem | Kong, Envoy | Ingress traffic management (can front Orion instances) |
| Complex RETE rule engine over large fact bases | Drools | Stateful rule evaluation engines |
| In-process workflow library embedded in Rust | [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) | Underlying execution library for custom Rust applications |
| General-purpose compute (ML training, video rendering) | Dedicated Services / Serverless | Custom microservice / serverless environments |

**Optimal Use Cases for Orion:**
- **AI Agent Execution:** Serving as the tool backend an LLM agent calls over HTTP, and — through the MCP server in `orion-cli` — the runtime an assistant authors and operates those tools in.
- **Microservice Orchestration:** Composing internal endpoints in-process without network hops or serialization costs.
- **Workflow & Business Automation:** Expressing complex conditional routing, data transformation, and validation rules as declarative JSON.
- **Stream Processing:** Ingesting and processing continuous event streams from Kafka topics and asynchronous webhooks.
- **Data & Analytics Ingestion:** Normalizing heterogeneous incoming payloads into standardized database models across SQL, MongoDB, Elasticsearch, and Redis.

**Out of Scope for Orion:** Long-running multi-step processes spanning days or requiring human-in-the-loop approvals.

The trade is worth stating plainly: your logic must be expressible as a pipeline
of Orion's task functions and JSONLogic. When it is not, `http_call` to a real
service you wrote is the intended escape hatch.

## vs. Temporal / Airflow / BPMN engines

Orchestrators specialize in **durable, stateful execution**, managing workflows that sleep for extended periods, survive process restarts mid-execution, and await manual intervention.

Orion workflows are **stateless execution pipelines**. They run within milliseconds inside a request loop or event stream, persisting operational state to target datastores or message brokers rather than maintaining complex saga states inside the runtime.

- **Use an Orchestrator** for multi-day saga transactions, manual approval workflows, and scheduled DAG tasks.
- **Use Orion** for AI agent tool execution, synchronous REST endpoints, microservice composition, and high-throughput stream processing.
- **Integration Pattern:** A Temporal activity can invoke an Orion channel endpoint, or an Orion task can trigger a Temporal workflow via an HTTP call.

## vs. Kong / Envoy / API gateways

API gateways focus on **proxying and policing** traffic destined for downstream application services. Orion **implements the execution logic itself**, terminating requests directly within a workflow pipeline.

The overlap — rate limiting, payload validation, deduplication, origin allow-lists — exists because Orion channels police their own ingress. For a fleet, a gateway still earns its place, with Orion as an upstream that needs fewer of the gateway's compensating features.

> [!WARNING]
> **Plan for authentication before you expose a channel.** The admin plane
> authenticates by configuration; a data channel authenticates only if it
> declares an `auth` block (API key or HMAC signature). There is no built-in
> JWT/OIDC verification and no mTLS termination. If your channels are reachable
> by anything you do not control, front them with a gateway, service mesh, or
> reverse proxy — see [Secure an Instance](./operate/security.md) for what to
> configure.

## vs. Drools and RETE rule engines

Orion evaluates [JSONLogic](https://jsonlogic.com) conditions. They are compiled at engine build time: fast, deterministic, and easy for an LLM to write. What they are not is a RETE engine doing incremental matching over a working memory of thousands of interdependent facts.

**Use Drools for:** "which of 10,000 rules fire as facts accumulate".
**Use Orion for:** "does this request satisfy these conditions — then transform and route it". The second model is simpler to write, review, version, and roll back.

## vs. n8n / Zapier / Make

Visual automation tools optimise for building an integration quickly: large app catalogues, drag-and-drop, hosted convenience. Orion optimises for production service traffic: thousands of requests per second, single-digit millisecond latency, versioned rollouts, circuit breakers, Prometheus metrics, and JSON definitions that live in your repository. [Orion UI](https://github.com/GoPlasmatic/Orion-ui) adds a dashboard for managing and visualising them, but the API stays the source of truth.

**Use an automation tool for:** a workflow that runs a few times an hour and touches 40 SaaS apps. **Use Orion for:** a workflow that *is* one of your services.

## vs. embedding dataflow-rs

Orion is the **runtime** built on the [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) engine. If you want workflow execution *inside* your own Rust application — no server, no admin API, no lifecycle management — embed dataflow-rs directly.

Orion is what you deploy when you want that engine plus channels, connectors, versioning, governance, and an admin API as a standing service.

---

Convinced, or at least curious? [Install & Run](./getting-started/install.md)
takes about a minute, and [Your First Service](./getting-started/first-service.md)
is four calls after that.

## Related

- [Install & Run](./getting-started/install.md) — decide by trying it; it takes about a minute.
- [How Orion Works](./concepts/how-orion-works.md) — the mental model, if you want it before the install.
- [Build with Claude Code](./ai/claude-code.md) — hand the authoring to an assistant.
- [Secure an Instance](./operate/security.md) — the authentication planning the gateway section above calls for.
