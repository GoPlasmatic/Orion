# Is Orion Right for You?

Orion is a declarative services runtime. A service is one JSON document holding
the logic, the connectors it reaches, and the endpoint it answers on. Post it to
a running server and it is live a second later, with the rate limiting, retries,
versioning and metrics already around it.

That puts Orion next to a lot of familiar tools without being quite any of them.
Orion is the service itself, not a proxy in front of one and not a coordinator
over several. This page maps the neighbours: what each kind of tool is for, and
how it relates to Orion. The linked pages make the case in full, including where
Orion loses.

## The chart

| What you are weighing | Examples | What it is for | How it relates to Orion |
|---|---|---|---|
| Building it yourself | Spring Boot, FastAPI, Express, Go | A service you compile, deploy and own end to end | **Replaces**, for services that fit a pipeline |
| [Durable execution engines](./compare/durable-execution.md) | Temporal, Restate, Step Functions, Airflow | Work that must survive a restart, or wait hours for a human | **Pairs with**. Orion retries a run from the start, never from where it stopped |
| [API gateways](./compare/api-gateways.md) | Kong, Envoy, APISIX, KrakenD | Policing and routing traffic to the services behind them | **Pairs with**. The services behind it run inside Orion's runtime |
| MCP tool servers | Hand-written MCP servers, FastMCP, LangChain tools | Exposing your systems to an LLM as callable tools | **Replaces**, and adds drafts, rollout and rollback |
| [Automation platforms](./compare/automation-platforms.md) | n8n, Zapier, Make, Node-RED | Wiring SaaS apps together quickly, at low volume | **Different job**. Orion carries production request traffic |
| Stream & integration tools | Camel, NiFi, Redpanda Connect, Flink | Moving and reshaping data between systems continuously | **Overlaps**. Orion handles each record on its own; windowing and engine-managed state are theirs |
| [Rule engines](./compare/rule-engines.md) | Drools, OPA, GoRules | Evaluating many rules over an accumulating fact base | **Overlaps**. In Orion each step feeds the next, in the order you wrote; re-firing rules until they settle is theirs |
| [Embedding dataflow-rs](./compare/dataflow-rs.md) | dataflow-rs | Running workflow tasks inside your own Rust program | **Sits under**. It is the engine Orion wraps |

Four words carry the last column:

- **Replaces.** Orion does this job instead.
- **Pairs with.** Both live in the same estate, each doing its own job.
- **Sits under.** It is a component of Orion, not an alternative to it.
- **Different job.** The overlap is superficial.

> [!WARNING]
> **Plan for authentication before you expose a channel.** The admin plane
> authenticates by configuration; a data channel authenticates only if it
> declares an `auth` block (API key or HMAC signature). There is no built-in
> JWT/OIDC verification and no mTLS termination. If your channels are reachable
> by anything you do not control, front them with a gateway, service mesh, or
> reverse proxy. See [Secure an Instance](./operate/security.md) for what to
> configure.

## Orion is a good fit when

- The logic fits an ordered pipeline: parse, validate, look something up,
  transform, respond.
- You want the endpoint live without a build, a deploy, or a restart.
- You would otherwise write the same rate limiting, retries, metrics and
  versioning for the fifth time.
- An LLM is writing or changing the logic, and you need drafts, dry-runs and
  one-call rollback around it.
- Traffic is request/response or per-record events, answered in milliseconds.
- The logic changes more often than the infrastructure around it does.

## Orion is the wrong tool when

- **The work spans hours or days, or waits for a human.** Orion runs inside a
  request and forgets. See
  [durable execution engines](./compare/durable-execution.md).
- **Something has to *start* on a schedule.** Orion runs when it is called, over
  REST, plain HTTP, or a Kafka topic. There is no timer and no cron.
- **You need gRPC, WebSockets, or a streaming response.** Those three protocols
  are the whole ingress surface.
- **The logic needs a real programming language.** There is no plugin mechanism,
  no scripting runtime and no WASM sandbox.
  [What you can extend](./concepts/how-orion-works.md#what-you-can-extend)
  states the boundary exactly.
- **You need JWT/OIDC or mTLS at the data plane** with nothing in front. See the
  warning above.
- **The request needs heavy computation.** Task functions parse, map, validate
  and talk to other systems. Image processing, model inference and large
  in-memory joins are not what Orion is for.

## The trade you are making

Your logic has to be expressible as a pipeline of Orion's
[task functions](./reference/functions.md) and
[JSONLogic](./reference/expressions.md). That is the whole trade. You give up
writing arbitrary code, and in exchange every service you put on the runtime
gets the same guards, versioning and traces without you writing any of it.

When one step does not fit, the workflow calls out to a service you wrote for
that step. This is a normal outcome rather than a failure of the design, and
everything around that step still gets the versioning, the guards and the
traces.

## Related

- [Install & Run](./getting-started/install.md) — decide by trying it; it takes about a minute.
- [How Orion Works](./concepts/how-orion-works.md) — the mental model, if you want it before the install.
- [Architectural Characteristics](./characteristics.md) — everything the runtime carries, mapped.
- [Build with Claude Code](./ai/claude-code.md) — hand the authoring to an assistant.
- [Secure an Instance](./operate/security.md) — the authentication planning the warning above calls for.
