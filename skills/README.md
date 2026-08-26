# Agent skills

An **agent skill** is a folder of instructions an AI coding agent loads on
demand. `orion/` teaches an agent to build and operate services on an Orion
runtime: the JSON shapes, the draft → test → activate discipline, the
`orion-cli` and `orion-server` command surface, and the traps that cost the
most time.

It is knowledge, not a service. The agent acts through the CLI you already
have installed, so there is no extra process to run, no port to open, and no
new thing holding your admin credentials.

## Install

Copy the folder where your agent looks for skills.

**Claude Code** — per project (commit it, and your team gets it too):

```bash
mkdir -p .claude/skills
cp -r path/to/Orion/skills/orion .claude/skills/
```

Or for every project on your machine:

```bash
mkdir -p ~/.claude/skills
cp -r path/to/Orion/skills/orion ~/.claude/skills/
```

Then ask for something Orion-shaped — "add an Orion channel that flags orders
over $10,000" — and the agent loads the skill on its own. `/skills` lists what
is registered.

**Another agent** that reads `SKILL.md` folders: point it at `skills/orion/`.
For anything else, `skills/orion/SKILL.md` is plain Markdown — paste it in as a
system prompt or project instruction.

## What the agent needs beside it

- `orion-cli` on `PATH` ([install](https://docs.goplasmatic.io/getting-started/install.html)).
- A reachable instance: `orion-cli config set-server http://localhost:8080`,
  or `ORION_SERVER_URL`.
- An API key if the instance has admin auth on — `ORION_API_KEY`.

The agent runs the CLI under your shell, so it has exactly the access you have,
and every admin write lands in the audit log under your principal. Nothing is
exposed to the network.

## Layout

```
orion/
├── SKILL.md                    # loaded first: concepts, the safe path, the command map
└── references/
    ├── workflows.md            # workflow JSON, task groups, fragments, data context, loops
    ├── functions.md            # the 27 task functions
    ├── expressions.md          # JSONLogic vocabulary and its silent-failure edges
    ├── channels.md             # channel config blocks and connectors
    └── cli.md                  # full command map, offline testing, compile, troubleshooting
```

`SKILL.md` is what enters the agent's context up front; the `references/` files
are opened only when the task needs them.

## Keeping it honest

The skill points at the instance for anything that can be discovered at
runtime — `orion-cli functions list` for input schemas, `--help` for flags —
rather than restating it. Prefer that over adding another table here: a table
can drift, the instance cannot.
