<!-- description: Install the Orion agent skill so an AI assistant can author, test and deploy services through orion-cli — inheriting your access, with nothing new on a port. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Set up the agent skill

An *agent skill* is a folder of instructions an AI coding agent loads on demand. Orion ships one, in [`skills/orion/`](https://github.com/GoPlasmatic/Orion/tree/main/skills/orion), and the agent works through the CLI you already have. It teaches the JSON shapes, the draft-test-activate discipline, the command surface, and the traps that cost the most time.

The skill is knowledge; `orion-cli` is the hands. There is no extra process to run, no port to open, and nothing new holding your admin credentials.

## Before you start

You need a running Orion instance (see [Install and run Orion](../../get-started/install.md)), `orion-cli` on your `PATH` and pointed at it, and an agent that reads skills. Claude Code is the worked example below.

```bash
orion-cli config set-server http://localhost:8080
orion-cli health
```

> [!TIP]
> No skill support in your tool? [`skills/orion/SKILL.md`](https://github.com/GoPlasmatic/Orion/blob/main/skills/orion/SKILL.md) is plain Markdown; paste it in as a system prompt. The [prompt pack](./prompt-pack.md) is the smaller, self-contained alternative for an assistant that has no shell at all and must use the REST API directly.

## Install it

Clone the repository, or copy the folder out of a release, and drop it where your agent looks for skills. Per project, so everyone working on that repository gets it once it is committed:

```bash
mkdir -p .claude/skills
cp -r /path/to/Orion/skills/orion .claude/skills/
```

Or for every project on your machine:

```bash
mkdir -p ~/.claude/skills
cp -r /path/to/Orion/skills/orion ~/.claude/skills/
```

Start `claude` and run `/skills`; `orion` should be listed. You do not invoke it by hand. The agent loads it when a task looks Orion-shaped.

## What it knows

`SKILL.md` is what enters the agent's context up front. It holds the three primitives, the safe path, the rollout and rollback procedures, and the handful of traps that produce silent wrong answers. The rest is opened only when the task needs it:

| File | Covers |
|---|---|
| `references/workflows.md` | Tasks, groups, fragments, context, loops, failures, and version selection |
| `references/functions.md` | Function selection, live schema discovery, expression-capable inputs, and egress boundaries |
| `references/expressions.md` | JSONLogic evaluation, templates, secrets, and silent-failure edges |
| `references/channels.md` | Ingress guards, request/response behaviour, cookies, stored config, and connectors |
| `references/cli.md` | Offline checks, lifecycle operations, compilation, packages, and troubleshooting |

Anything discoverable at runtime is not restated in the skill. It tells the agent to run `orion-cli functions list` for a function's input schema, and `--help` for a command's flags. A table can drift; the instance cannot.

## Verify

Ask for something read-only first:

> Is my Orion instance healthy? How many workflows and channels are active?

The agent should reach for `orion-cli health` and `orion-cli engine status`. From there, [Build a service with Claude Code](./claude-code.md) walks a full session, from one paragraph of English to a live endpoint.

## What the agent can and cannot do

It runs `orion-cli` under your shell, so it has exactly the access you have, and no more. Every admin write lands in the audit log under your principal, and you can label a whole session's changes for later:

```bash
orion-cli --change-context "ticket=OPS-4412" workflows activate order-triage
```

Nothing is exposed to the network. To give an agent *less* than your access, give it its own API key on an instance with [admin authentication](../../operate/run/security.md) enabled. Set `ORION_API_KEY` in the environment you launch it from.

> [!NOTE]
> Orion used to ship an MCP server inside `orion-cli`. It was removed in 1.2.0: its HTTP transport put the full admin API on a port with no authentication of its own, and every tool it exposed was a mirror of a CLI command. The skill plus the CLI covers the same ground with a smaller attack surface. See the [changelog](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-cli/CHANGELOG.md).

## Next steps

- [Build a service with Claude Code](./claude-code.md): a full guided session.
- [Prompt pack for any LLM](./prompt-pack.md): for an assistant with no shell.
- [CLI](../../reference/cli/index.md): every command the skill drives.
- [The entity lifecycle](../../concepts/lifecycle.md): the draft and active rules that make delegating this safe.
