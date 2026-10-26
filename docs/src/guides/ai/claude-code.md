<!-- description: A ten-minute session: install the Orion agent skill, describe a service in English, and watch Claude Code write, dry-run, deploy and roll it back safely. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-10-25 -->

# Build a service with Claude Code

This is the fastest way to experience "AI writes services, not code". Install the [Orion agent skill](./agent-skill.md), then describe the service you want. Claude writes the workflow, dry-runs it, deploys it with its endpoint, and every step stays reversible, in about ten minutes.

## What you will learn

- How the skill turns a paragraph of English into the same safe path you would follow by hand.
- What Claude reads back from real traces and engine status, rather than from what it thinks it deployed.
- How a change ships as a new deployment revision, planned before it is applied.
- Why rollback is rolling forward, and how to do it.

## Before you start

Tested with Orion 1.12.0. You need Claude Code, a POSIX shell, the Orion agent skill source, and [Orion and the CLI installed](../../get-started/install.md). Start the server locally, then configure the CLI and install the skill:

```bash
orion-cli config set-server http://localhost:8080
orion-cli health

mkdir -p .claude/skills
cp -r /path/to/Orion/skills/orion .claude/skills/
```

Start `claude` and run `/skills`; `orion` should be listed. The full setup, including the machine-wide install and what to do when admin auth is on, is in [Set up the agent skill](./agent-skill.md).

## 1. Build a service by describing it

Paste this into Claude Code:

> Create an Orion workflow called `order-triage` in `./definitions` that parses incoming orders, flags any order over $10,000 with an alert message, and adds a `risk_level` field ("high" above 10000, "normal" otherwise). Dry-run it with a realistic sample order **before** deploying. Then add a REST channel `POST /orders` that uses it, deploy both, and send a $25,000 test order through it.

Watch the commands as Claude works. The sequence is the same safe path you would follow by hand:

```bash
orion-cli lint definitions                        # 1. offline, no server, no writes
orion-cli clippy definitions                      #    advisory checks, only where certain
orion-cli dry-run -w definitions/order-triage.json -i sample.json   # 2. run it offline
orion-cli compile definitions -o dist/deployment  # 3. the deployment directory
orion-cli deployments plan -d dist/deployment        # 4. what would change; commits nothing
orion-cli deployments apply -d dist/deployment       #    the node swaps the new revision in
orion-cli send orders -f big-order.json              # 5. the test order comes back flagged
```

Your service is live, and the total code written is zero. Every step is a shell command. You can see exactly what ran, re-run any of it yourself, and read the same exit codes Claude read. `lint` and `compile` exit non-zero on any error, so nothing invalid reaches the plan.

## 2. Inspect and operate

Everything Orion records is queryable in the same session:

> Show me the recent traces for the orders channel. What did each task do on the last request?

> Is the engine healthy? What revision is deployed?

Claude answers from `orion-cli traces list`, `traces get`, `engine status` and `deployments get`. That is real observability data, not a summary of what it thinks it deployed.

## 3. Change it safely

Orion's governance makes iteration safe to delegate. Try:

> Lower the flag threshold to $5,000. Dry-run it with an order at $7,500, show me the plan, then deploy it.

A deployed definition is immutable, so Claude edits the file and ships a new revision: `dry-run`, `compile`, then `deployments plan -d`. The plan lists the workflow as changed and commits nothing. Only `deployments apply -d` changes what the node serves.

## 4. Roll back

> Roll the deployment back to the previous revision.

This is worth understanding rather than delegating blindly, because the obvious mental model is wrong. Nothing reactivates an old revision in place. A rollback commits an earlier revision's manifest as a *new* revision, so history only moves forward:

```bash
orion-cli deployments rollback --target-revision 1 \
  --expected-head-revision 2 --idempotency-key rollback-order-triage
```

`--expected-head-revision` refuses the rollback if someone deployed in the meantime. The definitions it restores are stored by digest, so they are exactly what last served. That is why rollback is trustworthy rather than hopeful. See [Rollback](../../reference/admin-api/deployments.md#rollback).

## Why a skill and not a tool server

Orion shipped an MCP server inside `orion-cli` until 1.2.0. It was removed. Its HTTP transport exposed the full admin API on a port with no authentication of its own. All 58 of its tools were mirrors of commands the CLI already had.

A skill is a better fit for the same job. It is knowledge, loaded only when a task needs it and costing nothing until then, and the CLI is the hands. The agent inherits your credentials rather than holding its own. Every write lands in the audit log under your principal, and nothing new listens on a port.

## Recap

- The skill teaches an agent the safe path: lint, dry-run, compile, plan, apply. It runs the same commands you would.
- An agent reads real traces and engine status, so what it reports is what the instance holds.
- A change is a new deployment revision, planned before it is applied.
- Rollback is rolling forward to definitions that are guaranteed to be what last served.

## Next steps

- [Set up the agent skill](./agent-skill.md): the install, what the skill knows, and how to scope an agent's access.
- [Prompt pack for any LLM](./prompt-pack.md): the same powers over the plain REST API, for an assistant with no shell.
- [Deployment endpoints](../../reference/admin-api/deployments.md): the plan, apply and rollback rules that make delegating this safe.
- [Worked examples: prompt to service](./worked-examples.md): the prompts behind four shipped services, with the JSON each produced.
