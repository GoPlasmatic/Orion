---
name: orion
description: Author, validate, test, deploy, inspect, roll out, and troubleshoot declarative services on Orion using orion-server and orion-cli. Use for Orion workflows, channels, connectors, packages, traces, or runtime operations; do not use for unrelated products also named Orion.
---

# Orion

Orion is a declarative services runtime. A service is described by JSON rather
than application code:

- a **workflow** is ordered business logic;
- a **channel** exposes a REST/HTTP route, a Kafka consumer, or a cron
  schedule, and binds it to a workflow;
- a **connector** names an external dependency and its policy;
- a **plugin** adds a custom task function as a sandboxed WebAssembly
  component.

Workflows, channels, and plugins are versioned (`draft -> active -> archived`).
Connectors are unversioned and update in place. A **package** is the deployable
closure of the workflows, channels, connectors, and plugins that make up one
service.

Use `orion-server` for local/offline authoring and package operations. Use
`orion-cli` for a running instance. Treat `--help`, the instance's function
catalog, and validation responses as authoritative over examples in this skill.

## Choose the operating path

For read-only questions, inspect first and do not mutate:

```bash
orion-cli health
orion-cli engine status
orion-cli workflows list
orion-cli channels list
orion-cli plugins list
orion-cli cron status
orion-cli traces list
```

For authoring or changing a service:

1. Inspect existing definitions and dependencies.
2. Edit or create source definitions locally.
3. Run `fmt --check`, `lint`, and `clippy`; dry-run with representative inputs.
4. Create or update a **draft** only.
5. Test the draft and inspect its trace.
6. Preflight activation with `--dry-run`.
7. Activate only after those checks pass.
8. Exercise the channel and inspect live traces.

```bash
orion-server fmt --check ./definitions
orion-server lint ./definitions
orion-server clippy ./definitions
orion-server dry-run -w workflow.json -i sample.json

orion-cli workflows create -f workflow.json
orion-cli workflows test order-processing -f sample.json --trace
orion-cli workflows activate order-processing --dry-run
orion-cli workflows activate order-processing

orion-cli channels create -f channel.json
orion-cli channels activate orders --dry-run
orion-cli channels activate orders
orion-cli send orders -f request.json
orion-cli traces list --channel orders
```

Do not activate untested definitions. Creating or editing a draft does not
affect serving traffic. Active definitions are immutable; use `new-version`,
edit the resulting draft, test it, then activate it.

If several related transitions must become visible together, pass
`--defer-reload` to each transition and run `orion-cli engine reload` once.
Package apply already performs dependency ordering and one reload.

## Authoring source versus deployable JSON

A definition directory may use `$from` shared values and `use` task fragments.
Those are source-language conveniences, not admin API wire format. Validate the
whole directory, then compile it before deployment:

```bash
orion-server compile ./definitions --name payments --version 1.5.0 -o payments.json
orion-server package plan -s https://target.example -f payments.json
orion-server package apply -s https://target.example -f payments.json
```

The single-document admin endpoints refuse unresolved source forms with
`UNCOMPILED_SOURCE`. Never hand-expand them; compilation also namespaces nested
fragment task IDs and validates cross-file references.

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

The raw ingress body is **not** a JSONLogic variable. It is called `payload`
only by parsing functions. `parse_json` with `source: "payload"` writes the
parsed value beneath `data`; expressions then read paths such as
`{"var":"data.order.total"}`.

## Runtime discovery

Never guess a task function's current input schema:

```bash
orion-cli functions list --quiet
orion-cli functions list --output json \
  | jq '.data[] | select(.name == "http_call")'
orion-cli workflows dependencies order-processing
```

JSON output retains the API envelope; list results are normally under `.data`.
Use `orion-cli <group> <command> --help` before relying on a flag not shown here.

## Release and rollback invariants

- Activating normally hot-reloads automatically.
- A partial rollout shares traffic with other active versions; the active
  percentages must form a valid partition before the channel can serve.
- Rollback means rolling forward: create a new draft containing the known-good
  archived content, test it, and activate it. Archived rows are never reactivated
  in place.
- Archiving the failing workflow does not fall back to an older version; it can
  leave its channels without a runnable workflow.
- Prefer reapplying a previous package artifact when packages are the promotion
  mechanism.

## High-value correctness rules

- Use `orion-cli send --raw` for a channel whose
  `config.request.body_mode` is `payload`.
- Prefer `data_query` / `data_write` for portable, parameterized data access;
  reserve `db_read` / `db_write` for SQL the portable dialect cannot express.
- Keep credentials outside definitions. Use connector secret references or
  declared `[secrets]`; use `[vars]` plus `var://name` for non-secret stored
  channel/connector configuration.
- Connector values are masked on read. Do not assume an exported masked value
  can be imported as a working credential.
- A `map` value is template-capable JSONLogic. An operator typo can become a
  literal object rather than an error; lint and inspect unexpected object
  results before deployment.
- Channel guards differ by ingress. Do not assume HTTP authentication, caching,
  or origin checks apply to Kafka or `channel_call`.

## Cron: a workflow started by a clock

A `cron` channel has no caller. It declares a schedule and a fixed payload in
`transport_config` — ordinary definition content, versioned and promoted like
everything else — and registers no route and no topic. It must be
`channel_type: "async"`.

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

- **The expression always has six fields** — second, minute, hour, day-of-month,
  month, day-of-week. Five- and seven-field forms are refused rather than
  guessed at, because `0 15 2 * * *` read as five fields would mean something
  entirely different. `timezone` is an IANA name; abbreviations are refused.
- **The payload arrives where a request body does.** The workflow reads it with
  `parse_json` from `payload`, so a workflow is portable between a route and a
  schedule with no change. What the schedule adds is a reserved,
  platform-stamped `metadata.trigger`: `type`, `occurrence_id`,
  `scheduled_for`, `started_at`, `timezone`, `attempt`, `singleton_key`. Use
  `scheduled_for` as the idempotency key — it is immutable across retries and
  unique per occurrence.
- **Everything caller-shaped is refused at authoring**, not stored and ignored:
  `methods`, `route_pattern`, `topic`, `consumer_group`, `config.auth`,
  `origin_allow_list`, `rate_limit`, `deduplication`, `cache`, `request`,
  `response`, `oauth2_login`. Only `validation_logic`, `backpressure`,
  `timeout_ms` and `tracing` still mean anything.
- **A cron channel is unreachable over HTTP and by `channel_call`.** Running it
  either way would execute the workflow outside the ledger and outside its
  lock. `orion-cli channels trigger <id>` is the deliberate manual path and
  takes the same claim and singleton.
- **Every scheduled instant is a durable occurrence.** Read them with
  `orion-cli cron status|list|get`; `cron retry` is another attempt at the same
  occurrence, keeping its id and `scheduled_for`.
- `misfire_policy` is `skip`, `latest` (default) or `catch_up` (which requires
  `max_catch_up`). `concurrency.policy = "forbid"` makes the schedule
  non-overlapping across the cluster; contending occurrences are recorded
  `skipped_singleton`, not dropped. Non-overlap is not exactly-once — work that
  must not be applied twice still needs an idempotent destination.
- The node must have `cron.enabled` (the default). An active cron channel on a
  node with it off is quarantined and `components.cron` degrades; activation is
  refused outright.

## Plugins: a pure function as a task

A plugin is a WebAssembly component that adds a task function; its world
imports nothing, so it can only transform the input it is given. Use one for
a codec or calculation that already exists as code, never for anything with
I/O (that stays `http_call` or a connector), and never for a field rewrite a
`map` expresses.

- Author with the `orion-plugin-sdk` crate: implement `Plugin::invoke(function,
  input) -> Result<Value, PluginError>`, call `export_plugin!(Type)`, build for
  `wasm32-unknown-unknown`, then `wasm-tools component new`.
- Describe it in `plugin.toml` beside the component: `abi = "orion:plugin@1.0.0"`,
  a reverse-domain `name`, and per function the `input_fields` in the same
  vocabulary as built-ins (`kind`, `required`, `template_at` for an
  engine-evaluated expression, `resolvable` for a `{"var": …}` fold). `output`
  is implicit; a function name is `<plugin>.<label>`.
- `orion-cli plugins create -f plugin.toml`, then `plugins activate`. The
  functions then appear in `orion-cli functions list` with `source: "plugin"`;
  discover their schemas there, never from memory.
- Offline: `orion-server lint|clippy|compile|dry-run|test --plugin-dir <dir>`.
  `dry-run` and `test` run the component for real; a function with no
  component fails as `PLUGIN_ARTIFACT_UNAVAILABLE`, and a function with no
  manifest is reported unverifiable, not invalid.
- Promotion: `package export --include-artifacts` carries the component;
  `apply` installs and activates plugins before workflows. A `plugin.toml` in
  a definition set compiles into the artifact.
- A `{"secret": …}` node is refused anywhere in a plugin task's input; a
  plugin never sees key material.

## Read only the reference needed

- Workflow structure, task groups, fragments, context, loops, failures, and
  version selection: [references/workflows.md](references/workflows.md)
- Function selection, schema discovery, expressions in inputs, and egress
  boundaries: [references/functions.md](references/functions.md)
- JSONLogic operators, templates, secrets, and failure modes:
  [references/expressions.md](references/expressions.md)
- Routes, ingress guards, shaped responses, cookies, connectors, and config
  references: [references/channels.md](references/channels.md)
- Offline commands, live CLI operations, package promotion, output parsing, and
  troubleshooting: [references/cli.md](references/cli.md)

For implementation-specific detail, consult the matching version of Orion's
documentation or the repository source. The public documentation index is
<https://docs.goplasmatic.io/llms.txt>.
