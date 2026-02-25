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

## Paused Rule Lifecycle

Pause rules to temporarily disable processing without deleting them. This is critical for AI-generated rules — create them as `paused`, validate with dry-run, then activate only when verified.

```bash
# Rule is active — data gets transformed
orion-cli send content -d '{"text": "Hello"}'
# → { "data": { "post": { "text": "Hello", "moderated": true, "status": "reviewed" } } }

# Pause the rule
orion-cli rules pause <rule-id>
orion-cli engine reload

# Rule is paused — data passes through unmodified
orion-cli send content -d '{"text": "Hello"}'
# → { "data": {} }

# Reactivate
orion-cli rules activate <rule-id>
orion-cli engine reload
# Rule is active again
```

Paused rules remain in the database and retain their configuration. See [API Reference](api-reference.md) for status management endpoints.

**Key patterns:** Runtime rule toggling, engine reload after status changes.

## HTTP Error Handling

Use `continue_on_error` to keep the pipeline running when external calls fail.

**AI prompt:**

```
Create a rule on the "http-test" channel that parses the payload and calls an external
API via the "my-api" connector. Enable continue_on_error so the pipeline doesn't halt
if the API call fails. Use a 2-second timeout.
```

**Generated rule:**

```json
{
  "name": "HTTP Call Error Test",
  "channel": "http-test",
  "continue_on_error": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "req" }
    }},
    { "id": "call", "name": "Call External API", "function": {
        "name": "http_call", "input": {
          "connector": "my-api",
          "method": "POST",
          "body": { "test": true },
          "timeout_ms": 2000
        }
    }}
  ]
}
```

**Send data:**

```bash
curl -s -X POST http://localhost:8080/api/v1/data/http-test \
  -H "Content-Type: application/json" \
  -d '{ "data": { "action": "test-call" } }'
```

**Response** (when the connector target is unreachable — errors are collected, not fatal):

```json
{
  "status": "ok",
  "data": { "req": { "action": "test-call" } },
  "errors": [
    { "code": "TASK_ERROR", "task_id": "call", "message": "IO error: HTTP request failed..." }
  ]
}
```

Without `continue_on_error`, the same failure would return a hard error and exit code 1. See [Production Features](production-features.md#fault-tolerant-pipelines) for details.

**Key patterns:** Fault-tolerant pipelines, error collection, graceful degradation.

## Connector Secret Masking

Connector secrets are automatically masked in API responses — AI generates rules that reference connector names, never credentials.

**AI prompt:**

```
Create an HTTP connector called "bearer-auth-api" pointing to https://api.example.com/v1
with bearer token authentication.
```

**Generated connector:**

```json
{
  "name": "bearer-auth-api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.example.com/v1",
    "auth": { "type": "bearer", "token": "super-secret-bearer-token-123" }
  }
}
```

**Read it back — secrets are masked:**

```bash
orion-cli connectors get <id>
# auth.token → "******"
# auth.password → "******"
# auth.key → "******"
```

Usernames and non-sensitive fields are returned as-is. AI-generated rules reference `"connector": "bearer-auth-api"` — they never see or embed the actual credentials. See [Connectors](connectors.md) for all auth schemes.

**Key patterns:** Secret management, safe API responses, credential isolation.

## AI-Assisted Rule Generation

Use an LLM to generate Orion rules from natural language, then validate them with dry-run before activating. The constrained JSON format makes LLM output reliable — no open-ended code generation.

**Prompt:**

```
Generate an Orion rule for the "orders" channel that detects potential fraud:
- Parse the payload into "order"
- Flag as suspicious if amount > 5000 AND shipping country differs from billing country
- Flag as high_risk if amount > 10000
- Otherwise mark as clear
```

**Generated rule:**

```json
{
  "name": "Order Fraud Detection",
  "channel": "orders",
  "priority": 10,
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "order" }
    }},
    { "id": "high_risk", "name": "Flag High Risk",
      "condition": { ">": [{ "var": "data.order.amount" }, 10000] },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.order.risk", "logic": "high_risk" },
        { "path": "data.order.requires_review", "logic": true }
      ]}}
    },
    { "id": "suspicious", "name": "Flag Suspicious",
      "condition": { "and": [
        { ">": [{ "var": "data.order.amount" }, 5000] },
        { "<=": [{ "var": "data.order.amount" }, 10000] },
        { "!=": [{ "var": "data.order.shipping_country" }, { "var": "data.order.billing_country" }] }
      ]},
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.order.risk", "logic": "suspicious" },
        { "path": "data.order.requires_review", "logic": true }
      ]}}
    },
    { "id": "clear", "name": "Mark Clear",
      "condition": { "<=": [{ "var": "data.order.amount" }, 5000] },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.order.risk", "logic": "clear" },
        { "path": "data.order.requires_review", "logic": false }
      ]}}
    }
  ]
}
```

**Validate with dry-run before activating:**

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/rules/{id}/test \
  -H "Content-Type: application/json" \
  -d '{ "data": { "amount": 7500, "shipping_country": "CA", "billing_country": "US" } }'
```

```json
{
  "matched": true,
  "trace": {
    "steps": [
      { "task_id": "parse", "result": "executed" },
      { "task_id": "high_risk", "result": "skipped" },
      { "task_id": "suspicious", "result": "executed" },
      { "task_id": "clear", "result": "skipped" }
    ]
  },
  "output": {
    "order": { "amount": 7500, "shipping_country": "CA", "billing_country": "US", "risk": "suspicious", "requires_review": true }
  },
  "errors": []
}
```

The workflow: **generate** → **dry-run** → **activate** → **monitor**. AI generates the rule, dry-run validates the logic, and the platform handles governance from there. See [AI Integration](ai-integration.md) for prompt templates and CI/CD patterns.

**Key patterns:** LLM-constrained JSON output, dry-run validation loop, API-driven lifecycle.

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
