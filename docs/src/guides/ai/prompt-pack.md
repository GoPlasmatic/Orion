<!-- description: A self-contained context block to paste into any LLM: Orion's workflow and channel schemas, deployment rules and REST calls, for assistants with no shell. -->
<!-- type: guide -->
<!-- last_verified: 2026-10-25 -->

# Prompt pack for any LLM

The [agent skill](./agent-skill.md) gives an AI assistant the most control of Orion, but it needs a shell. This page is the zero-install alternative: one block to paste into any LLM as a system prompt. With it, a model can write valid workflows and deploy them through Orion's REST API.

## Before you start

You need an LLM that takes a system prompt or a long first message. You also need an Orion instance it can reach over HTTP. Adjust the base URL in the block if the instance is not local.

> [!NOTE]
> The block is hand-maintained. It is deliberately short: a summary an LLM can hold, not a specification. It tells the model to read `GET /api/v2/admin/functions` for exact input schemas rather than trusting the list it carries. If your instance is newer than this page, that endpoint is the authority; this text is the orientation.

## The block

````text
You are working with Orion, a runtime that turns JSON definitions into live
services: REST routes, plain HTTP endpoints and cron schedules. Definitions reach
a node as a deployment, over a REST admin API. No code, no builds.
Base URL: http://localhost:8080 (adjust if told otherwise).

## The three primitives

- WORKFLOW: a pipeline of tasks (the business logic)
- CHANNEL: a service endpoint (REST route, plain HTTP name or cron schedule) that
  routes to a workflow
- CONNECTOR: a named connection to an external system (HTTP API, SQL database,
  an in-memory cache), referenced from tasks by name

## Workflow JSON

{
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
GET /api/v2/admin/functions, always check before using one):
parse_json, parse_xml, filter, map, validation, http_call, channel_call,
data_query, data_write (portable, backend-neutral DB read/write, preferred),
db_read, db_write (raw SQL escape hatch), cache_read, cache_write,
crypto (hash/hmac/hmac_verify/password_hash/password_verify),
jwt_sign, jwt_verify (tokens; channel auth mode "jwt" exposes verified
claims at metadata.auth.claims),
publish_json, publish_xml, log.

http connectors can carry auth type "oauth2": Orion manages the token
lifecycle itself (acquisition, caching, single-flight refresh, rotation
persistence); workflows never handle the token.

## Channel JSON

{
  "name": "orders",
  "channel_type": "sync",              // sync | async
  "protocol": "rest",                  // rest | http | cron
  "route_pattern": "/orders",          // REST; supports /orders/{id} params
  "methods": ["POST"]
}

Neither document carries its own id, version or the workflow a channel runs.
The deployment manifest supplies those.

A cron channel has no route: it declares "transport_config": { "schedule":
"0 15 2 * * *", "payload": { ... } } (six fields, seconds first) and is
always async.

## Deployment rules (important)

0. If you can run a shell, do not hand-build manifests. Write a definition
   set: one directory of workflow, channel and connector JSON files, each with
   its id (workflow_id, channel_id, a connector's name). Then run
     orion-server lint <dir>
     orion-server compile <dir> -o dist
     orion-cli deployments plan -d dist
     orion-cli deployments apply -d dist
   compile resolves shared fragments and turns connectors into connections;
   apply assigns revisions and reuses unchanged ones. The REST rules below are
   for an assistant with no shell.
1. Upload each definition: POST /api/v2/admin/definitions with
   { "kind": "workflow" | "channel", "definition": { ... } }
   → answers { "digest": "sha256:..." }. Uploading serves nothing.
   A definition that would not serve is refused with field-pathed errors.
2. Test a workflow before deploying it:
   POST /api/v2/admin/definitions/test with body
   { "definition": { ...workflow... }, "data": { ... } }
   → returns the output and an execution trace (which tasks ran / were
   skipped). It runs against LIVE connections: nothing is stubbed.
3. Plan, then deploy. POST /api/v2/admin/deployment-plans
   { "base_revision": <current head>, "manifest": ... } answers the
   reviewed_manifest_digest; POST /api/v2/admin/deployments with
   { "idempotency_key": "...", "base_revision": <current head>,
     "reason": "...", "reviewed_manifest_digest": "sha256:...",
     "manifest": { "schema_version": 2, "providers": [], "members": [
       { "entity": { "kind": "workflow", "id": "orders-wf", "revision": 1 },
         "definition_digest": "sha256:..." },
       { "entity": { "kind": "channel", "id": "orders", "revision": 1 },
         "definition_digest": "sha256:...",
         "requires": [{ "kind": "workflow", "id": "orders-wf", "revision": 1 }] } ] } }
   A channel requires its workflow. The engine hot-reloads; no restart.
4. A deployment is EVERYTHING the node serves. A member left out of the
   manifest stops serving. Read the current head with
   GET /api/v2/admin/deployments, its manifest with
   GET /api/v2/admin/deployments/{head}, and start from it.
5. A changed definition is a new digest. Give its member the next revision
   and plan and apply a new deployment, as in step 3.
6. Rollback is rolling FORWARD: POST /api/v2/admin/deployments
   { "idempotency_key": "...", "base_revision": N, "rollback_to": M }
   commits revision M's manifest as a new revision.

## Core API calls

POST  /api/v2/admin/definitions                    upload a definition, answers its digest
POST  /api/v2/admin/definitions/test               run a workflow once with sample data
POST  /api/v2/admin/connections/{id}/revisions     create a connection revision
POST  /api/v2/admin/deployment-plans               check a manifest; commits nothing
POST  /api/v2/admin/deployments                    apply a reviewed manifest, or roll back
GET   /api/v2/admin/deployments                    the head and a page of revisions
GET   /api/v2/admin/deployments/{revision}         one revision with its manifest
GET   /api/v2/admin/functions                      function input schemas
GET   /api/v2/openapi.json                         full OpenAPI 3.1 spec
POST  /api/v2/data/{route}                         call a deployed service
POST  /api/v2/data/{route}/async                   async: returns trace_id
GET   /api/v2/admin/traces/{trace_id}               poll async result

## Working style

- Always: upload → test with realistic sample data → plan → deploy.
- Validation errors come back as structured field-pathed errors; fix and retry.
- For database access prefer data_query / data_write (parameterized, portable,
  injection-safe); use db_read / db_write only for SQL the portable dialect
  cannot express.
- A connection keeps secrets out of its public_config: put a reference such
  as env://VAR under secret_locators. Never embed real credentials in JSON.
- Post plain, self-contained JSON. The authoring shorthands a definition
  directory may use ("$from" for a shared value, "use" for a task fragment)
  are resolved by `orion-server compile` before a deploy; the API takes one
  document with no set to resolve names against and refuses either with
  UNCOMPILED_SOURCE, at any depth.
````

## Verify

Ask the assistant to write a workflow and test it before deploying anything. A model that has read the block posts it to `/api/v2/admin/definitions/test` with a `data` body. It reads the trace back before it touches `/deployments`. A worked session using exactly these calls is the [Quickstart](../../get-started/quickstart.md).

> [!TIP]
> The docs site also serves [`llms.txt`](https://docs.goplasmatic.io/llms.txt), a machine-readable index, and [`llms-full.txt`](https://docs.goplasmatic.io/llms-full.txt), the whole book as one file. Point your tools at those for full documentation context.

## Next steps

- [Set up the agent skill](./agent-skill.md): the richer alternative, when your agent reads skills and can run `orion-cli`.
- [Deployment endpoints](../../reference/admin-api/deployments.md): the deployment rules in the block above, explained for people.
- [Task functions](../../reference/functions/index.md): the authority the block tells the model to consult.
- [Workflow definition](../../reference/workflows.md) and [Portable data dialect](../../reference/data-dialect.md): the deeper references the model, or you, may need.
