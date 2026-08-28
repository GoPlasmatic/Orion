# CLI reference

Two binaries. `orion-cli` drives a **running instance** over HTTP.
`orion-server` also carries **offline** subcommands that need no server, no
database and no config — those are the ones to reach for first, because they
cost nothing and fail fast.

`--help` on any subcommand is authoritative and current; this file is the map.

## Global flags (`orion-cli`)

Precedence for every setting: **flags > environment > `~/.orion/config.toml`**.

| Flag | Env | Notes |
|---|---|---|
| `--server <url>` | `ORION_SERVER_URL` | Instance base URL |
| `--api-key <key>` | `ORION_API_KEY` | Admin auth |
| `--api-key-header <name>` | `ORION_API_KEY_HEADER` | Default: `Authorization` with a `Bearer` prefix |
| `--change-context <ctx>` | `ORION_CHANGE_CONTEXT` | Audit label, e.g. `ticket=OPS-4412`; recorded on every audit row the command writes |
| `--output <fmt>` | — | `table` (default), `json`, `yaml` |
| `--quiet` | — | Only ids or minimal info |
| `--verbose` | — | Full response bodies |
| `--no-color` | `NO_COLOR` | |
| `--yes` | — | Skip confirmation prompts |

`--output json` prints the server's **whole response envelope**, so lists are
at `.data`:

```bash
orion-cli workflows list --output json | jq -r '.data[].workflow_id'
```

## Paging

Every `list` pages: 50 rows by default, 1000 max. The footer says
`Showing 50 of 3120` when the page is short, so a truncated listing never reads
as a complete one.

`--limit`, `--offset` everywhere; `--sort-by` / `--sort-order` on `workflows`,
`channels`, `connectors`, `traces`. Accepted sort columns differ per resource.

`traces list` adds `--cursor` (keyset paging from a previous page's
`next_cursor`; default ordering only, mutually exclusive with `--offset`,
cheaper on a large table) and `--include-total` (off by default — the count is
a full scan).

## Offline, before you touch a server

```bash
orion-server fmt ./definitions             # format to the one house style; --check diffs and exits 1
orion-server lint ./definitions            # a whole set — also resolves references between files
orion-server clippy ./definitions          # advisory rules beyond lint, said only when certain; --list, --explain <rule>
orion-server lint workflow.json            # one file
orion-server lint ./defs --deny-warnings   # advisories fail too
```

A **directory is linted as a set**: every channel, workflow and connector under
it is validated *and* the references between them resolve — a `channel_call`
target, a task's connector and its type, a channel's `workflow_id`, duplicate
ids, names and routes. Those are the errors a per-file lint cannot see.
Entities are found by shape: an object with `tasks` is a workflow,
`connector_type` a connector, `channel_type`/`protocol` a channel.

Use `--requires-channel` / `--requires-connector` (repeatable) for a set that
genuinely depends on something deployed elsewhere. Each finding carries a
stable `[check]` id so a pipeline can grandfather one rule without silencing
the rest. `note:` findings are exit-neutral inventory, not defects.

```bash
orion-server dry-run -w wf.json -i input.json --stubs stubs.json --metadata md.json
```

Executes the workflow in an in-process engine and prints the per-task trace.
Connector tasks are answered from `--stubs`; without a matching stub the task
fails and *names the stub it needs*. The stub's inner key is the task's
`connector` (or `channel` for `channel_call`); `"*"` matches any. Output
carries `data`, `metadata`, `temp_data`, `audit_trail`, `calls`, plus `trace`,
`matched` and `errors`.

```bash
orion-server test examples/workflow-tests        # a directory of *.case.json
orion-server test one.case.json
```

A case:

```json
{
  "name": "flags high-value orders",
  "workflow": "high-value-order.json",
  "input": { "order_id": "ORD-1", "total": 25000 },
  "stubs": { "http_call": { "crm": { "name": "Ada" } } },
  "expect": { "data.order.flagged": true }
}
```

| Field | Meaning |
|---|---|
| `workflow` | Path to the workflow JSON, **relative to the case file** |
| `input` | The bare payload |
| `stubs` / `stubs_file` | Inline connector stubs, or a path |
| `metadata` | Request metadata as the HTTP ingress would have built it |
| `expect` | Rooted dotted paths → expected values |
| `expect_errors` | Expected task-error codes. **Defaults to empty**, so a workflow that starts failing cannot pass silently |
| `expect_calls` | Expected connector calls per function, in order — asserts what a task *tried to send*, with payloads resolved as the real handler resolves them |
| `expect_tasks` | Ids of the tasks that ran, in order. Unchecked when omitted |

Add `--definitions <dir>` to `dry-run` and `test` when the set uses shared
definitions — a `$from` value or a `use` fragment. Linting a *directory* finds
the catalog on its own; only the single-file commands need the flag.

Other offline `orion-server` commands: `validate-config`, `migrate
[--dry-run]`, `test-connectivity`, `preflight` (scans the stored estate for
upgrade breaks), `dump-openapi`.

## Shared definitions, and compiling a set

A definition set can say a thing once. Any JSON file in the set carrying
`constants`, `errors` or `fragments` (and no entity field) is a **shared
document** — found by shape, split across as many files as you like, and a name
defined twice is an error rather than last-write-wins.

```json
{ "input": { "$from": "constants.db", "collection": "users" } }
{ "id": "_session", "use": "require-session", "with": { "deny_message": "Please sign in." } }
```

- **`$from` splices** the named value into the object it sits in. It is a merge,
  not a substitution, and **siblings win** — a call site overrides one field
  without copying the rest. A `$from` alone in its object, naming a scalar or
  array, replaces the whole node.
- **`use` expands a fragment**, a named and parameterised task sequence.
  `with` supplies its `$param` values; a parameter with no `default` is required
  at every call site.

Two rules about fragment ids, both worth knowing before you author one:

- **Every** id the fragment contributes is prefixed with the call-site id, at
  every depth — a task group's own id and its members' alike (`_session.check`,
  `_session.deny`). The prefix is flat, one segment, not one per enclosing
  group. That is what lets a fragment be used twice in one workflow.
- A fragment **cannot use another fragment**, at any depth. Nesting one inside a
  task group is refused with `shared.fragment_nested`, not quietly expanded.

Both resolve **before** validation, so `lint`, `dry-run` and `test` all check
and run the expanded form.

**Deploying a set that uses either needs a compile step.** The admin API takes
one document and has no set to resolve names against, so it refuses a reference
with `UNCOMPILED_SOURCE` rather than guessing — naming the reference, its
authored coordinate, and the command that resolves it:

```bash
orion-server compile ./definitions --name payments --version 1.4.0 -o dist/package.json
orion-server package apply -s https://prod.orion.internal -f dist/package.json
```

`compile` runs every gate `lint <dir>` runs first and emits nothing if that
fails. Three output shapes:

| `--format` | Output | Consumed by |
|---|---|---|
| `artifact` (default) | One promotion artifact, hashed exactly as `package export` hashes one | `orion-server package plan\|apply\|diff` |
| `dir` | The input tree mirrored, one file per entity, shared documents consumed | a POST per file — `orion-cli workflows import -f …` |
| `bulk` | `connectors.json`, `workflows.json`, `channels.json` | the bulk import endpoints, in that order |

`--name` and `--version` are required for `artifact` and meaningless for the
other two. `artifact` marks workflows and channels `activate: true` (a directory
carries no stored status); `--no-activate` applies them as drafts. `dir` and
`bulk` emit request bodies, so an entity there may omit its id exactly as a
hand-written POST may — `artifact` needs explicit ids, because `apply` activates
by id.

## Managing resources

`workflows` (alias `rules`), `channels` (`ch`), `connectors` (`conn`) share a
shape: `list`, `get`, `create`, `update`, `delete`, `validate`, `export`,
`import`. Input is `-f <file>`, `-d <json>` or `--stdin`.

Workflows and channels add the lifecycle: `activate`, `archive`, `versions`,
`new-version`. Only **drafts** accept `update`. Workflows also have
`dependencies` (alias `deps`), `rollout -p <n>`, `test`, and `diff`.
Connectors add `enable`, `disable`, `test` (a real probe of the target),
`circuit-breakers`, `reset-breaker <connector>:<channel>`.

Two flags on `activate` / `archive` matter for promotion:

- **`--dry-run`** runs every gate the real transition would and reports the
  findings, writing nothing. It **exits 1 when the transition would be
  refused**, so it gates a script. It earns its keep most on channels, where
  activation requires an active workflow, a non-colliding route, and a stored
  config that still builds.
- **`--defer-reload`** commits the row but leaves the engine serving the
  previous set. Batch several, then `orion-cli engine reload` once.

`import --on-conflict` decides what an already-stored id means: `fail`
(default), `skip`, or `new_version` (upsert — the draft is replaced in place,
or a new draft version is cut over an active entity; identical content is a
no-op). `--dry-run` previews.

`workflows diff -f <file>` answers the question `import` would act on, matching
on `workflow_id` and comparing the server's `content_hash`. It exits `1` on
drift.

## Sending data and reading traces

```bash
orion-cli send orders -f order.json                       # sync
orion-cli send orders -d '{"total": 25000}' --profile     # + _orion.profile timings
orion-cli send orders -f order.json --async-mode --wait   # async, poll to completion
orion-cli send payload-channel --raw -f body.json         # body_mode = "payload"
```

`--raw` sends the payload verbatim with no `{"data": …}` envelope. `--metadata`
is **refused** alongside it, not silently dropped — a payload-mode channel
stamps metadata server-side and accepts none from the caller. `--profile` is
sync-only and needs the server's `tracing.debug_profile_enabled`.

```bash
orion-cli traces list --status failed --channel orders
orion-cli traces get <id> --token <t>
orion-cli traces wait <id>            # exit 0 completed, 1 failed, 2 timeout
```

Reading a trace needs an admin credential **or** the per-submission trace token
the async `202` returns beside the id — otherwise anyone who guessed an id
could read another caller's payload.

## Operations

| Command | Notes |
|---|---|
| `health` | Exits `1` when any component is degraded |
| `engine status` / `engine reload` | Reload also commits changes made with `--defer-reload` |
| `functions list` | Names, categories, and input schemas |
| `metrics [--raw]` | `--raw` is the Prometheus exposition text |
| `audit-logs list` | Exact-match filters: `--action`, `--resource-type`, `--resource-id`, `--principal`, `--start-time`, `--end-time` (RFC 3339). An unrecognised filter name is a `400`, never unfiltered rows |
| `backups create` / `backups list` | SQLite only |
| `packages list` / `packages get <name>` | The read side of promotion receipts |
| `dlq list` / `get` / `requeue <id>` / `purge --older-than-hours <n>` | `--exhausted true` narrows to entries nothing will retry — the only ones `purge` deletes |
| `benchmark -n <n> -c <n>` | Built-in scenarios, or `--workflow` + `--channel` |
| `completions <shell>` | bash, zsh, fish, powershell, elvish |

Pair `--change-context` on the writing side with `audit-logs list` on the
reading side to pull one promotion back out as a single operation.

## Promoting a whole service

`orion-server package` moves a package — selected channels, their workflows,
and every connector those workflows reference — between instances as one JSON
artifact. Every subcommand except `lint` calls an instance's admin API,
authenticating with `ORION_ADMIN_TOKEN`.

An artifact comes from one of two places: `export` reads a live instance, and
[`compile`](#shared-definitions-and-compiling-a-set) builds the same shape from
a directory of definitions with no instance to export from. `lint`, `plan`,
`apply` and `diff` cannot tell the two apart.

```bash
orion-server package export -s https://dev.orion.internal \
  --tag payments --name payments --version 1.4.0 -o payments-1.4.0.json
orion-server package lint -f payments-1.4.0.json          # offline
orion-server package plan -s https://prod... -f payments-1.4.0.json   # zero writes
orion-server package apply -s https://prod... -f payments-1.4.0.json
orion-server package diff  -s https://prod... -f payments-1.4.0.json  # exits 1 on drift
```

`apply` stages everything, activates in dependency order, reloads once, and
records a receipt. It is idempotent. Applied versions are **content-immutable**
— the same version with different content is a `409`, so any change needs a
version bump. Re-applying the previous artifact is the package-level rollback.

## Troubleshooting

**Conditions never match / `data.*` is empty.** The workflow is missing a
`parse_json` first task. `payload` is not in the JSONLogic context.

**A `map` produced an object where a scalar was expected.** A misspelled
operator was written through as a literal. Check the name against
`references/expressions.md`; conditions would have errored, mappings do not.

**A value stored in Mongo/SQL looks like `{"cat": [...]}`.** Connector payload
fields fold `{"var": …}` and nothing else. Compute it in a `map` task first and
reference `temp_data.*`. `orion-server lint` warns about this.

**A create or import is refused with `UNCOMPILED_SOURCE`.** The document still
carries a `$from` or a `use` — authoring syntax a definition *set* resolves and
a single POST cannot. Send what `orion-server compile <dir>` writes, rather than
hand-inlining the reference. The error names the reference and where it sits.

**Task ids changed after upgrading, and a package re-apply returns 409.** A
fragment containing a task group used to leak its nested ids unprefixed; they
are namespaced now, so recompiled workflows have different ids and a different
`content_hash`. Applied versions are content-immutable — bump the package
version. `expect_tasks` assertions naming a nested fragment id need the prefix.

**Activation refused.** Run it with `--dry-run` to see the findings. A channel
needs its workflow active first, a route that collides with nothing serving,
and a config that still parses.

**The channel returns 404 / requests are refused at every ingress.** A stored
config that no longer parses is *quarantined* rather than served with a guard
missing. `orion-server preflight` names affected channels.

**Changes are not live.** Activation hot-reloads on its own; `engine reload` is
only needed after `--defer-reload`. Connectors are unversioned — an `update`
needs a reload to be picked up.

**Everything returns an auth error.** `ORION_API_KEY` must match one of the
instance's `admin_auth.api_keys`. If admin auth is off, remove the key rather
than sending an unused one.

**A `--dry-run` looked like it passed but the real transition failed.** Check
the exit code, not the HTTP status: the pre-flight returns its findings inside
a `200`, and the command exits `1` when `valid` is false.
