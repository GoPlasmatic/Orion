---
name: orion
description: Author, validate, test, compile, deploy, inspect, roll back, and troubleshoot declarative services on Orion using orion-server and orion-cli. Use for Orion workflows, channels, connectors, plugins, definition sets, deployments, traces, or runtime operations; do not use for unrelated products also named Orion.
---

# Orion

Orion is a declarative services runtime. A service is described by JSON files
rather than application code:

- a **workflow** is ordered business logic;
- a **channel** exposes a REST/HTTP route or a cron schedule, and binds it
  to a workflow;
- a **connector** names an external dependency and its policy;
- a **plugin** adds a custom task function as a sandboxed WebAssembly
  component.

These files live together in a **definition set**: one directory. A node
serves exactly one **deployment**, a manifest naming every definition and
connection it runs, committed as a numbered revision. The newest committed
revision is the **head**. There is no other write path: no endpoint edits one
entity, and no endpoint reloads the engine.

Use `orion-server` for offline authoring and compilation. Use `orion-cli` for a
running instance. Treat `--help`, the instance's function catalog, and
validation responses as authoritative over examples in this skill.

## Choose the operating path

For read-only questions, inspect first and change nothing:

```bash
orion-cli health
orion-cli engine status
orion-cli deployments get <revision>
orion-cli cron status
orion-cli traces list
curl -s "$ORION_SERVER_URL/api/v2/admin/deployments" | jq '{head_revision, applied_revision}'
```

For authoring or changing a service:

1. Find the definition set the instance was deployed from. Edit files there.
2. Run `fmt --check`, `lint`, and `clippy` on the directory.
3. Dry-run each changed workflow with representative inputs, and run `test`.
4. Compile the set into a deployment directory.
5. Plan it against the target. Read every `+`, `=` and `-` line. A changed
   member shows as `+ id@2` beside `- id@1`.
6. Apply it only after those checks pass.
7. Exercise the channel and inspect live traces.

```bash
orion-server fmt --check ./definitions
orion-server lint ./definitions
orion-server clippy ./definitions
orion-server dry-run -w definitions/workflow.json -i sample.json
orion-server test ./definitions/tests      # the set's *.case.json files

orion-server compile ./definitions -o dist/deployment
orion-cli deployments plan -d dist/deployment
orion-cli deployments apply -d dist/deployment

orion-cli send orders -f request.json
orion-cli traces list --channel orders
```

Do not apply an untested set. Editing files affects nothing until an apply.
Applying the same directory again commits nothing: the CLI reports the head as
up to date.

## A deployment replaces the head

An apply commits the directory's members as the whole of what the node serves.
Anything the head deploys that the directory does not carry shows as `-` in
the plan and stops serving. Services that must run side by side belong in one
definition set. Never apply a directory built from part of the estate unless
removing the rest is the intent.

Never hand-build a manifest when you have a shell. `compile` assigns nothing
by hand, `apply -d` reuses unchanged revisions, and both refuse a set that
would not build. Hand-written manifests (`deployments apply -f`) are for the
cases `compile` does not express, such as a traffic split.

## Source form versus deployable JSON

A definition set may use `$from` shared values and `use` task fragments. Those
are source conveniences that `compile` resolves. The admin API takes one
canonical document at a time and refuses unresolved source forms with
`UNCOMPILED_SOURCE`. Never hand-expand them: compilation also namespaces
fragment task IDs and checks cross-file references.

`compile` writes `deployment.json`, `definitions/<hex>.json` and
`connections/<id>.json`. A workflow loses `workflow_id`, a channel loses
`channel_id` and `workflow_id`; the manifest member supplies them. A channel
`requires` its workflow, and a workflow `requires` every connection its tasks
name.

## Minimal shapes

```json
{
  "workflow_id": "order-processing",
  "name": "Order processing",
  "condition": true,
  "tasks": [
    {
      "id": "parse",
      "name": "Parse request",
      "function": {
        "name": "parse_json",
        "input": { "source": "payload", "target": "order" }
      }
    },
    {
      "id": "flag",
      "name": "Flag large order",
      "function": {
        "name": "map",
        "input": { "mappings": [
          { "path": "data.order.large", "logic": {
            ">": [{ "var": "data.order.total" }, 10000]
          } }
        ] }
      }
    }
  ]
}
```

```json
{
  "channel_id": "orders",
  "name": "orders",
  "channel_type": "sync",
  "protocol": "rest",
  "route_pattern": "/orders",
  "methods": ["POST"],
  "workflow_id": "order-processing"
}
```

Every entity needs an explicit id. The raw ingress body is **not** a JSONLogic
variable. It is called `payload` only by parsing functions. `parse_json` with
`source: "payload"` writes the parsed value beneath `data`. Expressions then
read paths such as `{"var":"data.order.total"}`.

## Connectors become connections

A connector is a file in the set. `compile` turns it into a connection revision
over a built-in driver. Every `env://` or `vault://` string in its config moves
to the connection's `secret_locators`. A literal credential is refused.

```json
{
  "id": "orders-db",
  "name": "orders-db",
  "connector_type": "db",
  "config": {
    "type": "db",
    "driver": "postgresql",
    "connection_string": "env://ORDERS_DB_URL",
    "operations": { "delete": false, "raw_write": false }
  }
}
```

A `db` connector must name its `driver`: `postgresql`, `mysql` or `sqlite`. The
server resolves each reference from its own environment or Vault, so the
target must define `ORDERS_DB_URL`. The deployment never holds the value.

## Runtime discovery

Never guess a task function's current input schema:

```bash
orion-cli functions list --quiet
orion-cli functions list --output json \
  | jq '.data[] | select(.name == "http_call")'
```

JSON output keeps the API envelope; list results are normally under `.data`.
Run `orion-cli <group> <command> --help` before relying on a flag not shown
here.

To run one workflow on a live node without deploying it, send it to
`POST /api/v2/admin/definitions/test` with `{"definition": …, "data": …}`.
Nothing is stubbed there: an `http_call` sends and a `db_write` writes. Prefer
`orion-server dry-run` with stubs unless live behaviour is the point.

## Release and rollback invariants

- An apply is compare-and-swap on the head. If another apply landed first,
  re-plan against the new head.
- `plan` and `apply` refuse a manifest that would not build with
  `400 DEPLOYMENT_WOULD_NOT_BUILD`, before anything is committed.
- A committed head that one node cannot build leaves that node serving its
  previous generation, with `engine_reload` degraded on `/health`.
- A member a node cannot load is quarantined while the rest serve. Check
  `orion-cli engine status` after every apply.
- Rollback rolls forward. `orion-cli deployments rollback` commits an earlier
  revision's manifest as a new revision. It does not undo writes or calls
  already made.
- In a cluster, every node converges on the head. Wait with
  `orion-cli deployments wait <revision> --target observed`.

```bash
orion-cli deployments rollback --target-revision 3 \
  --expected-head-revision 4 --idempotency-key rollback-4-to-3
```

## High-value correctness rules

- Use `orion-cli send --raw` for a channel whose
  `config.request.body_mode` is `payload`.
- Prefer `data_query` / `data_write` for portable, parameterized data access;
  reserve `db_read` / `db_write` for SQL the portable dialect cannot express.
- Keep credentials outside definitions. Use `env://` or `vault://` references
  in connectors, or declared `[secrets]` read with `{"secret": "name"}`. Use
  `[vars]` plus `var://name` for non-secret configuration.
- A `map` value is template-capable JSONLogic. An operator typo can become a
  literal object rather than an error. Lint, run clippy, and inspect unexpected
  object results before deploying.
- Channel guards differ by ingress. Do not assume HTTP authentication, caching,
  or origin checks apply to `channel_call` or cron.

## Cron: a workflow started by a clock

A `cron` channel has no caller. It declares a schedule and a fixed payload in
`transport_config` and registers no route. It must be `channel_type: "async"`.

```json
{
  "channel_id": "nightly-order-rollup",
  "name": "Nightly order rollup",
  "channel_type": "async",
  "protocol": "cron",
  "workflow_id": "order-rollup",
  "transport_config": {
    "schedule": "0 15 2 * * *",
    "timezone": "Asia/Kolkata",
    "payload": { "window": "previous_day" },
    "misfire_policy": "latest",
    "concurrency": { "policy": "forbid" }
  }
}
```

- **The expression always has six fields**: second, minute, hour,
  day-of-month, month, day-of-week. Five- and seven-field forms are refused,
  because `0 15 2 * * *` read as five fields would mean something else.
  `timezone` is an IANA name; abbreviations are refused.
- **The payload arrives where a request body does.** The workflow reads it with
  `parse_json` from `payload`, so a workflow moves between a route and a
  schedule unchanged. The schedule adds a platform-stamped `metadata.trigger`:
  `type`, `occurrence_id`, `scheduled_for`, `started_at`, `timezone`,
  `attempt`, `singleton_key`. Use `scheduled_for` as the idempotency key.
- **Everything caller-shaped is refused at authoring**: `methods`,
  `route_pattern`, `topic`, `consumer_group`, `config.auth`,
  `origin_allow_list`, `rate_limit`, `deduplication`, `cache`, `request`,
  `response`, `oauth2_login`. Only `validation_logic`, `backpressure`,
  `timeout_ms` and `tracing` still mean anything.
- **A cron channel is unreachable over HTTP and by `channel_call`.**
  `orion-cli cron trigger <channel>` is the manual path, and takes the same
  claim and singleton a scheduled run takes.
- **Every scheduled instant is a durable occurrence.** Read them with
  `orion-cli cron status|list|get`. `cron retry` is another attempt at the
  same occurrence, keeping its id and `scheduled_for`.
- `misfire_policy` is `skip`, `latest` (default) or `catch_up` (which requires
  `max_catch_up`). `concurrency.policy = "forbid"` makes the schedule
  non-overlapping across the cluster; contenders are recorded
  `skipped_singleton`. Non-overlap is not exactly-once, so a destination that
  must not apply work twice still needs to be idempotent.
- The node must have `cron.enabled` (the default). A deployed cron channel on a
  node with it off is quarantined and `components.cron` degrades.

## Plugins: a pure function as a task

A plugin is a WebAssembly component that adds a task function. Its world
imports nothing, so it can only transform the input it is given. Use one for a
codec or calculation that already exists as code. Never use one for I/O (that
stays `http_call` or a connector), or for a field rewrite a `map` expresses.

- Author with the `orion-plugin-sdk` crate: implement `Plugin::invoke(function,
  input) -> Result<Value, PluginError>`, call `export_plugin!(Type)`, build for
  `wasm32-unknown-unknown`, then `wasm-tools component new`.
- Describe it in `plugin.toml` beside the component: `abi =
  "orion:plugin@1.0.0"`, a reverse-domain `name`, and per function the
  `input_fields` in the built-ins' vocabulary (`kind`, `required`,
  `template_at`, `resolvable`). A function name is `<plugin>.<label>`.
- Put the `plugin.toml` and its component in the definition set. `compile`
  inlines the component, and the plugin deploys with the workflows that call it.
  The target needs `plugins.enabled = true`.
- Once deployed, its functions appear in `orion-cli functions list` with
  `source: "plugin"`. Read their schemas there.
- Offline: `orion-server lint|clippy|compile|dry-run|test --plugin-dir <dir>`.
  `dry-run` and `test` run the component for real. A function with no
  component fails as `PLUGIN_ARTIFACT_UNAVAILABLE`, and one with no manifest is
  reported unverifiable, not invalid.
- A `{"secret": …}` node is refused anywhere in a plugin task's input.

## Read only the reference needed

- Workflow structure, task groups, fragments, context, loops, failures, and
  revision selection: [references/workflows.md](references/workflows.md)
- Function selection, schema discovery, expressions in inputs, and egress
  boundaries: [references/functions.md](references/functions.md)
- JSONLogic operators, templates, secrets, and failure modes:
  [references/expressions.md](references/expressions.md)
- Routes, ingress guards, shaped responses, cookies, config references, and
  connector files: [references/channels.md](references/channels.md)
- Offline commands, compile, deployments, rollback, cron, output parsing, and
  troubleshooting: [references/cli.md](references/cli.md)

For implementation-specific detail, consult the matching version of Orion's
documentation or the repository source. The public documentation index is
<https://docs.goplasmatic.io/llms.txt>.
