# Orion vs Rule Engines

> **In one line.** A rule engine works out the firing order itself and re-fires
> rules as new facts appear. Orion runs the steps in the order you wrote, each
> one seeing what the last produced, once. If deciding that order is the hard
> part of your problem, you want the engine. If the decision is about one
> payload, Orion is simpler to write, review, version and roll back.

<div class="compare-meta">

**How it relates:** Overlaps, on conditions over one payload

**Where they overlap:** both evaluate declarative conditions over structured data

**Last reviewed:** 2026-08, against Drools 10.2 and OPA 1.19

</div>

## Side by side

|  | Rule engines | Orion |
|---|---|---|
| What it is | An evaluator over a body of rules and facts | A runtime that serves service definitions you send it |
| Unit of work | A rule set evaluated against working memory | A [workflow](../concepts/workflows.md) run against one payload |
| How you write the logic | DRL, Rego, or a decision table | [JSONLogic](../reference/expressions.md), inside a task or a condition |
| Where state lives | Facts in working memory, accumulating | The run's [data context](../reference/workflows.md#the-data-context), which each task adds to and the response ends |
| How a change ships | Rebuild or redistribute the rule set | One API call, hot-reloaded |
| Typical latency / cadence | Sub-millisecond decisions, after the engine is loaded | The whole request, in milliseconds |
| What it needs to run | The engine, embedded or as a sidecar | [One binary](../getting-started/install.md) |

## What rule engines are good at

- **Scale of rules.** Drools' RETE matching stays fast as the rule count grows,
  because it matches incrementally rather than re-evaluating everything.
- **Facts that interact.** A rule firing asserts a new fact, which fires
  another. That inference is the whole point, and Orion has no equivalent.
- **Conflict resolution.** When several rules match, salience and the agenda
  decide the order deterministically.
- **A language built for the job.** Rego expresses authorization policy — over
  hierarchies, sets and partial evaluation — far better than a condition tree.
- **Rules as a governed artifact.** Bundles, versioned separately from any
  service, distributed to many consumers at once.

## What Orion does instead

- Compiles conditions at engine build time, so evaluation is fast and
  deterministic, and a bad expression fails at
  [activation](../concepts/lifecycle.md) rather than at request time.
- Puts the decision *inside* the service: the same workflow validates the
  payload, decides, enriches from a database, and returns the answer.
- Collects rule violations as data with the
  [`validation` function](../reference/functions.md#validation--validate), so a
  decision API can return every failure at once rather than the first.
- Makes the rules easy for an LLM to write, and safe to let it —
  [draft, dry-run, activate, roll back](../build/versioning.md).

## Where they overlap

Both evaluate declarative conditions over structured data, and for a decision
API answering about one payload the overlap is nearly total. Orion is the
smaller thing to run there, because the rules and the endpoint that serves them
are the same artifact.

Nor is the number of rules what separates them. Conditions compile when the
engine builds, and a request reaches one channel and the workflow behind it, so
hundreds of conditions still answer in milliseconds.

The overlap ends at inference, and not where you might expect. State does build
up as a run proceeds: tasks share one data context, and a task's condition can
match on what an earlier task wrote there. What never happens is a rule firing
*again*.

## Choose a rule engine when

- You have hundreds or thousands of rules, and business users maintain them.
- Asserting a fact has to re-trigger rules that already ran, until the set
  settles.
- The same policy must be enforced identically by many services.
- Rule ordering, priority and conflict resolution are part of the specification.
- Authorization policy is the problem, and Rego is built for it.

## Choose Orion when

- The decision is about one payload, and the answer is the response.
- The rules and the endpoint change together and should ship together.
- You want the lookup, the decision and the transformation in one pipeline
  rather than a call out to a decision service and back.
- You can write the order the rules run in, and one forward pass through it
  settles the answer.

## Running both

Keep the engine as the decision service and call it from a workflow with
[`http_call`](../reference/functions.md#http_call) — an OPA sidecar answering
an authorization question is a normal step in an Orion pipeline. Orion handles
ingress, validation, enrichment and the response shape; the engine answers the
one question it is better at.

## What Orion cannot do here

- **No inference.** A task reads what earlier tasks wrote, but nothing re-fires:
  a condition already passed is never reconsidered, so a fact derived late
  cannot change a decision made early. Ordering that correctly is your job.
- **No RETE or incremental matching.** Every condition is evaluated as reached,
  and nothing is shared between two conditions that test the same thing. The
  cost of a request grows with the number of conditions on its path, where a
  RETE engine's does not.
- **No salience, agenda or conflict resolution.** Tasks run in the order you
  wrote them; a condition either passes or it does not.
- **No decision tables** as a first-class artifact, and no rule-authoring UI
  for business users.
- **A fixed operator set.** [The operator tables](../reference/expressions.md)
  are the complete set Orion compiles — not the whole JSONLogic spec, and not
  extensible.
- **No rules as a shared artifact.** Rules live in the workflow that uses them.
  Sharing them across services means a
  [`channel_call`](../reference/functions.md#channel_call) to a workflow that
  owns the decision.

## Related

- [Is Orion Right for You?](../comparison.md) — the chart, and the other neighbours.
- [Expression Language (JSONLogic)](../reference/expressions.md) — every operator, with a test holding the list true.
- [Common Workflow Patterns](../guides/workflow-patterns.md) — exclusive branches and error collection.
- [Author Workflows](../build/workflows.md) — conditions, mapping and validation in practice.
