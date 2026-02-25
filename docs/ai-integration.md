# AI Integration

[← Back to README](../README.md)

## Overview

AI writes rules, not services. Instead of generating microservices that need their own governance, LLMs generate Orion rules — constrained JSON that the platform validates, versions, and monitors automatically. This guide covers prompt templates, validation workflows, and CI/CD patterns for AI-generated rules.

## Prompt Templates

Structure your LLM prompts to produce valid Orion rules. Each template includes a system prompt and an example.

### Classification Rules

**System prompt:**

```
You generate Orion rules in JSON format. Rules have:
- name, channel, priority (integer, lower runs first), condition (JSONLogic or true)
- tasks: array of { id, name, condition (optional, JSONLogic), function: { name, input } }
- Every rule starts with a parse_json task: { "name": "parse_json", "input": { "source": "payload", "target": "<entity>" } }
- Use "map" function with "mappings" array for classification. Each mapping has "path" (dot notation) and "logic" (value or JSONLogic).
- Task conditions use { "var": "data.<entity>.<field>" } to reference parsed data.

Output ONLY the JSON rule. No explanation.
```

**User prompt:**

```
Create a rule on channel "tickets" that classifies support tickets:
- critical: priority is "p0" or contains keyword "outage"
- urgent: priority is "p1"
- normal: everything else
```

**Generated rule:**

```json
{
  "name": "Ticket Classifier",
  "channel": "tickets",
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "ticket" }
    }},
    { "id": "critical", "name": "Mark Critical",
      "condition": { "or": [
        { "==": [{ "var": "data.ticket.priority" }, "p0"] },
        { "in": ["outage", { "var": "data.ticket.subject" }] }
      ]},
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.ticket.classification", "logic": "critical" },
        { "path": "data.ticket.escalate", "logic": true }
      ]}}
    },
    { "id": "urgent", "name": "Mark Urgent",
      "condition": { "==": [{ "var": "data.ticket.priority" }, "p1"] },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.ticket.classification", "logic": "urgent" },
        { "path": "data.ticket.escalate", "logic": true }
      ]}}
    },
    { "id": "normal", "name": "Mark Normal",
      "condition": { "and": [
        { "!=": [{ "var": "data.ticket.priority" }, "p0"] },
        { "!=": [{ "var": "data.ticket.priority" }, "p1"] },
        { "!": { "in": ["outage", { "var": "data.ticket.subject" }] } }
      ]},
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.ticket.classification", "logic": "normal" },
        { "path": "data.ticket.escalate", "logic": false }
      ]}}
    }
  ]
}
```

**Send data:**

```bash
curl -s -X POST http://localhost:8080/api/v1/data/tickets \
  -H "Content-Type: application/json" \
  -d '{ "data": { "subject": "Production outage in payments", "priority": "p2" } }'
```

**Response:**

```json
{
  "status": "ok",
  "data": {
    "ticket": { "subject": "Production outage in payments", "priority": "p2", "classification": "critical", "escalate": true }
  },
  "errors": []
}
```

### Routing Rules

**System prompt:**

```
You generate Orion rules that route data to external services via http_call tasks.
- Use connectors by name (do not embed URLs or credentials in rules)
- Use task-level conditions to control which endpoints receive the data
- Available connectors will be provided in the user prompt

Output ONLY the JSON rule. No explanation.
```

**User prompt:**

```
Route notifications on channel "alerts" using these connectors:
- "slack-webhook": all alerts
- "pagerduty-api": only severity "critical" or "high"
Connector names are already configured in Orion.
```

**Generated rule:**

```json
{
  "name": "Alert Router",
  "channel": "alerts",
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "alert" }
    }},
    { "id": "slack", "name": "Notify Slack", "function": {
        "name": "http_call", "input": {
          "connector": "slack-webhook",
          "method": "POST",
          "body_logic": { "cat": ["Alert: ", { "var": "data.alert.message" }] }
        }
    }},
    { "id": "pagerduty", "name": "Page On-Call",
      "condition": { "in": [{ "var": "data.alert.severity" }, ["critical", "high"]] },
      "function": { "name": "http_call", "input": {
          "connector": "pagerduty-api",
          "method": "POST",
          "body_logic": { "var": "data.alert" }
        }
    }}
  ]
}
```

**Send data:**

```bash
curl -s -X POST http://localhost:8080/api/v1/data/alerts \
  -H "Content-Type: application/json" \
  -d '{ "data": { "message": "Database CPU at 95%", "severity": "critical" } }'
```

**Response** — both Slack and PagerDuty tasks execute for critical severity:

```json
{
  "status": "ok",
  "data": {
    "alert": { "message": "Database CPU at 95%", "severity": "critical" }
  },
  "errors": []
}
```

### Validation Rules

**System prompt:**

```
You generate Orion rules that validate incoming data using the "map" function.
- Use task-level conditions to check field presence and constraints
- Return validation results in mapped output fields
- Use JSONLogic operators: "!!", ">=", "var", "missing"

Output ONLY the JSON rule. No explanation.
```

**User prompt:**

```
Create a rule on channel "registrations" that validates user signups:
- email must be present
- age must be >= 18
- Set data.validation.passed to true/false
```

**Generated rule:**

```json
{
  "name": "Registration Validator",
  "channel": "registrations",
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse Payload", "function": {
        "name": "parse_json", "input": { "source": "payload", "target": "user" }
    }},
    { "id": "valid", "name": "Mark Valid",
      "condition": { "and": [
        { "!!": [{ "var": "data.user.email" }] },
        { ">=": [{ "var": "data.user.age" }, 18] }
      ]},
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.validation.passed", "logic": true }
      ]}}
    },
    { "id": "invalid", "name": "Mark Invalid",
      "condition": { "or": [
        { "!": [{ "var": "data.user.email" }] },
        { "<": [{ "var": "data.user.age" }, 18] }
      ]},
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.validation.passed", "logic": false }
      ]}}
    }
  ]
}
```

**Send data:**

```bash
curl -s -X POST http://localhost:8080/api/v1/data/registrations \
  -H "Content-Type: application/json" \
  -d '{ "data": { "email": "alice@example.com", "age": 25, "name": "Alice" } }'
```

**Response:**

```json
{
  "status": "ok",
  "data": {
    "user": { "email": "alice@example.com", "age": 25, "name": "Alice" },
    "validation": { "passed": true }
  },
  "errors": []
}
```

## Validation Workflow

Every AI-generated rule should go through this workflow before reaching production.

### Step 1: Generate the rule

Use your LLM (API call, chat, or agent) with the prompt templates above to produce a rule JSON.

### Step 2: Create the rule (paused)

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/rules \
  -H "Content-Type: application/json" \
  -d '{
    "name": "AI-Generated Fraud Check",
    "channel": "orders",
    "status": "paused",
    "condition": true,
    "tasks": [ ... ]
  }'
```

Creating it as `paused` ensures it won't process live traffic until validated.

### Step 3: Dry-run with test data

```bash
curl -s -X POST http://localhost:8080/api/v1/admin/rules/{id}/test \
  -H "Content-Type: application/json" \
  -d '{ "data": { "amount": 15000, "country": "US" } }'
```

### Step 4: Check the trace

Inspect the response to verify correct task execution:

```json
{
  "matched": true,
  "trace": {
    "steps": [
      { "task_id": "parse", "result": "executed" },
      { "task_id": "high_risk", "result": "executed" },
      { "task_id": "normal", "result": "skipped" }
    ]
  },
  "output": { "order": { "amount": 15000, "risk": "high", "requires_review": true } },
  "errors": []
}
```

Confirm that the right tasks ran, the right tasks were skipped, and the output matches expectations. Run multiple test cases covering edge cases.

### Step 5: Activate

```bash
curl -s -X PUT http://localhost:8080/api/v1/admin/rules/{id}/activate
```

The rule is now live. The engine hot-reloads automatically — no restart needed.

## CI/CD for AI-Generated Rules

Integrate AI rule generation into your deployment pipeline. Rules are JSON files — they version, diff, and review like any other config.

### Pipeline pattern

```
AI generates rule → commit as JSON → CI runs dry-run → review → import
```

### GitHub Actions example

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

### Test case format

Store test cases alongside rules:

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

## Safety Guardrails

AI-generated rules get the same governance as hand-written ones:

- **Version history** — every rule change is recorded. Roll back to any previous version if an AI-generated rule misbehaves.
- **Paused status** — instantly disable a rule without deleting it. Create AI rules as `paused` by default and activate only after dry-run validation.
- **Dry-run before activate** — test with representative data and inspect the full execution trace. No rule should go live without passing dry-run tests.
- **Audit trail** — the `rule_versions` table records who changed what and when, providing accountability for AI-generated changes.
- **Connectors isolate secrets** — AI generates rules that reference connector names, never credentials. Secrets stay in connector configs, outside the rule JSON.

See [Use Cases & Patterns](use-cases.md#ai-assisted-rule-generation) for a worked example, and [API Reference](api-reference.md) for the full endpoint documentation.
