# Is Orion Right for You?

Orion **is** the service. A gateway sits in front of it, a durable execution
engine sits above it, and
[dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) runs inside it. You
post JSON to a running server and get a live endpoint, and the runtime brings
the rate limiting, retries, versioning and metrics you would otherwise write
again for every service you own.

This page maps the neighbours. Each row of the chart says what that kind of
tool is for and how it relates to Orion. The linked page makes the case in
full — including where Orion loses.

## Where Orion sits

```orion-diagram
{
  "direction": "LR",
  "nodes": [
    { "id": "gw",    "label": "API gateway",       "sublabel": "in front of Orion",      "type": "gateway" },
    { "id": "orch",  "label": "Durable execution", "sublabel": "above Orion",            "type": "infra" },
    { "id": "orion", "label": "Orion",             "sublabel": "the service itself",     "type": "service" },
    { "id": "df",    "label": "dataflow-rs",       "sublabel": "inside Orion",           "type": "workflow" },
    { "id": "sys",   "label": "Your systems",      "sublabel": "SQL · Mongo · ES · APIs", "type": "datastore" }
  ],
  "edges": [
    { "from": "gw",    "to": "orion" },
    { "from": "orch",  "to": "orion" },
    { "from": "orion", "to": "df" },
    { "from": "orion", "to": "sys" }
  ]
}
```

## The chart

| What you are weighing | Examples | What it is for | How it relates to Orion |
|---|---|---|---|
| Building it yourself | Spring Boot, FastAPI, Express, Go | A service you compile, deploy and own end to end | **Replaces** — for services that fit a pipeline |
| [Durable execution engines](./compare/durable-execution.md) | Temporal, Restate, Step Functions, Airflow | Work that must survive a restart, or wait hours for a human | **Pairs with** — they call Orion, Orion calls them |
| [API gateways](./compare/api-gateways.md) | Kong, Envoy, APISIX, KrakenD | Policing and routing traffic to the services behind them | **Pairs with** — Orion is one of the services behind it |
| MCP tool servers | Hand-written MCP servers, FastMCP, LangChain tools | Exposing your systems to an LLM as callable tools | **Replaces** — and adds drafts, rollout and rollback |
| [Automation platforms](./compare/automation-platforms.md) | n8n, Zapier, Make, Node-RED | Wiring SaaS apps together quickly, at low volume | **Different job** — Orion carries production request traffic |
| Stream & integration tools | Camel, NiFi, Redpanda Connect, Flink | Moving and reshaping data between systems continuously | **Overlaps** — Orion does per-record work, not windowed joins |
| [Rule engines](./compare/rule-engines.md) | Drools, OPA, GoRules | Evaluating many rules over an accumulating fact base | **Overlaps** — at small rule counts |
| [Embedding dataflow-rs](./compare/dataflow-rs.md) | dataflow-rs | Running workflow tasks inside your own Rust program | **Sits under** — it is the engine Orion wraps |

Four words carry the last column:

- **Replaces** — Orion does this job instead.
- **Pairs with** — both live in the same estate, each doing its own job.
- **Sits under** — it is a component of Orion, not an alternative to it.
- **Different job** — the overlap is superficial.

> [!WARNING]
> **Plan for authentication before you expose a channel.** The admin plane
> authenticates by configuration; a data channel authenticates only if it
> declares an `auth` block (API key or HMAC signature). There is no built-in
> JWT/OIDC verification and no mTLS termination. If your channels are reachable
> by anything you do not control, front them with a gateway, service mesh, or
> reverse proxy — see [Secure an Instance](./operate/security.md) for what to
> configure.

## Orion is a good fit when

- The logic fits an ordered pipeline: parse, validate, look something up,
  transform, respond.
- You want the endpoint live without a build, a deploy, or a restart.
- You would otherwise write the same rate limiting, retries, metrics and
  versioning for the fifth time.
- An LLM is writing or changing the logic, and you need drafts, dry-runs and
  one-call rollback around it.
- Traffic is request/response or per-record events — thousands a second,
  answered in milliseconds.
- The payload carries a list, and each element needs a connector call — a few
  dozen of them, not a few thousand.
- The logic changes more often than the infrastructure around it does.

## Orion is the wrong tool when

- **The work spans hours or days, or waits for a human.** Orion runs inside a
  request and forgets. See
  [durable execution engines](./compare/durable-execution.md).
- **Something has to *start* on a schedule.** Orion runs when it is called —
  over REST, plain HTTP, or a Kafka topic. There is no timer and no cron.
- **You need gRPC, WebSockets, or a streaming response.** Those three
  protocols are the whole ingress surface.
- **The logic needs a real programming language.** There is no plugin
  mechanism, no scripting runtime and no WASM sandbox —
  [what you can extend](./concepts/how-orion-works.md#what-you-can-extend)
  states the boundary exactly.
- **You need JWT/OIDC or mTLS at the data plane** with nothing in front. See
  the warning above.
- **You need every last microsecond.** The published record is
  [5.1K–5.7K workflow requests/sec per instance](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/tests/benchmark/results/v1.0.0/SUMMARY.md);
  a hand-written Go service doing the same work will beat it.

## The trade you are making

Your logic must be expressible as a pipeline of Orion's
[task functions](./reference/functions.md) and
[JSONLogic](./reference/expressions.md). The pipeline does not have to run once:
a workflow [`loop`](./reference/workflows.md#loop) repeats the whole task list
once per sweep, so a call per element of a list is a supported thing to write —
sequentially, inside the one request, bounded by a `max` you declare.

When the logic is not expressible that way, `http_call` to a real service you
wrote is the intended escape hatch — and that is a normal outcome, not a
failure of the design. A workflow that reaches outside for one hard step still
gets the versioning, the guards and the traces for everything around it.

## Related

- [Install & Run](./getting-started/install.md) — decide by trying it; it takes about a minute.
- [How Orion Works](./concepts/how-orion-works.md) — the mental model, if you want it before the install.
- [Architectural Characteristics](./characteristics.md) — everything the runtime carries, mapped.
- [Build with Claude Code](./ai/claude-code.md) — hand the authoring to an assistant.
- [Secure an Instance](./operate/security.md) — the authentication planning the warning above calls for.
