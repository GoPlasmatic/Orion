<!-- description: A self-contained context block to paste into any LLM: Orion's workflow and channel schemas, lifecycle rules and REST calls, for assistants with no shell. -->
# Prompt Pack (any LLM)

The [agent skill](./skills.md) is the richest way to give an AI assistant
control of Orion: the full authoring reference, loaded on demand, driving the
`orion-cli` binary. But it needs an agent that reads skills *and* can run a
shell.

This page is the zero-install alternative: **paste the block below into any
LLM** (as a system prompt, a project instruction, or just the first message)
and it has enough context to write valid workflows and deploy them through
Orion's plain REST API — no CLI, no tooling, just HTTP.

> [!NOTE]
> **Provenance.** The block is hand-maintained and matches **Orion 1.0.0**. It
> is deliberately short — a summary an LLM can hold, not a specification, so it
> tells the model to read `GET /api/v1/admin/functions` for exact input schemas
> rather than trusting the list it carries. If your instance is newer than this
> page, that endpoint is the authority; this text is the orientation.

````text
You are working with Orion, a runtime that turns JSON definitions into live
REST/Kafka services. Everything is managed over a REST admin API. No code,
no deploys.
Base URL: http://localhost:8080 (adjust if told otherwise).

## The three primitives

- WORKFLOW: a pipeline of tasks (the business logic)
- CHANNEL: a service endpoint (REST route or Kafka topic) that routes to a workflow
- CONNECTOR: a named connection to an external system (HTTP API, SQL database,
  MongoDB, Elasticsearch, Redis cache, Kafka), referenced from tasks by name

## Workflow JSON

{
  "workflow_id": "kebab-case-id",
  "name": "Human Name",
  "condition": true,                    // or a JSONLogic expression
  "tasks": [
    {
      "id": "task-id",
      "name": "Task name",
      "condition": { ">": [{ "var": "data.order.total" }, 10000] },   // optional; JSONLogic
      "function": { "name": "<function>", "input": { ... } }
    }
  ]
}

Data flow: requests arrive as { "data": <payload> }. A parse_json task with
input { "source": "payload", "target": "order" } places the payload at
data.order; downstream JSONLogic reads it via { "var": "data.order.<field>" }
and map tasks write via paths like "data.order.flagged". The final data
context is returned to the caller.

Built-in task functions (get exact input schemas from
GET /api/v1/admin/functions, always check before using one):
parse_json, parse_xml, filter, map, validation, http_call, channel_call,
data_query, data_write (portable, backend-neutral DB read/write, preferred),
db_read, db_write (raw SQL escape hatch), cache_read, cache_write,
mongo_read, mongo_write, mongo_aggregate (raw MongoDB; documents are extended
JSON — $oid/$date/nested shapes),
crypto (hash/hmac/hmac_verify/password_hash/password_verify),
jwt_sign, jwt_verify (tokens; channel auth mode "jwt" exposes verified
claims at metadata.auth.claims),
send_email (SMTP connector), storage_presign, storage_head (object storage),
publish_json, publish_xml, publish_kafka, log.

http connectors can carry auth type "oauth2" — Orion manages the token
lifecycle itself (acquisition, caching, single-flight refresh, rotation
persistence); workflows never handle the token.

## Channel JSON

{
  "channel_id": "orders", "name": "orders",
  "channel_type": "sync",              // sync | async
  "protocol": "rest",                  // rest | http | kafka
  "route_pattern": "/orders",          // REST; supports /orders/{id} params
  "methods": ["POST"],
  "workflow_id": "kebab-case-id",
  "tags": ["pkg:orders"]               // optional selection labels; filter with ?tag=
}

## Lifecycle rules (important)

1. Create returns a DRAFT. Nothing serves traffic until activated.
2. Test drafts before activating:
   POST /api/v1/admin/workflows/{id}/test  with body { "data": { ... } }
   → returns an execution trace (which tasks ran / were skipped).
3. Activate: PATCH /api/v1/admin/workflows/{id}/status  {"status":"active"}
   (same for channels). The engine hot-reloads; no restart.
4. Active versions are IMMUTABLE. To change one, create a new version and
   activate it:
   POST /api/v1/admin/workflows/{id}/versions   cut a fresh draft
   PUT  /api/v1/admin/workflows/{id}            put the new content in it
   then activate as above.
5. Rollback is rolling FORWARD to the old content. Nothing reactivates an
   archived version in place — status addresses a workflow id, not a version.
   Cut a new version, PUT the known-good content into it, activate. Because
   active versions are immutable, that content is exactly what last served.

## Core API calls

POST  /api/v1/admin/workflows                      create workflow (draft)
POST  /api/v1/admin/workflows/{id}/test            dry-run with sample data
PATCH /api/v1/admin/workflows/{id}/status          {"status":"active"}
POST  /api/v1/admin/channels                       create channel (draft)
PATCH /api/v1/admin/channels/{id}/status           {"status":"active"}
POST  /api/v1/admin/connectors                     create connector (live immediately)
GET   /api/v1/admin/functions                      function input schemas
GET   /api/v1/openapi.json                         full OpenAPI 3.1 spec
POST  /api/v1/data/{route}                         call a deployed service
POST  /api/v1/data/{route}/async                   async: returns trace_id
GET   /api/v1/admin/traces/{trace_id}               poll async result

## Working style

- Always: create draft → test with realistic sample data → activate.
- Validation errors come back as structured field-pathed errors; fix and retry.
- For database access prefer data_query / data_write (parameterized, portable,
  injection-safe); use db_read / db_write only for SQL the portable dialect
  cannot express.
- Connector configs support ${VAR} / ${VAR:-default} environment references.
  Never embed real credentials in JSON.
- Post plain, self-contained JSON. The authoring shorthands a definition
  directory may use — "$from" for a shared value, "use" for a task fragment —
  are resolved by `orion-server compile` before a deploy; the API takes one
  document with no set to resolve names against and refuses either with
  UNCOMPILED_SOURCE, at any depth.
````

A worked end-to-end session using exactly these calls is
[Understand the HTTP Flow](../getting-started/first-service.md), and the lifecycle
rules above are explained for humans in
[The Entity Lifecycle](../concepts/lifecycle.md). The deeper references the LLM
(or you) may need are the [Workflow Reference](../reference/workflows.md),
[Function Reference](../reference/functions.md), and
[Portable Data Dialect](../reference/data-dialect.md).

> **Tip:** the docs site also serves [`llms.txt`](https://docs.goplasmatic.io/llms.txt)
> (a machine-readable index) and [`llms-full.txt`](https://docs.goplasmatic.io/llms-full.txt)
> (the entire documentation as one file). Point your tools at those for full
> documentation context.

## Related

- [Agent Skill Setup](./skills.md): the richer alternative, when your agent
  reads skills and can run `orion-cli`.
- [The Entity Lifecycle](../concepts/lifecycle.md): the draft/active rules in
  the block above, explained for humans.
- [Task Functions](../reference/functions.md): the authority the block tells
  the model to consult.
