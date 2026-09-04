<!-- description: Upgrading Orion 1.1.x to 1.2.0 — only what changes behaviour: task groups and terminal steps, shared definition sources, and the MCP server's removal. -->
# Upgrading to 1.2.0

This page is for operators and authors upgrading an existing Orion deployment
from **1.1.x** to **1.2.0**. It covers only what *changes behaviour*. The new
capabilities — task groups and `terminal`, shared definition sources,
`orion-server lint <dir>`, the offline call log, request `metadata` in a case
file — are in the
[CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/CHANGELOG.md).

**1.2.0 is a minor release and behaves like one.** No config key was renamed
or removed, no API path moved, no metric was renamed, and the release ships
**no database migrations**: the schema is byte-identical to 1.1.0's, so a
rollback needs no schema work. Seven changes can reach you. **Two are breaking
— the CLI's MCP server is gone, and offline test suites need every `expect`
path rooted, and two more can turn a green CI gate red.** Those are the rows
to read first.

The version-independent procedure — back up, preflight, validate config,
migrate, roll — is on [Upgrades](./upgrades.md).

---

## Before you start

| # | Check | Applies to you if |
|---|-------|-------------------|
| 1 | [Root every `expect` path in your `*.case.json` files](#1-breaking-an-expect-path-must-name-its-root) | You run `orion-server test` — **this one fails cases that used to pass, and some that used to pass wrongly** |
| 2 | [Re-run `package lint` on your artifacts](#2-package-lint-checks-more-and-fails-on-less) | Your promotion pipeline gates on `orion-server package lint` |
| 3 | [Check for a workflow with an empty task list](#3-a-workflow-with-no-tasks-is-refused-at-create) | Anything automated creates workflows in two steps — a placeholder, then the tasks |
| 4 | [Check for a stored task naming `enrich`](#4-a-workflow-the-engine-cannot-dispatch-quarantines-its-channel) | Your estate predates 1.0, or you import definitions from an instance that does |
| 5 | [Tolerate a missing `input_fields`](#5-adminfunctions-lists-every-function-not-just-the-schema-registry) | You consume `GET /api/v1/admin/functions` in tooling |
| 6 | [Nothing — a limit was loosened](#6-task-groups-may-nest-the-full-8-levels) | You author deeply nested task groups |
| 7 | [Replace `orion-cli mcp serve`](#7-breaking-the-clis-mcp-server-is-removed) | You connect an AI client to Orion through the CLI's MCP server |

One further behaviour change landed *after* 1.2.0:
[fragment ids inside a task group are namespaced](#after-120-fragment-ids-inside-a-task-group).
Read it if you author task fragments and are going to 1.2.1 or later.

`orion-server preflight` **does not cover this release.** Its rules are the
0.3 → 1.0 breaks; a clean run says nothing about the rows above. Each section
below carries its own detection command.

---

## 1. BREAKING: an `expect` path must name its root

**What changed.** In a `*.case.json`, a leading `data.` used to be optional.
That made the case file the only surface in Orion accepting an unrooted path —
every mapping `path` in every shipped workflow already spells one, and the
cost of the exception was silence. `metadata.foo` was read as
`data.metadata.foo`, came back absent, and because an expected `null` matches
an absent path, `"metadata.foo": null` **passed**. A typo'd root
(`"dat.order.id"`) failed the same way.

A path naming no root now fails the case *before* the workflow runs, with the
fix in the message.

**How you'll notice.** `orion-server test` fails cases that previously passed.
Some of those were passing wrongly — a green assertion against a path nothing
ever wrote.

**What to do.** Prepend `data.` to every unrooted key:

```bash
jq '.expect |= with_entries(
      if (.key | test("^(data|metadata|temp_data|calls|audit_trail)([.\\[]|$)"))
      then . else .key |= "data." + . end)' case.json
```

Then re-read the cases the migration touched: one that asserted
`metadata.something` was reading the wrong document, so the value it expects
may never have been checked at all. See
[Test Workflows Offline](../build/testing.md#every-expect-path-names-its-root).

---

## 2. `package lint` checks more, and fails on less

**What changed.** `package lint` and `lint <dir>` now run one shared
cross-reference pass. `package lint` gains the checks the artifact form never
had — connector type, duplicate `route_pattern`, the unresolvable-JSONLogic
advisory, and `env://` collection, and every finding now carries a stable
`check` id and a severity.

The severity is the part that cuts both ways:

- **New error-severity checks can fail an artifact that passed before.**
  Connector type mismatches and duplicate `route_pattern`s were invisible to
  the artifact lint and are errors now.
- **Warnings and notes no longer fail.** The exit code keys on errors alone, so
  the unresolvable-JSONLogic advisory and the `env://` inventory print without
  breaking the gate. Use `--deny-warnings` to restore a strict gate.

**How you'll notice.** A promotion pipeline gating on `package lint` either
starts failing on a real cross-reference problem it could not see before, or
starts passing an artifact whose only findings were advisory.

**What to do.** Run `orion-server package lint -f <artifact>` against your
current artifacts before upgrading the pipeline, and fix what it names. If you
want the old all-findings-are-fatal behaviour, add `--deny-warnings`, but note
it counts warnings, not inventory notes, so a set that authors secrets with
`env://` no longer fails for doing the documented thing.

---

## 3. A workflow with no tasks is refused at create

**What changed.** `"tasks": []` used to be accepted with a `201`. It parses
cleanly and then fails the engine's own `Workflow::validate()`, which runs
during the engine **build**, so, exactly like a duplicate task id, activating
one took down every channel on every node rather than quarantining itself. It
is now a `400` at create, which is what the
[Workflow Reference](../reference/workflows.md#the-workflow-object) has always
said (`tasks` is an "ordered, non-empty list").

**How you'll notice.** Any automation that creates a workflow as a placeholder
and fills in the tasks with a follow-up `PUT` now fails on the first call.

**What to do.** Create the workflow with its tasks in one call. Drafts are free
and unlimited, so there is no reason to stage an empty one. To find stored rows
that would fail a re-save:

```bash
curl -s -H "Authorization: Bearer $ORION_ADMIN_TOKEN" \
  http://localhost:8080/api/v1/admin/workflows | \
  jq -r '.data[] | select((.tasks | length) == 0) | .workflow_id'
```

Existing stored rows are not rewritten by the upgrade. An active one was
already breaking your engine build; a draft one simply cannot be saved again
until it has a task.

---

## 4. A workflow the engine cannot dispatch quarantines its channel

**What changed.** A stored workflow is now screened against the **real handler
registry** at load. Previously the screen tested a hand-kept name list, which
only covered names that reach the engine's "custom function" path.

`enrich`, `http_call` and `publish_kafka` do not: they deserialize into typed
built-in variants, so the engine accepts them at build time whether or not a
handler is registered. Orion registers `http_call` and `publish_kafka` but
**not** `enrich`, so a stored task naming `enrich` used to build cleanly and
then fail every request with `FunctionNotFound`. Its channel is now
[quarantined](./troubleshooting.md#a-channel-answers-503-failed-to-load-and-is-not-being-served) instead.

**How you'll notice.** A channel that answered requests — badly, failing every
one — now answers `503` at every ingress, appears under `/health`'s
`channels.quarantined`, and puts the `channels` component into `degraded`.
Nothing that worked stops working; a failure moves from per-request to
per-channel, where it is visible.

**What to do.** Find any stored task naming a function the engine will not run:

```bash
curl -s -H "Authorization: Bearer $ORION_ADMIN_TOKEN" \
  http://localhost:8080/api/v1/admin/workflows | \
  jq -r '.data[] | .workflow_id as $w | [.. | objects | select(has("function"))
         | .function.name] | unique | map(select(. == "enrich")) | .[] |
         "\($w): \(.)"'
```

Creation has refused `enrich` since 1.0, so a row carrying one predates that or
arrived by import. Replace it with `http_call` against the same upstream.

---

## 5. `/admin/functions` lists every function, not just the schema registry

**What changed.** The endpoint served the schema registry — the functions Orion
input-validates, which is 18 of the 27 names a workflow may use. The nine it
omitted (`map`, `filter`, `log`, `parse_json`, `parse_xml`,
`validation`/`validate`, `publish_json`, `publish_xml`) are the most-used
functions there are, so anything completing from this endpoint offered the
connector functions and none of the ones people type.

All 27 are listed now. Engine built-ins carry `source: "engine"` and **omit**
`input_fields` entirely — omitted rather than nulled, because absence is the
honest encoding for "this function declares no schema". Orion's own handlers
carry `source: "orion"` and their schema as before, and `validation` carries
`validate` in `aliases` rather than appearing twice.

**How you'll notice.** More rows, a new `source` field, and some rows with no
`input_fields` key at all.

**What to do.** This is additive, but a consumer that assumed every row carries
`input_fields` will need to branch on `source`, or simply tolerate the key's
absence. Nothing else about the endpoint moved.

---

## 6. Task groups may nest the full 8 levels

**What changed.** A loosening, listed for completeness. Orion refused a
workflow nesting [task groups](../reference/workflows.md#task-groups) exactly 8
deep, with a message claiming the engine would refuse to build it. It would
not — Orion counted nesting from a different base than the engine does, so the
enforced limit was one level tighter than the real one and one level tighter
than the documentation said.

**What to do.** Nothing. A definition that was refused is now accepted; nothing
that was accepted is now refused.

---

## 7. BREAKING: the CLI's MCP server is removed

**What changed.** `orion-cli mcp serve` is gone, along with the 58 MCP tools it
exposed. The subcommand no longer exists, the `ghcr.io/goplasmatic/orion-cli`
image no longer opens port 8081, and the entry in the MCP registry is no longer
published.

Two reasons. The first is a security one and is why this landed in a minor
release rather than waiting: **the HTTP transport had no authentication of its
own.** `mcp serve --http` bound `0.0.0.0:8081` and served the full admin API —
create, activate, delete, and read every trace — to anything that could reach
the port, using the operator's own `ORION_API_KEY` upstream. Anyone who ran it
outside loopback, or used the published `docker-compose.yml`, exposed their
whole control plane. The second is that every tool was a hand-written mirror of
an `orion-cli` command over the same transport, so the surface cost two edits
per change and earned nothing.

**Detection.** You are affected if anything you run references the subcommand:

```bash
grep -rn '"mcp".*"serve"\|mcp serve' \
  ~/.claude.json .mcp.json .cursor/ docker-compose*.yml 2>/dev/null
```

A stdio client shows it as a server that fails to start; an HTTP client as a
connection refused on 8081.

**What to do.** Install the [agent skill](../ai/skills.md) and let the assistant
drive `orion-cli` directly:

```bash
mkdir -p .claude/skills
cp -r /path/to/Orion/skills/orion .claude/skills/
```

The skill is knowledge rather than a service, so the assistant runs the CLI
under your shell: it inherits your access instead of holding its own, every
admin write lands in the audit log under your principal, and nothing listens on
a port. [Agent Skill Setup](../ai/skills.md) covers the install and how to give
an agent scoped credentials.

If your client cannot run a shell — Claude Desktop, Cursor's chat panel — there
is no in-product replacement. Use the [Prompt Pack](../ai/prompt-pack.md)
against the REST API, or stay on 1.1.0 until you can move the workflow to a
client with a terminal.

**While you are here.** If you ran `mcp serve --http` on anything reachable
beyond loopback, treat the admin key it carried as exposed and rotate it —
`admin_auth.api_keys` in the [configuration](../reference/configuration.md), and
[Audit Logs](./audit-logs.md) for what was done with it.

---

## After 1.2.0: fragment ids inside a task group

*Applies from 1.2.1. Skip it unless a definition set of yours declares
`fragments`.*

A fragment's task ids are prefixed with the call-site id, so that a fragment
cannot collide with the workflow including it or with a second instance of
itself. Through 1.2.0 that held only while every step in the fragment was a
plain task: when a fragment contained a **task group**, only the group's own
`id` was rewritten and the ids of the tasks inside it were emitted verbatim
into the host workflow's namespace. Using such a fragment twice produced
duplicate step ids; using it once collided with any host task sharing a name
with one of its nested tasks, and either was refused with
`DUPLICATE_TASK_ID`, which fails the whole engine reload rather than one
workflow. Nothing showed the author it was coming, because the colliding name
is private to the fragment.

Every id a fragment contributes is now prefixed, at every depth, flat
(`{call-site}.{id}`) rather than one segment per enclosing group, so a
fragment's `refused`/`deny` inside a group become `_session.refused` and
`_session.deny`. In the same walk, a `use` nested inside a task group is now
refused (`shared.fragment_nested`) instead of surviving unexpanded into a
workflow the engine cannot parse.

**What to do.** Detect it first — a set is affected only if a fragment it
declares contains a task group:

```bash
# 1. Does the set declare fragments at all?
grep -rl '"fragments"' ./definitions

# 2. Recompile with the new binary, then ask the instance what moved
orion-server compile ./definitions --name <pkg> --version <next> -o dist/pkg.json
orion-server package diff -s https://prod.orion.internal -f dist/pkg.json
```

`diff` exits `1` on drift and names the entities that differ from what the
instance stores — a workflow whose fragment ids moved is one of them, alongside
any other edit made since the last apply. An exit of `0` means the recompile
changed nothing and none of this reaches you.

Otherwise:

- **Bump the package version.** Recompiled workflows carry different task ids
  and therefore a different `content_hash`, and an applied package version is
  content-immutable — re-applying at the same version returns `409`.
- **Update `expect_tasks` assertions** in `*.case.json` files that name a
  nested fragment id; they need the call-site prefix now.
- **Drop any hand-written prefix.** An author who worked around the collision
  by pre-prefixing a fragment's nested ids gets it applied twice, and can
  remove theirs.

Trace step ids, `metadata.progress` keys and the per-task metric label follow
the task ids, so a dashboard or alert pinned to a nested fragment id needs the
prefixed name.

---

## Related

- [Upgrades](./upgrades.md): the version-independent procedure.
- [Troubleshooting](./troubleshooting.md): quarantine, degraded health, and
  the rest of the symptom index.
- [Test Workflows Offline](../build/testing.md): the case-file surface row 1
  changes.
- [Workflow Reference](../reference/workflows.md): the workflow and task
  contract, including task groups and `terminal`.
- [Agent Skill Setup](../ai/skills.md): what replaces the MCP server row 7
  removes.
