<!-- description: Where Orion fits against durable execution engines, API gateways, automation platforms and rule engines, and the six cases where it is the wrong tool. -->
<!-- type: concept -->
<!-- last_verified: 2026-09-14 -->

# Is Orion right for you?

Orion is a declarative services runtime: a workflow defines a service's logic, a channel defines its entry point, and connectors reach external systems. That puts it next to many familiar tools without being any of them. This page maps the neighbours and states, with the same weight, where Orion is the wrong tool.

Orion is the service itself. It is not a proxy in front of one and not a coordinator over several. The linked pages make each case in full, including where Orion loses.

## The chart

| What you are weighing | Examples | What it is for | How it relates to Orion |
|---|---|---|---|
| Building it yourself | Spring Boot, FastAPI, Express, Go | A service you compile, deploy and own end to end | **Replaces**, for services that fit a pipeline |
| [Durable execution engines](./vs-durable-execution.md) | Temporal, Restate, Step Functions, Airflow | Work that must survive a restart, or wait hours for a human | **Pairs with**. Orion retries a run from the start, never from where it stopped |
| [API gateways](./vs-api-gateways.md) | Kong, Envoy, APISIX, KrakenD | Policing and routing traffic to the services behind them | **Pairs with**. The services behind it run inside Orion's runtime |
| MCP tool servers | Hand-written MCP servers, FastMCP, LangChain tools | Exposing your systems to an LLM as callable tools | **Replaces**, and adds drafts, rollout and rollback |
| [Automation platforms](./vs-automation-platforms.md) | n8n, Zapier, Make, Node-RED | Wiring SaaS apps together in minutes, at low volume | **Different job**. Orion carries production request traffic |
| Stream and integration tools | Camel, NiFi, Redpanda Connect, Flink | Moving and reshaping data between systems continuously | **Overlaps**. Orion handles each record on its own; windowing and engine-managed state are theirs |
| [Rule engines](./vs-rule-engines.md) | Drools, OPA, GoRules | Evaluating many rules over an accumulating fact base | **Overlaps**. In Orion each step feeds the next in the order you wrote; re-firing rules until they settle is theirs |
| [Embedding dataflow-rs](./vs-dataflow-rs.md) | dataflow-rs | Running workflow tasks inside your own Rust program | **Sits under**. It is the engine Orion wraps |

Four words carry the last column:

- **Replaces.** Orion does this job instead.
- **Pairs with.** Both live in the same estate, each doing its own job.
- **Sits under.** It is a component of Orion, not an alternative to it.
- **Different job.** The overlap is superficial.

## Orion is a good fit when

- The logic fits an ordered pipeline: parse, validate, look something up, transform, respond.
- You want the endpoint live without a build, a deploy or a restart.
- You would otherwise write the same rate limiting, retries, metrics and versioning for the fifth time.
- An LLM is writing or changing the logic, and you need drafts, dry runs and one-command rollback around it.
- Traffic is request/response, per-record events, or scheduled runs, answered in milliseconds.
- The logic changes more often than the infrastructure around it does.

## Orion is the wrong tool when

- **The work spans hours or days, or waits for a human.** Orion runs inside a request and forgets. A cron channel can *start* a workflow on a schedule, but nothing can pause one mid-run. See [durable execution engines](./vs-durable-execution.md).
- **You need gRPC, WebSockets or a streaming response.** REST, plain HTTP, Kafka and a cron schedule are the whole ingress surface.
- **The logic needs a real programming language with I/O.** A pure transformation ships as a [plugin](../../concepts/plugins.md): a WebAssembly component in a sandbox with no filesystem, clock, network or secrets. Anything that talks to another system stays an `http_call` to a service you write. [What you can extend](../../concepts/how-orion-works.md#what-you-can-extend) states the boundary exactly.
- **You need full OIDC flows or mTLS at the data plane** with nothing in front. JWT verification is built in; the login redirect dance and client certificates are not.
- **The request needs heavy computation.** Task functions parse, map, validate, talk to other systems, and run an admitted [model](../../concepts/models.md) small enough to pass its probe ceiling. Image processing, training and large in-memory joins are not what Orion is for.
- **You need an app catalogue.** There are seven connector types. Reaching a SaaS API means an `http_call` you write.

> [!WARNING]
> Plan for authentication before you expose a channel. The admin plane authenticates by configuration; a data channel authenticates only if it declares an `auth` block (API key, HMAC signature or JWT). Without one, anything that can reach the port can call the service. [Secure an instance](../../operate/run/security.md) says what to configure.

## The trade you are making

Your logic has to be expressible as a pipeline of Orion's [task functions](../../reference/functions/index.md) and [JSONLogic](../../reference/expressions.md). That is the whole trade. You give up writing arbitrary code. In exchange, every service on the runtime gets the same guards, versioning and traces without you writing any of it.

When one step does not fit, the workflow calls out to a service you wrote for that step. That is a normal outcome, not a failure of the design, and everything around that step still gets the versioning, the guards and the traces.

## Related

- [Install and run Orion](../install.md): decide by running it locally and verifying its health.
- [How Orion works](../../concepts/how-orion-works.md): the mental model, if you want it before the install.
- [Architectural characteristics](../../concepts/architectural-characteristics.md): everything the runtime carries, mapped.
- [Build a service with Claude Code](../../guides/ai/claude-code.md): hand the authoring to an assistant.
- [Secure an instance](../../operate/run/security.md): the authentication planning the warning calls for.
