---
name: orion
description: Build and operate services on an Orion runtime — author workflow/channel/connector JSON, validate and dry-run it offline, deploy it through the draft→test→activate path, and read traces. Use whenever the task involves Orion, orion-cli, orion-server, an Orion workflow/channel/connector, or a request to add, change, test, roll out or roll back a service on an Orion instance.
---

# Orion

Orion turns JSON definitions into live REST/Kafka services. There is no code to
write and no deploy step: you declare the logic, and rate limiting, metrics,
health checks, retries and request tracing are already there.

You drive it with the **`orion-cli`** binary (a running instance, over HTTP) and
the **`orion-server`** binary (offline checks, no server needed). Both are
authoritative about themselves — `--help` on any subcommand is current in a way
this skill cannot be.

## The three primitives

| Primitive | What it is | Versioned? |
|---|---|---|
| **Workflow** | An ordered pipeline of tasks — the business logic | Yes: draft → active → archived |
| **Channel** | A service endpoint (REST route, HTTP, or Kafka topic) bound to one workflow by `workflow_id` | Yes, same lifecycle |
| **Connector** | A named connection to an external system (HTTP API, SQL, MongoDB, Redis, Kafka, SMTP, object storage), referenced from tasks by name | No — `update` writes in place |

The workflows, channels and connectors of one service form a **package**, the
unit `orion-server package` promotes between instances.

## Before anything else

```bash
orion-cli config set-server http://localhost:8080   # or --server / ORION_SERVER_URL
orion-cli health                                    # exits 1 if any component is degraded
```

If the instance has admin auth on, supply a key: `--api-key`, `ORION_API_KEY`,
or `orion-cli config set api_key <key>`. Precedence is flags > env >
`~/.orion/config.toml`.

Add `--output json` to any command when you need to parse the result rather
than read it. Add `--change-context "ticket=OPS-4412"` to stamp every audit row
the command writes.

## The safe path — follow it every time

This ordering is the whole point of Orion's lifecycle. Do not skip steps, and
do not activate something you have not dry-run.

```bash
# 1. Check offline first — no server, no writes, field-pathed errors
orion-server lint ./definitions          # a whole set: also resolves the references between files
orion-server lint workflow.json          # or a single file

# 2. Create — lands as a DRAFT, serving nothing
orion-cli workflows create -f workflow.json

# 3. Dry-run with realistic sample data, and read the trace
orion-cli workflows test my-workflow -f sample.json --trace

# 4. Pre-flight the transition, then activate (the engine hot-reloads)
orion-cli workflows activate my-workflow --dry-run
orion-cli workflows activate my-workflow

# 5. Same for the channel, which needs its workflow active first
orion-cli channels create -f channel.json
orion-cli channels activate my-channel --dry-run
orion-cli channels activate my-channel

# 6. Send real data and read what happened
orion-cli send my-channel -f request.json
orion-cli traces list --channel my-channel
```

Four rules that fall out of this:

1. **Create returns a draft.** Nothing serves traffic until it is activated.
2. **Active versions are immutable.** To change one, `orion-cli workflows
   new-version <id>`, edit the draft, test it, activate it.
3. **`--dry-run` exits 1 when the transition would be refused**, so it gates a
   script. It reports findings inside a `200`; a command that only checked the
   HTTP status would read a failing pre-flight as a passing one.
4. **`--defer-reload` batches.** Commit several changes with it, then
   `orion-cli engine reload` once so they go live together.

If the definitions use the set's authoring conveniences — `$from` for a shared
value, `use` for a task fragment — **compile before you deploy**. The admin API
takes one document with no set to resolve names against, so it refuses a
reference with `UNCOMPILED_SOURCE`:

```bash
orion-server compile ./definitions --name payments --version 1.4.0 -o dist/package.json
orion-server package apply -s https://prod.orion.internal -f dist/package.json
```

`--format dir` or `--format bulk` instead if you are importing per file rather
than promoting a package. `compile` runs `lint <dir>` first and writes nothing
if it fails.

## Roll out gradually

```bash
orion-cli workflows rollout my-workflow -p 10     # 10% of traffic
orion-cli workflows rollout my-workflow -p 100
```

`0` is refused — the accepted range is `1`–`100`, because a version serving no
traffic is an archived version, not an active one.

## Roll back

There is **no command that reactivates an archived version in place.** Status
addresses a workflow id, not a version, and activating always promotes the
current draft. Rolling back is rolling *forward* to the old content:

```bash
orion-cli workflows versions order-processing            # 1. find the good version
orion-cli workflows new-version order-processing         # 2. cut a fresh draft
orion-cli workflows update order-processing -f good.json # 3. put the known-good content in it
orion-cli workflows test order-processing -f sample.json # 4. confirm, then activate
orion-cli workflows activate order-processing
```

Because active versions are immutable, the content you copy is exactly what it
was when it last served. If you promote with packages, re-apply the previous
artifact instead — `orion-server package apply -s <url> -f payments-1.3.0.json`
is the whole procedure.

Two things that look like shortcuts are not: a rollout of `0` is refused, and
archiving the bad version does **not** fall back to its predecessor — archiving
takes every active version of that workflow out of service and quarantines any
channel bound to it.

## The shapes, minimally

A workflow — an id, a name, a match condition, and an ordered task list:

```json
{
  "workflow_id": "high-value-order",
  "name": "High-Value Order",
  "condition": true,
  "tasks": [
    { "id": "parse", "name": "Parse payload",
      "function": { "name": "parse_json", "input": { "source": "payload", "target": "order" } } },
    { "id": "flag", "name": "Flag order",
      "condition": { ">": [{ "var": "data.order.total" }, 10000] },
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.order.flagged", "logic": true }
      ] } } }
  ]
}
```

A channel — an endpoint bound to that workflow:

```json
{
  "channel_id": "high-value-orders",
  "name": "high-value-orders",
  "channel_type": "sync",
  "protocol": "rest",
  "route_pattern": "/high-value-orders",
  "methods": ["POST"],
  "workflow_id": "high-value-order"
}
```

## Traps that cost the most time

- **`payload` is not in the JSONLogic context.** `{"var": "payload.x"}`
  resolves to nothing. A workflow that reads request data must start with
  `parse_json` (`source: "payload"`), or every later condition sees an empty
  context.
- **A misspelled operator inside a `map` mapping is silent.** JSONLogic cannot
  tell `{"uppr": [...]}` from a data object, so it is written through as a
  literal — no error, `200` to the caller. When a mapping yields an object
  where you expected a scalar, check the operator name first. Conditions
  compile strictly and *do* error on the same typo.
- **Activation hot-reloads on its own.** Call `engine reload` only after
  `--defer-reload`, or to force a rebuild.
- **`orion-cli send` needs `--raw`** for a channel with
  `request.body_mode = "payload"`; otherwise the default `{"data": …}` envelope
  arrives as a key literally named `data`.
- **Connector secrets are masked on read**, so an exported connector needs its
  credentials supplied again on import.
- **`$from` and `use` are authoring syntax, not wire format.** A POST carrying
  either is refused with `UNCOMPILED_SOURCE`, however deep in the document it
  sits. Run `orion-server compile <dir>` and send its output; do not hand-inline
  the reference.
- **`env://` works in five workflow fields, not everywhere.** Only `crypto.key`,
  `jwt_sign.key` and `jwt_verify`'s `keys` / `issuer` / `audience` resolve a
  secret reference; anywhere else it is sent on as that literal text, so a POST
  carrying one is refused with `UNRESOLVED_SECRET_REF` naming the field. Put the
  value on a connector instead — the connector holds the credential and the
  workflow names the connector.
- **A value that varies by environment is `[vars]` or `[secrets]`, never a
  literal in the definition.** The operator declares both in the config file; a
  workflow reads a var as `{"var": "metadata.vars.name"}` and a secret as
  `{"secret": "name"}`. The difference is traces: a var is stamped into every
  message and *is* recorded, a secret is held by the engine and cannot be. So a
  `map` mapping or a `log` field reading a secret is refused when the engine is
  built, as is a name the instance does not declare — both quarantine the
  channel. Read a secret in `crypto.key`, `jwt_sign.key` or a task condition,
  and nowhere that gets written back.

## Get the authoritative schema at runtime

Never guess a function's input shape. Ask the instance:

```bash
orion-cli functions list                  # name, category and description, as a table
orion-cli functions list --quiet          # just the valid names, one per line
orion-cli functions list --output json \
  | jq '.data[] | select(.name == "http_call")'   # one function's full input schema
```

`--output json` prints the server's response envelope, so the array is at
`.data` — that holds for every list command, not just this one.

`orion-cli workflows dependencies <id>` shows what a workflow actually
references — connectors, and `channel_call` targets.

## References

Read these as needed; they are not loaded until you open them.

| File | Covers |
|---|---|
| `references/workflows.md` | Workflow JSON in full: tasks, task groups, `terminal`, fragments, loops, the data context, metadata, error branching, rollout, matching |
| `references/functions.md` | All 27 task functions, grouped, with the inputs for the common ones |
| `references/expressions.md` | The complete JSONLogic operator vocabulary and its silent-failure edges |
| `references/channels.md` | Channel JSON, the config blocks (auth, rate limit, dedup, cache, validation, response shaping), and connector types |
| `references/cli.md` | Full `orion-cli` and `orion-server` command map, offline testing, shared definitions and `compile`, packages, and troubleshooting |

For anything beyond these, the docs site is <https://docs.goplasmatic.io> and
it serves `llms.txt` and `llms-full.txt` for machine consumption.
