<!-- description: Four Orion services, each described in a sentence and deployed as JSON — with every block pulled from the example packages CI actually deploys and tests. -->
# Worked Examples: Prompt to Service

Four services, each described in a sentence, generated as JSON, and deployed. The
JSON in this page is pulled from the repository's example packages, so every
block is the file CI deploys and tests — not a paraphrase of it.

## Set up once

These examples all run against the shipped packages, so start from a clone with
a server on `http://localhost:8080`:

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion
```

Each example below names the one command that creates its workflow **and** its
channel and activates both. Running the `curl` before that command is the one
guaranteed way to get a `404` — the endpoint does not exist until the channel
does.

## Tiered order classification

**What you want:** classify orders into tiers, and set a discount per tier.

**The prompt:**

```
Create an Orion workflow for the "order-tiers" channel that parses the payload
into "order" and assigns a tier from the amount: vip at 500 or more with a 15%
discount, premium from 100 to 500 with 5%, standard below 100 with none.
```

**The workflow:**

```json
{{#include ../../../examples/packages/order-classification/workflow.json}}
```

**Deploy and call it:**

```bash
./examples/deploy.sh order-classification

curl -s -X POST http://localhost:8080/api/v1/data/order-tiers \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "amount": 750, "product": "Diamond Ring" } }'
```

The response carries the parsed order with `tier` and `discount_pct` added.

**What this shows:** task-level conditions as an if/else chain. The three tier
tasks have mutually exclusive conditions, so exactly one runs and the others are
recorded as skipped in the trace.

## Range-based sensor alerts

**What you want:** grade sensor readings into severities, and flag the ones that
need attention.

**The prompt:**

```
Create an Orion workflow for the "sensors" channel that parses the payload into
"reading" and sets severity from temperature: critical above 90 or below 0,
warning from 70 to 90, normal otherwise. Set an alert flag for critical and
warning.
```

**The workflow:**

```json
{{#include ../../../examples/packages/iot-sensor-alert/workflow.json}}
```

**Deploy and call it:**

```bash
./examples/deploy.sh iot-sensor-alert

curl -s -X POST http://localhost:8080/api/v1/data/sensors \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "sensor_id": "SENSOR-42", "temperature": 95 } }'
```

**What this shows:** `and` and `or` composing a range test. Note that the bands
are written to be mutually exclusive — overlapping ranges would run both tasks,
and the later one would win.

## Normalizing webhook payloads

**What you want:** take whatever shape a provider sends and store one shape.

**The prompt:**

```
Create an Orion workflow for the "webhooks" channel that parses the payload into
"event" and maps provider fields into a common schema, tolerating missing
fields.
```

**The workflow:**

```json
{{#include ../../../examples/packages/webhook-transform/workflow.json}}
```

**Deploy and call it:**

```bash
./examples/deploy.sh webhook-transform

curl -s -X POST http://localhost:8080/api/v1/data/webhooks \
  -H 'Content-Type: application/json' \
  --data @examples/packages/webhook-transform/request.json
```

**What this shows:** `var` is null-safe. A field the provider omitted maps to
`null` rather than failing the task, which is what lets one workflow accept
several providers' payloads. Send an empty body and the workflow still
normalizes.

**In production, add authentication.** A webhook endpoint reachable by anyone is
a webhook endpoint anyone can forge. Use `hmac` mode — see
[Configure Channels › Authenticate callers](../build/channels.md#authenticate-callers).

## Severity-based notification routing

**What you want:** log everything, email anything above `low`, and text only the
urgent ones.

> [!IMPORTANT]
> **Nothing is actually sent.** The `email` and `sms` tasks set flags with
> `map`; no email leaves the process and no SMS is delivered. The example is
> about the *routing decision*, which is the part worth version-controlling.
>
> To make it real, replace those `map` tasks with `http_call` tasks pointing at
> an email and an SMS connector. The conditions do not change — see
> [Connect Databases & APIs](../build/connectors.md).

**The prompt:**

```
Create an Orion workflow for the "notifications" channel that parses the payload
into "notification", logs everything, emails anything except low severity, and
sends SMS only for high and critical.
```

**The workflow:**

```json
{{#include ../../../examples/packages/notification-routing/workflow.json}}
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

**What this shows:** the `in` operator for set membership, and a progressive
pipeline where each task adds to the same object rather than branching away from
it.

## Where to go next

- **A connector-backed example:**
  [Your First Connector](../getting-started/first-connector.md) builds the
  `postgres-orders` package by hand, against a real database.
- **The patterns behind these:**
  [Common Workflow Patterns](./workflow-patterns.md).
- **Testing them:** every self-contained example above has offline regression
  cases in `examples/workflow-tests/` — see
  [Test Workflows Offline](../build/testing.md).
- **Shipping them:** [CI/CD with Packages](./ci-cd.md).
