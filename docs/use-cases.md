# Use Cases & Patterns

[← Back to README](../README.md)

Real-world examples showing how AI generates Orion rules from natural language. Every example follows the same workflow: **describe what you need → AI generates the rule → send data → get results**. The rule definitions come directly from the [e2e test cases](../tests/e2e/cases/).

## E-Commerce Order Classification

Classify orders into tiers and compute discounts using multiple rules on the same channel.

**AI prompt:**

```
Create two rules on the "orders" channel:
1. A parse rule at priority 0 that parses the payload into "order"
2. A classification rule at priority 10 that assigns tiers based on amount:
   - VIP: amount >= 500, discount 15%
   - Premium: amount 100-500, discount 5%
   - Standard: amount < 100, no discount
```

**Generated rules:**

```json
{
  "name": "Parse Orders",
  "channel": "orders",
  "priority": 0,
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "order" }
    }}
  ]
}
```

```json
{
  "name": "VIP Order Rule",
  "channel": "orders",
  "priority": 10,
  "condition": true,
  "tasks": [
    { "id": "vip_tier", "name": "Set VIP Tier",
      "condition": { ">=": [{ "var": "data.order.amount" }, 500] },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.order.tier", "logic": "vip" },
        { "path": "data.order.discount_pct", "logic": 15 }
      ]}}
    },
    { "id": "premium_tier", "name": "Set Premium Tier",
      "condition": { "and": [
        { ">=": [{ "var": "data.order.amount" }, 100] },
        { "<": [{ "var": "data.order.amount" }, 500] }
      ]},
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.order.tier", "logic": "premium" },
        { "path": "data.order.discount_pct", "logic": 5 }
      ]}}
    },
    { "id": "standard_tier", "name": "Set Standard Tier",
      "condition": { "<": [{ "var": "data.order.amount" }, 100] },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.order.tier", "logic": "standard" },
        { "path": "data.order.discount_pct", "logic": 0 }
      ]}}
    }
  ]
}
```

**Send data:**

```bash
curl -s -X POST http://localhost:8080/api/v1/data/orders \
  -H "Content-Type: application/json" \
  -d '{ "data": { "amount": 750, "product": "Diamond Ring" } }'
```

**Response:**

```json
{
  "status": "ok",
  "data": {
    "order": { "amount": 750, "product": "Diamond Ring", "tier": "vip", "discount_pct": 15 }
  },
  "errors": []
}
```

**Key patterns:** Multi-rule priority ordering, task-level conditions, computed output fields.

## IoT Sensor Alert Classification

Classify sensor readings into severity levels using range-based conditions.

**AI prompt:**

```
Create a rule on the "sensors" channel that classifies temperature readings:
- Critical: temperature > 90 or below 0, set alert flag
- Warning: temperature 70-90, set alert flag
- Normal: temperature 0-70, no alert
Parse the payload into "reading" and set severity and alert fields.
```

**Generated rule:**

```json
{
  "name": "Sensor Alert Pipeline",
  "channel": "sensors",
  "priority": 10,
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "reading" }
    }},
    { "id": "critical", "name": "Mark Critical",
      "condition": { "or": [
        { ">": [{ "var": "data.reading.temperature" }, 90] },
        { "<": [{ "var": "data.reading.temperature" }, 0] }
      ]},
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.reading.severity", "logic": "critical" },
        { "path": "data.reading.alert", "logic": true }
      ]}}
    },
    { "id": "warning", "name": "Mark Warning",
      "condition": { "and": [
        { ">": [{ "var": "data.reading.temperature" }, 70] },
        { "<=": [{ "var": "data.reading.temperature" }, 90] }
      ]},
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.reading.severity", "logic": "warning" },
        { "path": "data.reading.alert", "logic": true }
      ]}}
    },
    { "id": "normal", "name": "Mark Normal",
      "condition": { "and": [
        { ">=": [{ "var": "data.reading.temperature" }, 0] },
        { "<=": [{ "var": "data.reading.temperature" }, 70] }
      ]},
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.reading.severity", "logic": "normal" },
        { "path": "data.reading.alert", "logic": false }
      ]}}
    }
  ]
}
```

**Send data:**

```bash
curl -s -X POST http://localhost:8080/api/v1/data/sensors \
  -H "Content-Type: application/json" \
  -d '{ "data": { "temperature": 80, "sensor_id": "SENSOR-42" } }'
```

**Response:**

```json
{
  "status": "ok",
  "data": {
    "reading": { "temperature": 80, "sensor_id": "SENSOR-42", "severity": "warning", "alert": true }
  },
  "errors": []
}
```

| Input | Severity | Alert |
|-------|----------|-------|
| `temperature: 45` | normal | false |
| `temperature: 80` | warning | true |
| `temperature: 95` | critical | true |
| `temperature: -5` | critical | true |

**Key patterns:** Range-based classification with `and`/`or` conditions, boolean flags.

## Webhook Payload Transformation

Normalize incoming webhook payloads from different providers into a consistent internal schema.

**AI prompt:**

```
Create a rule on the "webhooks" channel that normalizes webhook payloads from any provider:
- Map "origin" to "source"
- Map "type" to "event_type"
- Map "body" to "payload"
- Add a "processed" flag set to true
Output should be under data.normalized.
```

**Generated rule:**

```json
{
  "name": "Webhook Transform Pipeline",
  "channel": "webhooks",
  "priority": 10,
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "event" }
    }},
    { "id": "normalize", "name": "Normalize Schema", "function": {
        "name": "map", "input": { "mappings": [
          { "path": "data.normalized.source", "logic": { "var": "data.event.origin" } },
          { "path": "data.normalized.event_type", "logic": { "var": "data.event.type" } },
          { "path": "data.normalized.payload", "logic": { "var": "data.event.body" } },
          { "path": "data.normalized.processed", "logic": true }
        ]}
    }}
  ]
}
```

**Send data:**

```bash
curl -s -X POST http://localhost:8080/api/v1/data/webhooks \
  -H "Content-Type: application/json" \
  -d '{ "data": { "origin": "github", "type": "push", "body": {"ref": "refs/heads/main"} } }'
```

**Response:**

```json
{
  "status": "ok",
  "data": {
    "normalized": { "source": "github", "event_type": "push", "payload": {"ref": "refs/heads/main"}, "processed": true }
  },
  "errors": []
}
```

Missing optional fields produce `null` — no errors. This makes the pipeline safe for partial payloads from different webhook providers.

**Key patterns:** Schema mapping with `var`, null-safe field access, static enrichment.

## Notification Routing

Route notifications to different delivery channels based on severity.

**AI prompt:**

```
Create a rule on the "notifications" channel that routes by severity:
- Log all notifications
- Send email for anything except "low" severity
- Send SMS only for "high" and "critical" severity
Parse the payload into "notification".
```

**Generated rule:**

```json
{
  "name": "Notification Router",
  "channel": "notifications",
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "notification" }
    }},
    { "id": "log_all", "name": "Log All Notifications", "function": {
        "name": "map", "input": { "mappings": [
          { "path": "data.notification.logged", "logic": true }
        ]}
    }},
    { "id": "email", "name": "Send Email",
      "condition": { "!=": [{ "var": "data.notification.severity" }, "low"] },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.notification.email_sent", "logic": true }
      ]}}
    },
    { "id": "sms", "name": "Send SMS for High/Critical",
      "condition": { "in": [{ "var": "data.notification.severity" }, ["high", "critical"]] },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.notification.sms_sent", "logic": true }
      ]}}
    }
  ]
}
```

**Send data:**

```bash
curl -s -X POST http://localhost:8080/api/v1/data/notifications \
  -H "Content-Type: application/json" \
  -d '{ "data": { "message": "Disk usage at 92%", "severity": "high" } }'
```

**Response:**

```json
{
  "status": "ok",
  "data": {
    "notification": { "message": "Disk usage at 92%", "severity": "high", "logged": true, "email_sent": true, "sms_sent": true }
  },
  "errors": []
}
```

| Severity | Logged | Email | SMS |
|----------|--------|-------|-----|
| low | yes | no | no |
| medium | yes | yes | no |
| high | yes | yes | yes |
| critical | yes | yes | yes |

In production, replace the `map` tasks with `http_call` tasks pointing to your email and SMS [connectors](connectors.md).

**Key patterns:** Task-level condition gating, `in` operator for set membership, progressive pipeline.

## Compliance Risk Classification

Classify transactions by risk level and use [dry-run testing](api-reference.md) to verify rules before activating them.

**AI prompt:**

```
Create a rule on the "compliance" channel that classifies transaction risk:
- High risk: amount > 10000, requires manual review
- Normal risk: amount <= 10000, no review needed
Parse the payload into "txn".
```

**Generated rule:**

```json
{
  "name": "Risk Classifier",
  "channel": "compliance",
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "txn" }
    }},
    { "id": "high_risk", "name": "Flag High Risk",
      "condition": { ">": [{ "var": "data.txn.amount" }, 10000] },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.txn.risk_level", "logic": "high" },
        { "path": "data.txn.requires_review", "logic": true }
      ]}}
    },
    { "id": "normal_risk", "name": "Normal Risk",
      "condition": { "<=": [{ "var": "data.txn.amount" }, 10000] },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.txn.risk_level", "logic": "normal" },
        { "path": "data.txn.requires_review", "logic": false }
      ]}}
    }
  ]
}
```

**Dry-run before going live:**

```bash
orion-cli rules test <rule-id> -d '{"amount": 50000, "currency": "USD"}'
```

```json
{
  "matched": true,
  "trace": { "steps": [
    { "task_id": "parse", "result": "executed" },
    { "task_id": "high_risk", "result": "executed" },
    { "task_id": "normal_risk", "result": "skipped" }
  ]},
  "output": { "txn": { "amount": 50000, "currency": "USD", "risk_level": "high", "requires_review": true } }
}
```

**Send data:**

```bash
curl -s -X POST http://localhost:8080/api/v1/data/compliance \
  -H "Content-Type: application/json" \
  -d '{ "data": { "amount": 50000, "currency": "USD", "account": "ACC-1234" } }'
```

**Response:**

```json
{
  "status": "ok",
  "data": {
    "txn": { "amount": 50000, "currency": "USD", "account": "ACC-1234", "risk_level": "high", "requires_review": true }
  },
  "errors": []
}
```

The trace shows exactly which tasks ran and which were skipped — verify the logic is correct before a single real transaction flows through.

**Key patterns:** Dry-run verification, execution trace inspection, regulatory workflow.

## AI Workflow & CI/CD

AI writes rules, not services. Instead of generating microservices that need their own governance, LLMs generate Orion rules — constrained JSON that the platform validates, versions, and monitors automatically.

### Prompt Templates

Structure your LLM prompts to produce valid Orion rules. Here's a reusable system prompt:

```
You generate Orion rules in JSON format. Rules have:
- name, channel, priority (integer, lower runs first), condition (JSONLogic or true)
- tasks: array of { id, name, condition (optional, JSONLogic), function: { name, input } }
- Every rule starts with a parse_json task: { "name": "parse_json", "input": { "source": "payload", "target": "<entity>" } }
- Use "map" function with "mappings" array for transforms. Each mapping has "path" (dot notation) and "logic" (value or JSONLogic).
- Use "http_call" with "connector" (by name) for external API calls. Do not embed URLs or credentials in rules.
- Task conditions use { "var": "data.<entity>.<field>" } to reference parsed data.

Output ONLY the JSON rule. No explanation.
```

### Validation Workflow

Every AI-generated rule should go through this workflow before reaching production:

1. **Generate** — use your LLM with the prompt template above
2. **Create as paused** — `POST /api/v1/admin/rules` with `"status": "paused"` so it won't process live traffic
3. **Dry-run** — `POST /api/v1/admin/rules/{id}/test` with representative test data
4. **Check the trace** — verify the right tasks ran, the right ones were skipped, and output matches expectations
5. **Activate** — `PATCH /api/v1/admin/rules/{id}/status` with `"status": "active"`

### CI/CD Pipeline

Integrate AI rule generation into your deployment pipeline. Rules are JSON files — they version, diff, and review like any other config.

```
AI generates rule → commit as JSON → CI runs dry-run → review → import
```

**GitHub Actions example:**

```yaml
name: Validate AI Rules
on:
  pull_request:
    paths: ['rules/**/*.json']

jobs:
  validate:
    runs-on: ubuntu-latest
    services:
      orion:
        image: ghcr.io/goplasmatic/orion:latest
        ports: ['8080:8080']
    steps:
      - uses: actions/checkout@v4

      - name: Import rules (paused)
        run: |
          for file in rules/**/*.json; do
            curl -s -X POST http://localhost:8080/api/v1/admin/rules \
              -H "Content-Type: application/json" \
              -d @"$file"
          done

      - name: Dry-run test cases
        run: |
          for test in rules/tests/**/*.json; do
            RULE_ID=$(jq -r '.rule_id' "$test")
            DATA=$(jq -c '.data' "$test")
            EXPECTED=$(jq -c '.expected_output' "$test")

            RESULT=$(curl -s -X POST \
              "http://localhost:8080/api/v1/admin/rules/${RULE_ID}/test" \
              -H "Content-Type: application/json" \
              -d "$DATA")

            OUTPUT=$(echo "$RESULT" | jq -c '.output')
            if [ "$OUTPUT" != "$EXPECTED" ]; then
              echo "FAIL: $test"
              echo "Expected: $EXPECTED"
              echo "Got: $OUTPUT"
              exit 1
            fi
          done

      - name: Deploy to production
        if: github.event_name == 'push' && github.ref == 'refs/heads/main'
        run: |
          orion-cli rules import -f rules/ --server ${{ secrets.ORION_URL }}
```

**Test case format** — store test cases alongside rules:

```
rules/
  fraud-detection.json       # The rule
  tests/
    fraud-high-risk.json     # Test case
    fraud-clear.json         # Test case
```

Each test case:

```json
{
  "rule_id": "fraud-detection",
  "data": { "data": { "amount": 15000, "country": "US" } },
  "expected_output": { "order": { "amount": 15000, "risk": "high", "requires_review": true } }
}
```

### Safety Guardrails

AI-generated rules get the same governance as hand-written ones:

- **Version history** — every rule change is recorded. Roll back if an AI-generated rule misbehaves.
- **Paused status** — create AI rules as `paused` by default and activate only after dry-run validation.
- **Dry-run before activate** — test with representative data and inspect the full execution trace.
- **Audit trail** — the `rule_versions` table records who changed what and when.
- **Connectors isolate secrets** — AI generates rules that reference connector names, never credentials.

## Common Rule Patterns

### The parse-then-process pattern

Every rule that reads input data must start with `parse_json`. Without it, task conditions referencing `data.X` see empty context.

```json
{
  "tasks": [
    { "id": "parse", "function": { "name": "parse_json", "input": { "source": "payload", "target": "order" } } },
    { "id": "process", "condition": { ">": [{ "var": "data.order.total" }, 100] }, "function": { ... } }
  ]
}
```

### Multi-rule pipelines with priority

Multiple rules on the same channel execute in priority order (lowest first). Use a shared parse rule at priority 0, then classification rules at higher priorities:

```
Priority 0:  Parse payload → data.order
Priority 10: Classify tier (VIP/premium/standard)
Priority 20: Apply discounts based on tier
```

### Task-level vs rule-level conditions

- **Rule-level condition** — determines whether the entire rule matches. Set to `true` for rules that always run.
- **Task-level condition** — determines whether a specific task within a matched rule executes. Use for branching logic within a pipeline.

```json
{
  "condition": true,
  "tasks": [
    { "id": "always", "function": { ... } },
    { "id": "conditional", "condition": { ">": [{ "var": "data.amount" }, 500] }, "function": { ... } }
  ]
}
```

### External API calls with connectors

Keep credentials in [connectors](connectors.md), reference them by name in rules:

```json
{
  "tasks": [
    { "id": "parse", "function": { "name": "parse_json", "input": { "source": "payload", "target": "event" } } },
    { "id": "notify", "function": { "name": "http_call", "input": {
        "connector": "slack-webhook",
        "method": "POST",
        "body_logic": { "var": "data.event" }
    }}}
  ]
}
```
