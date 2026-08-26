# Upgrading to 1.2.0

This page is for operators and authors upgrading an existing Orion deployment
from **1.1.x** to **1.2.0**. It covers only what *changes behaviour*. The new
capabilities — task groups and `terminal`, shared definition sources,
`orion-server lint <dir>`, the offline call log, request `metadata` in a case
file — are in the
[CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/CHANGELOG.md).

**1.2.0 is a minor release and behaves like one.** No config key was renamed
or removed, no API path moved, no metric was renamed, and the release ships
**no database migrations** — the schema is byte-identical to 1.1.0's, so a
rollback needs no schema work. Six changes can reach you. **One is breaking for
offline test suites, and two can turn a green CI gate red** — those are the
rows to read first.

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

`orion-server preflight` **does not cover this release.** Its rules are the
0.3 → 1.0 breaks; a clean run says nothing about the rows above. Each section
below carries its own detection command.

---

## 1. BREAKING: an `expect` path must name its root

**What changed.** In a `*.case.json`, a leading `data.` used to be optional.
That made the case file the only surface in Orion accepting an unrooted path —
every mapping `path` in every shipped workflow already spells one — and the
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
advisory, and `env://` collection — and every finding now carries a stable
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
want the old all-findings-are-fatal behaviour, add `--deny-warnings` — but note
it counts warnings, not inventory notes, so a set that authors secrets with
`env://` no longer fails for doing the documented thing.

---

## 3. A workflow with no tasks is refused at create

**What changed.** `"tasks": []` used to be accepted with a `201`. It parses
cleanly and then fails the engine's own `Workflow::validate()`, which runs
during the engine **build** — so, exactly like a duplicate task id, activating
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
**not** `enrich` — so a stored task naming `enrich` used to build cleanly and
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
input-validates — which is 18 of the 27 names a workflow may use. The nine it
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
`input_fields` will need to branch on `source` — or simply tolerate the key's
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

## Related

- [Upgrades](./upgrades.md) — the version-independent procedure.
- [Troubleshooting](./troubleshooting.md) — quarantine, degraded health, and
  the rest of the symptom index.
- [Test Workflows Offline](../build/testing.md) — the case-file surface row 1
  changes.
- [Workflow Reference](../reference/workflows.md) — the workflow and task
  contract, including task groups and `terminal`.
