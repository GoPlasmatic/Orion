<!-- description: Four Orion services, each described in a sentence and deployed as JSON — with every block pulled from the example packages CI actually deploys and tests. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-09-14 -->

# Worked examples: prompt to service

Four services, each described in a sentence, generated as JSON, and deployed. The JSON on this page is included from the repository's example packages. Every block is the file CI deploys and tests, not a paraphrase of it.

## What you will learn

- What a one-paragraph prompt for an Orion service looks like, and what comes back.
- Task-level conditions as an if/else chain, and why the branches must not overlap.
- How `var` tolerates missing fields, and what that buys a webhook normalizer.
- The `in` operator, and a pipeline where each task adds to one object.

## Before you start

Tested with Orion 1.8.0. You need Git, `curl`, Python 3, a POSIX shell (on Windows, WSL), and an Orion server on `http://localhost:8080`. The examples use the shipped packages, so start from a clone:

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion
```

Each example names the one command that creates its workflow and its channel and activates both. Running the `curl` before that command is the one guaranteed way to get a `404`: the endpoint does not exist until the channel does.

## 1. Tiered order classification

**What you want:** classify orders into tiers, and set a discount per tier.

**The prompt:**

```text
Create an Orion workflow for the "order-tiers" channel that parses the payload
into "order" and assigns a tier from the amount: vip at 500 or more with a 15%
discount, premium from 100 to 500 with 5%, standard below 100 with none.
```

**The workflow:**

```json
{{#include ../../../../examples/packages/order-classification/workflow.json}}
```

**Deploy and call it:**

```bash
./examples/deploy.sh order-classification

curl -s -X POST http://localhost:8080/api/v1/data/order-tiers \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "amount": 750, "product": "Diamond Ring" } }'
```

The response carries the parsed order with `tier` and `discount_pct` added. What this shows is task-level conditions as an if/else chain. The three tier tasks have mutually exclusive conditions, so exactly one runs and the others are recorded as skipped in the trace.

## 2. Range-based sensor alerts

**What you want:** grade sensor readings into severities, and flag the ones that need attention.

**The prompt:**

```text
Create an Orion workflow for the "sensors" channel that parses the payload into
"reading" and sets severity from temperature: critical above 90 or below 0,
warning from 70 to 90, normal otherwise. Set an alert flag for critical and
warning.
```

**The workflow:**

```json
{{#include ../../../../examples/packages/iot-sensor-alert/workflow.json}}
```

**Deploy and call it:**

```bash
./examples/deploy.sh iot-sensor-alert

curl -s -X POST http://localhost:8080/api/v1/data/sensors \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "sensor_id": "SENSOR-42", "temperature": 95 } }'
```

What this shows is `and` and `or` composing a range test. The bands are written to be mutually exclusive. Overlapping ranges would run both tasks, and the later one would win.

## 3. Normalizing webhook payloads

**What you want:** take whatever shape a provider sends and store one shape.

**The prompt:**

```text
Create an Orion workflow for the "webhooks" channel that parses the payload into
"event" and maps provider fields into a common schema, tolerating missing
fields.
```

**The workflow:**

```json
{{#include ../../../../examples/packages/webhook-transform/workflow.json}}
```

**Deploy and call it:**

```bash
./examples/deploy.sh webhook-transform

curl -s -X POST http://localhost:8080/api/v1/data/webhooks \
  -H 'Content-Type: application/json' \
  --data @examples/packages/webhook-transform/request.json
```

What this shows is that `var` is null-safe. A field the provider omitted maps to `null` rather than failing the task, which is what lets one workflow accept several providers' payloads. Send an empty body and the workflow still normalizes.

In production, add authentication. A webhook endpoint reachable by anyone is a webhook endpoint anyone can forge. Use `hmac` mode; see [Authenticate callers](../author/channels.md#authenticate-callers).

## 4. Severity-based notification routing

**What you want:** log everything, email anything above `low`, and text only the urgent ones.

> [!NOTE]
> Nothing is sent. The `email` and `sms` tasks set flags with `map`; no email leaves the process and no SMS is delivered. The example is about the routing decision, which is the part worth version-controlling. To make it real, replace those `map` tasks with `http_call` tasks pointing at an email and an SMS connector. The conditions do not change. See [Connect a database or API](../author/connectors.md).

**The prompt:**

```text
Create an Orion workflow for the "notifications" channel that parses the payload
into "notification", logs everything, emails anything except low severity, and
sends SMS only for high and critical.
```

**The workflow:**

```json
{{#include ../../../../examples/packages/notification-routing/workflow.json}}
```

**Deploy and call it:**

```bash
./examples/deploy.sh notification-routing

curl -s -X POST http://localhost:8080/api/v1/data/notifications \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "message": "Disk usage at 92%", "severity": "high" } }'
```

| Severity | `logged` | `email_sent` | `sms_sent` |
|----------|:-:|:-:|:-:|
| low | Yes | No | No |
| medium | Yes | Yes | No |
| high | Yes | Yes | Yes |
| critical | Yes | Yes | Yes |

What this shows is the `in` operator for set membership. It is also a progressive pipeline, where each task adds to the same object rather than branching away from it.

## Recap

- A prompt names the channel, the parse target, and the rule in plain words; the JSON that comes back is a parse task and a few conditional `map` tasks.
- Branches are separate tasks with conditions that cannot both be true.
- `var` on a missing field is `null`, not an error, so one normalizer serves several providers.
- Every self-contained example above has offline regression cases in `examples/workflow-tests/`.

## Next steps

- [Add your first connector](../../get-started/tutorials/first-connector.md): a connector-backed example, built by hand against a real database.
- [Common workflow patterns](../patterns/workflow-patterns.md): the patterns behind these four.
- [Test a workflow offline](../author/testing.md): the regression cases that hold them true.
- [CI/CD with packages](../patterns/ci-cd.md): shipping them.
