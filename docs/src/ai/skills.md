# Agent Skill Setup

An **agent skill** is a folder of instructions an AI coding agent loads on
demand. Orion ships one, in
[`skills/orion/`](https://github.com/GoPlasmatic/Orion/tree/main/skills/orion).
It teaches an agent the JSON shapes, the draft → test → activate discipline,
the `orion-cli` and `orion-server` command surface, and the traps that cost the
most time — then the agent does the work through the CLI you already have.

That is the whole design. The skill is knowledge; `orion-cli` is the hands.
There is no extra process to run, no port to open, and nothing new holding your
admin credentials.

## Before you start

- A running Orion instance — see [Install & Run](../getting-started/install.md).
- `orion-cli` on your `PATH`, from the same page, pointed at that instance:

  ```bash
  orion-cli config set-server http://localhost:8080
  orion-cli health
  ```

- An agent that reads skills. Claude Code is the worked example below.

> [!TIP]
> No skill support in your tool? [`skills/orion/SKILL.md`](https://github.com/GoPlasmatic/Orion/blob/main/skills/orion/SKILL.md)
> is plain Markdown — paste it in as a system prompt. The
> [Prompt Pack](./prompt-pack.md) is the smaller, self-contained alternative for
> an assistant that has no shell at all and must use the REST API directly.

## Install it

Clone the repo (or copy the folder out of a release) and drop it where your
agent looks for skills.

Per project — commit it, and everyone working on that repo gets it:

```bash
mkdir -p .claude/skills
cp -r /path/to/Orion/skills/orion .claude/skills/
```

For every project on your machine:

```bash
mkdir -p ~/.claude/skills
cp -r /path/to/Orion/skills/orion ~/.claude/skills/
```

Start `claude` and run `/skills` — `orion` should be listed. You do not invoke
it by hand: the agent loads it when a task looks Orion-shaped.

## What it knows

`SKILL.md` is what enters the agent's context up front — the three primitives,
the safe path, the rollout and rollback procedures, and the handful of traps
that produce silent wrong answers. The rest is opened only when the task needs
it:

| File | Covers |
|---|---|
| `references/workflows.md` | Workflow JSON in full: tasks, task groups, `terminal`, loops, the data context, request metadata, error branching |
| `references/functions.md` | All 27 task functions, grouped, with inputs for the common ones |
| `references/expressions.md` | The complete JSONLogic vocabulary and its silent-failure edges |
| `references/channels.md` | Channel JSON, every `config` guard block, and connector types |
| `references/cli.md` | Full `orion-cli` / `orion-server` map, offline testing, packages, troubleshooting |

Anything discoverable at runtime is *not* restated in the skill — it tells the
agent to run `orion-cli functions list` for a function's input schema, and
`--help` for a command's flags. A table can drift; the instance cannot.

## Verify it

Ask for something read-only first:

> Is my Orion instance healthy? How many workflows and channels are active?

The agent should reach for `orion-cli health` and `orion-cli engine status`.
From there, [Build a Service with Claude Code](./claude-code.md) walks a full
session, from one paragraph of English to a live endpoint.

## What the agent can and cannot do

It runs `orion-cli` under your shell, so it has exactly the access you have —
no more. Every admin write lands in the audit log under your principal, and you
can label a whole session's changes for later:

```bash
orion-cli --change-context "ticket=OPS-4412" workflows activate order-triage
```

Nothing is exposed to the network. If you want an agent to have *less* than
your access, give it its own API key on an instance with
[admin authentication](../operate/security.md) enabled and set `ORION_API_KEY`
in the environment you launch it from.

> [!NOTE]
> Orion used to ship an MCP server inside `orion-cli`. It was removed in 1.2.0:
> its HTTP transport put the full admin API on a port with no authentication of
> its own, and every tool it exposed was a mirror of a CLI command. The skill
> plus the CLI covers the same ground with a smaller attack surface. See the
> [changelog](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-cli/CHANGELOG.md).

## Related

- [Build a Service with Claude Code](./claude-code.md) — a full guided session.
- [Prompt Pack (any LLM)](./prompt-pack.md) — for an assistant with no shell.
- [CLI Reference](../reference/cli.md) — every command the skill drives.
- [The Entity Lifecycle](../concepts/lifecycle.md) — the draft/active rules that
  make delegating this safe.
