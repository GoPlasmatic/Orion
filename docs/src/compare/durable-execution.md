<!-- description: Orion vs Temporal, Restate, Step Functions and Airflow: resume-from-failure versus answer-and-forget, why weight is not the distinction, and when to run both. -->
# Orion vs Durable Execution Engines

> **In one line.** A durable execution engine remembers where a run got to, so
> it can survive a crash or wait a week for a human. Orion remembers nothing: a
> run that fails starts again at the first task, if it runs again at all. If the
> work has to outlive the process running it, that is not Orion's job.

<div class="compare-meta">

**How it relates:** Pairs with Orion

**Where they overlap:** both run ordered steps against external systems, and both retry

**Last reviewed:** 2026-08, against Restate 1.7 and Temporal 1.31

</div>

One note before the comparison, because this category is wide. Airflow
schedules DAGs across a cluster; [Restate](https://docs.restate.dev) answers
request-shaped handlers and can reply in milliseconds. "Heavy orchestrator"
describes one end of the category and not the other, so weight is not the
distinction that matters. This is.

## Side by side

|  | Durable execution engines | Orion |
|---|---|---|
| What it is | A runtime that journals every step so a run can resume | A runtime that serves service definitions you send it |
| Unit of work | A run, with an id, resumable | One request, or one Kafka record |
| How you write the logic | Handlers or DAGs in TypeScript, Java, Python or Go | JSON, posted to a running server |
| Where state lives | In the engine's journal — that *is* the product | In the run's data context while it lasts; nothing after it unless you wrote it to a datastore |
| How a change ships | Deploy the code, register the new version | One API call, hot-reloaded, no restart |
| Typical latency / cadence | Milliseconds to days, by design | Milliseconds, bounded by the channel timeout |
| What it needs to run | The engine plus your deployed workers | [One binary](../getting-started/install.md) |

## What durable execution engines are good at

- **Resuming.** A run whose process died picks up at the step it reached, not
  at the beginning. Nothing Orion has is equivalent.
- **Waiting.** Restate's durable `ctx.sleep`, Temporal's timers, and Camunda's
  user tasks let a run pause for minutes or months without holding a
  connection open.
- **Waiting for a *person*.** Restate's awakeables resolve from outside — an
  approval callback that completes a run days later.
- **Keyed state with one writer.** Restate's virtual objects give a run
  persistent per-key state and serialize access to it.
- **Arbitrary logic.** The steps are functions in a language you already use,
  with the debugger, libraries and tests that come with it.

## What Orion does instead

- Answers the request now, and stores the
  [execution trace](../operate/traces.md) rather than a resumable journal.
- Takes the logic as JSON over an API, so a change is
  [a version and a rollout](../build/versioning.md), not a deploy.
- Carries the ingress concerns itself —
  [rate limits, auth, validation, dedup and backpressure](../reference/channel-config.md)
  per channel, with nothing to write.
- Composes in-process: [`channel_call`](../reference/functions.md#channel_call)
  runs another channel's workflow with no network hop.

## Where they overlap

Both run ordered steps against external systems, with retries and timeouts
around each one. For work that finishes inside a single request, that overlap
is real, and Orion is the smaller thing to operate.

Orion is not defenceless about failure either. A [Kafka
channel](../guides/kafka-channels.md) carries an at-least-once guarantee: the
offset is committed only once the record has been processed, so a node that dies
mid-record redelivers it and runs it again.

The overlap ends at *where* it runs again. Orion starts the workflow over from
the first task, because there is no journal to resume from. A sync HTTP request
gets no guarantee at all: it is answered, or it is lost.

## Choose a durable execution engine when

- The process takes hours or days, or sleeps between steps.
- A human has to approve something before the next step runs.
- A partial failure must not be re-executed from the beginning — the money has
  already moved.
- You need to query the state of thousands of in-flight runs.
- The logic is genuinely complex code, not a pipeline of transformations.

## Choose Orion when

- The work finishes inside one request or one record, in milliseconds.
- You want the endpoint and its logic in the same place, with no service to
  build around them.
- The logic changes often and you want each change versioned, rolled out by
  percentage, and reversible with one call.
- An assistant is authoring the logic and you want
  [draft-before-activate](../concepts/lifecycle.md) around it.

## Running both

They compose cleanly in either direction, and this is the common production
shape:

- **The engine calls Orion.** A Temporal activity or a Restate handler posts to
  an Orion channel for the transform-and-enrich step, so that logic stays
  versioned in Orion instead of being redeployed with the worker.
- **Orion calls the engine.** An Orion workflow ends with an
  [`http_call`](../reference/functions.md#http_call) that starts a long-running
  run, and returns the run id to the caller immediately. It is also where a list
  too long to sweep goes — twenty calls fit in a request; a thousand do not.

## What Orion cannot do here

- **No resume.** A redelivered Kafka record runs again from the first task, so
  the steps that already succeeded run twice. Nothing picks up mid-pipeline, and
  a crash loses an in-flight `/async` submission outright.
- **No timers and no schedules.** Orion runs when something calls it — REST,
  plain HTTP, or Kafka. There is no `sleep`, no cron, and no delayed
  invocation.
- **No human-in-the-loop primitive.** There is no awakeable, no task token, and
  no way for an external system to complete a paused run.
- **No per-key state or single-writer guarantees.** A workflow can keep state in
  a datastore, but the runtime holds none between requests and nothing
  serializes two of them touching the same key.
  [Deduplication](../reference/channel-config.md#deduplication) bounds replays
  within a window; it is not keyed state.
- **Fan-out is bounded and in-process.** A workflow
  [`loop`](../reference/workflows.md#loop) repeats the task list once per sweep,
  with a counter you index the array with, so one call per item is a supported
  thing to write. But the sweeps are sequential, they must finish inside the
  channel timeout, and a `max` over
  [`engine.max_loop_iterations`](../reference/configuration.md) is refused when
  you save the workflow. No parallel fan-out, and nothing that outlives the
  request.

## Related

- [Is Orion Right for You?](../comparison.md): the chart, and the other neighbours.
- [Traces & Async Processing](../operate/traces.md): what Orion stores instead of a journal.
- [The Entity Lifecycle](../concepts/lifecycle.md): draft, active, archived, and hot reload.
- [Timeouts, Retries & Circuit Breakers](../operate/failure-handling.md): what Orion re-drives, and what it deliberately does not.
