<!-- description: A ten-minute session: install the Orion agent skill, describe a service in English, and watch Claude Code draft, dry-run, activate and roll it back safely. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-09-14 -->

# Build a service with Claude Code

This is the fastest way to experience "AI writes services, not code". Install the [Orion agent skill](./agent-skill.md), then describe the service you want. Claude drafts the workflow, dry-runs it, activates it and wires up the endpoint, while Orion's lifecycle rules keep every step reversible, in about ten minutes.

## What you will learn

- How the skill turns a paragraph of English into the same safe path you would follow by hand.
- What Claude reads back from real traces and engine status, rather than from what it thinks it deployed.
- How a change ships as a new version behind a canary rollout.
- Why rollback is rolling forward, and how to do it.

## Before you start

Tested with Orion 1.8.0. You need Claude Code, a POSIX shell, the Orion agent skill source, and [Orion and the CLI installed](../../get-started/install.md). Start the server locally, then configure the CLI and install the skill:

```bash
orion-cli config set-server http://localhost:8080
orion-cli health

mkdir -p .claude/skills
cp -r /path/to/Orion/skills/orion .claude/skills/
```

Start `claude` and run `/skills`; `orion` should be listed. The full setup, including the machine-wide install and what to do when admin auth is on, is in [Set up the agent skill](./agent-skill.md).

## 1. Build a service by describing it

Paste this into Claude Code:

> Create an Orion workflow called `order-triage` that parses incoming orders, flags any order over $10,000 with an alert message, and adds a `risk_level` field ("high" above 10000, "normal" otherwise). Test it with a realistic sample order **before** activating. Then create a REST channel `POST /orders` that uses it, activate everything, and send a $25,000 test order through it.

Watch the commands as Claude works. The sequence is the same safe path you would follow by hand:

```bash
orion-server lint order-triage.json                 # 1. offline, no server, no writes
orion-server clippy order-triage.json               #    advisory checks, only where certain
orion-cli workflows create -f order-triage.json     # 2. lands as a DRAFT
orion-cli workflows test order-triage -f sample.json --trace   # 3. dry-run
orion-cli workflows activate order-triage --dry-run # 4. pre-flight the transition
orion-cli workflows activate order-triage           #    engine hot-reloads
orion-cli channels create -f orders-channel.json    # 5. the endpoint
orion-cli channels activate orders
orion-cli send orders -f big-order.json             # 6. the test order comes back flagged
```

Your service is live, and the total code written is zero. Every step is a shell command. You can see exactly what ran, re-run any of it yourself, and read the same exit codes Claude read. `workflows activate --dry-run` exits `1` when the transition would be refused, which is what makes it a real gate rather than a formality.

## 2. Inspect and operate

Everything Orion records is queryable in the same session:

> Show me the recent traces for the orders channel. What did each task do on the last request?

> Is the engine healthy? How many workflows and channels are active?

Claude answers from `orion-cli traces list`, `traces get` and `engine status`. That is real observability data, not a summary of what it thinks it deployed.

## 3. Change it safely

Orion's governance makes iteration safe to delegate. Try:

> Lower the flag threshold to $5,000. Create a new version, dry-run it with an order at $7,500, and roll it out to 10% of traffic first.

Active versions are immutable, so Claude must cut a new version (`orion-cli workflows new-version`), test it, activate it, and then `orion-cli workflows rollout order-triage -p 10` for the canary.

## 4. Roll back

> Roll the orders workflow back to the previous version.

This is worth understanding rather than delegating blindly, because it is the one place the obvious mental model is wrong. Nothing reactivates an archived version in place. Status addresses a workflow id, not a version, and activating always promotes the current draft. Rolling back is rolling *forward* to the old content:

```bash
orion-cli workflows versions order-triage          # find the good version
orion-cli workflows new-version order-triage       # cut a fresh draft
orion-cli workflows update order-triage -f good.json
orion-cli workflows activate order-triage
```

Because active versions are immutable, the content being copied is exactly what it was when it last served. That is why rollback is trustworthy rather than hopeful. If you promote with [packages](../../operate/maintain/promotion.md), re-applying the previous artifact is the whole procedure.

## Why a skill and not a tool server

Orion shipped an MCP server inside `orion-cli` until 1.2.0. It was removed. Its HTTP transport exposed the full admin API on a port with no authentication of its own. All 58 of its tools were mirrors of commands the CLI already had.

A skill is a better fit for the same job. It is knowledge, loaded only when a task needs it and costing nothing until then, and the CLI is the hands. The agent inherits your credentials rather than holding its own. Every write lands in the audit log under your principal, and nothing new listens on a port.

## Recap

- The skill teaches an agent the safe path: lint, create a draft, test, preflight, activate. It runs the same commands you would.
- An agent reads real traces and engine status, so what it reports is what the instance holds.
- A change is a new version. A canary is a rollout percentage, sticky per caller.
- Rollback is rolling forward to content that is guaranteed to be what last served.

## Next steps

- [Set up the agent skill](./agent-skill.md): the install, what the skill knows, and how to scope an agent's access.
- [Prompt pack for any LLM](./prompt-pack.md): the same powers over the plain REST API, for an assistant with no shell.
- [The entity lifecycle](../../concepts/lifecycle.md): the draft, active and immutable rules that make delegating this safe.
- [Worked examples: prompt to service](./worked-examples.md): the prompts behind four shipped services, with the JSON each produced.
