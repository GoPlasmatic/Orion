<!-- description: Every orion-server and orion-cli command: fmt, lint, clippy, dry-run, offline tests, package promotion, plus workflow, channel, connector, trace and engine management. -->
# CLI Reference

Orion ships two binaries: `orion-server`, the runtime with diagnostic and promotion subcommands, and `orion-cli`, the admin client. Both accept `--version`, which prints the version, git hash, and build timestamp.

## orion-server

Running `orion-server` with no subcommand starts the server. The global flag `-c, --config <path>` names the TOML config file and applies to every subcommand. Each subcommand loads the same merged configuration: defaults, then the file, then `ORION_*` environment overrides. See [How Settings Are Resolved](./configuration.md#how-settings-are-resolved).

```bash
orion-server                          # Start with defaults
orion-server -c config.toml           # Start with a config file
```

### `validate-config`

Validates the configuration without starting the server, then prints the full effective config with secrets masked. Exits non-zero on an invalid value.

```bash
orion-server validate-config [--format <toml|json|summary>]
```

| Flag | Description |
|------|-------------|
| `--format` | Output format: `toml` (default), `json`, or `summary` (a short human summary). |

Example: `orion-server -c config.toml validate-config --format summary`

### `migrate`

Runs database migrations against the configured `storage.url` without starting the server.

```bash
orion-server migrate [--dry-run]
```

| Flag | Description |
|------|-------------|
| `--dry-run` | Preview pending migrations without applying them. |

Example: `orion-server -c config.toml migrate --dry-run`

### `lint`

Statically validates a workflow JSON file with the same checks the admin `POST /workflows` endpoint runs. Exits non-zero with field-pathed errors, so it can gate CI. Needs no config, database, or server.

```bash
orion-server lint <workflow.json | dir> [--deny-warnings]
                  [--requires-channel NAME]... [--requires-connector NAME]...
```

| Flag | Description |
|------|-------------|
| `--deny-warnings` | Exit non-zero on advisory findings too, not just errors. |
| `--requires-channel` | Channel name that may be referenced without being in the set. Repeatable, directory mode only. |
| `--requires-connector` | Connector name that may be referenced without being in the set. Repeatable, directory mode only. |
| `--definitions` | Directory holding the set's shared `constants`, `errors` and `fragments`. Implicit when linting a directory. |

**A directory is linted as a set.** Every channel, workflow and connector under it is validated, *and* the references between them are resolved — a `channel_call` target, a task's connector and its type, a channel's `workflow_id`, duplicate ids, names and routes. Those are the errors a per-file lint cannot see, because the file that would disprove them is one it never opens.

Entities are found by shape, recursively: an object with `tasks` is a workflow, `connector_type` a connector, `channel_type` or `protocol` a channel. Anything else is reported as skipped rather than silently ignored, and a directory yielding no definitions is an error.

By default every reference must resolve inside the set. Use `--requires-channel` / `--requires-connector` for a set that genuinely depends on something deployed elsewhere — the directory equivalent of a package artifact's `requires`.

```
$ orion-server lint ./definitions
note: definitions/request.json is not a channel, workflow or connector — skipped
error: [closure.connector] workflow 'auth-login': connector 'sias-mongo' is neither in the set nor declared on the boundary
warning: [closure.channel_call_dynamic] workflow 'route': resolves channel_call targets dynamically — closure checking cannot cover those calls
./definitions: 0 connector(s), 62 workflow(s), 62 channel(s) — 1 error(s), 1 warning(s)
```

Each finding carries a stable `[check]` id, so a pipeline can grandfather one rule without silencing the rest. `[env.unresolved]` is an error rather than an advisory: it fires when a workflow field that resolves no secret reference contains one, which the admin API refuses on the same terms. `note:` findings are exit-neutral inventory, not defects — `[env.reference]` lists each environment variable the set references via `env://`, `[secrets.reference]` each name it reads with `{"secret": …}` and so needs declared in the serving instance's `[secrets]` section, both with the files that reference them; neither the exit code nor `--deny-warnings` counts them.

Advisory findings print on stderr and do not fail the command unless `--deny-warnings` is set. Today there is one: JSONLogic in a connector field that folds `{"var": …}` and nothing else, so the expression is stored or sent verbatim.

Example: `orion-server lint examples/packages/high-value-order/workflow.json`

### `compile`

Compiles a definition set into files the admin API accepts, resolving the authoring conveniences a set may use — `$from` for a shared value, `use` for a task fragment. Needs no config, database, or server.

```bash
orion-server compile <dir> [-o <PATH>] [--format artifact|dir|bulk]
                           [--name NAME] [--version VERSION]
                           [--requires-channel NAME]... [--requires-connector NAME]...
                           [--deny-warnings] [--no-activate]
```

| Flag | Description |
|------|-------------|
| `-o, --output` | A file for `--format artifact` (default: stdout); a directory for `dir` and `bulk`, where it is required. |
| `--format` | `artifact` (default), `dir`, or `bulk` — see below. |
| `--name` | Package name. Required for `--format artifact`. |
| `--version` | Package version. Required for `--format artifact`. Applied versions are immutable — any content change needs a bump. |
| `--requires-channel` | Channel name that may be referenced without being in the set; recorded in the artifact's `requires`. Repeatable. |
| `--requires-connector` | Connector name that may be referenced without being in the set. Repeatable. |
| `--deny-warnings` | Exit non-zero on advisory findings too, not just errors. |
| `--no-activate` | Do not mark workflows and channels for activation, so the artifact applies as drafts. |

**Why it exists.** References resolve when a *set* is loaded, and the admin API loads no set: it takes one document, with nothing to resolve names against. Without this step the only path from `definitions/` to a running instance was a deploy tool that reimplemented the expander — and a partial reimplementation shows up as `UNCOMPILED_SOURCE` on the POST, 62 workflows deep.

**It runs `lint <dir>` first**, and emits nothing if that fails. A compile that wrote out a set its own linter rejects is how an artifact reaches `package apply` having passed CI.

| `--format` | Output | Consumed by |
|---|---|---|
| `artifact` | One promotion artifact, hashed exactly as `package export` hashes one | `orion-server package plan\|apply\|diff` |
| `dir` | The input tree mirrored, one file per entity, shared documents consumed | a POST per file — `orion-cli workflows import -f …` |
| `bulk` | `connectors.json`, `workflows.json`, `channels.json` | the bulk import endpoints, in that order |

`artifact` marks workflows and channels `activate: true`, because a directory carries no stored status and a package whose entities never activate applies cleanly and serves nothing. Set `"activate": false` on an entity, or pass `--no-activate`, to override. `dir` and `bulk` emit no activation intent — that is a package concept, and their files are request bodies.

Entities must carry explicit ids for `artifact` only: `apply` activates a channel by `channel_id` and reads activation intent off it, so an id-less entity in an artifact is one `apply` would stage and never activate. `dir` and `bulk` emit request bodies, where the server derives an id from the name exactly as it does for a hand-written POST — leaving `channel_id` out of a definition is an ordinary way to author a set, and committing a server-generated UUID would tie the set to one instance.

```
$ orion-server compile ./definitions --name payments --version 1.4.0 -o dist/package.json
compiled: shared.fragments rewrote 23 document(s)
compiled: shared.values rewrote 51 document(s)
./definitions: 4 connector(s), 62 workflow(s), 62 channel(s), 9 shared value(s), 3 fragment(s) — 0 error(s), 0 warning(s)
wrote payments@1.4.0 (4 connectors, 62 workflows, 62 channels) to dist/package.json
```

Example: `orion-server compile ./definitions --name payments --version 1.4.0 -o dist/package.json && orion-server package apply -s https://prod.orion.internal -f dist/package.json`

### `fmt`

Formats definition files to the house style, the way `cargo fmt` formats Rust. One style, nothing to configure; the style itself is documented on [Definition Style](./fmt.md). Needs no config, database, or server.

```bash
orion-server fmt [PATH]... [--check] [--stdin]
```

| Flag | Description |
|------|-------------|
| `PATH` | Files or directories (default: `.`). Every `.json` under a directory is formatted — entities, shared documents, `*.case.json` files and fixtures alike. Hidden entries, `target/` and `node_modules/` are skipped; symlinked directories are not followed. |
| `--check` | Write nothing. Print a unified diff for every file that is not in the house style and exit 1 if there is one — the CI form. |
| `--stdin` | Format one document from stdin to stdout, for editor integration. On a parse error nothing reaches stdout. |

| Exit code | Meaning |
|---|---|
| `0` | Every file is formatted (or was just written). |
| `1` | `--check` found at least one file it would rewrite. |
| `2` | A file could not be read, parsed or written. The other files are still processed. |

Files are rewritten atomically (a sibling temp file renamed over the original, permissions preserved), and only after the formatted output has been parsed again and compared with the input as the runtime sees it — a formatter that could change what a workflow means would not be safe to run from a pre-commit hook. A file that is not strict JSON, has a duplicate key, or nests deeper than the runtime's parser accepts is reported with its line and column and left untouched.

```
$ orion-server fmt --check ./definitions
--- a/definitions/orders/channel.json
+++ b/definitions/orders/channel.json
@@ -3,9 +3,7 @@
   "name": "orders",
   "channel_type": "sync",
   "protocol": "rest",
-  "methods": [
-    "POST"
-  ],
+  "methods": ["POST"],
   "route_pattern": "/orders",
1 file(s) would be reformatted, 61 unchanged
```

Example: `orion-server fmt ./definitions && orion-server lint ./definitions`

### `clippy`

Advisory checks beyond `lint`, said only when certain — the `cargo clippy` to `lint`'s `cargo check`. The rules, each with the proof it rests on and when it stays silent, are on [Advisory Checks](./clippy.md). No configuration, no suppression. Needs no database or server; takes the serving config with `-c` for the two rules that read `[vars]` and `[secrets]`.

```bash
orion-server clippy <dir | file> [--deny-warnings] [--format text|json]
                    [--definitions DIR] [--requires-channel NAME]... [--requires-connector NAME]...
orion-server clippy --list
orion-server clippy --explain <rule>
```

| Flag | Description |
|------|-------------|
| `<dir>` / `<file>` | A directory is checked as a set (every rule); a single file as a set of one — the set-scoped rules have nothing to compare it with. |
| `--deny-warnings` | Exit non-zero on warnings too. |
| `--format json` | One JSON object per diagnostic on stdout — `level`, `rule`, `entity`, `file`, `path`, `line`, `column`, `message`, `remedy` — and nothing else, for editors and pipelines. |
| `--list` | Every rule with its level, scope and summary. |
| `--explain RULE` | One rule's rationale, its proof and when it is silent. |
| `--definitions`, `--requires-*` | As `lint` takes them. |
| `-c FILE` (global) | The serving instance's config. Only a config you name counts: the defaults say nothing about `[vars]` or `[secrets]`. |

| Exit code | Meaning |
|---|---|
| `0` | No error. Warnings may have been printed. |
| `1` | A `lint` error, a `deny`-level rule, or a warning under `--deny-warnings`. |
| `2` | The path is not a set, or a usage error. |

`lint` runs first. Its findings are re-reported, and when it reports an *error* the rules do not run — the summary says `fix those first`. Diagnostics go to stderr in `lint`'s line format with a `file:line:col:` prefix wherever the source file has the same coordinates as the compiled form (no `use`, no `$from`); the one-line summary goes to stdout.

```
$ orion-server clippy ./definitions
definitions/workflows/auth-login.json: warning: [perf.redundant_step_condition] workflow 'Auth - login' at tasks[15].tasks[0].condition: 2 consecutive steps (`send_otp` and `when_unverified`) repeat this condition, and none of them writes what it reads; it is evaluated 2 times for one answer
        fix: wrap them in a task group carrying the condition once: { "id": …, "condition": …, "tasks": [ … ] }
note: [correctness.metadata_var_undeclared] skipped — needs the serving config (-c <config.toml>)
./definitions: 59 workflow(s), 62 channel(s), 9 connector(s) — 0 error(s), 1 warning(s) from 13 rule(s)
```

Example: `orion-server -c config.toml clippy ./definitions --deny-warnings`

### `dry-run`

Executes a workflow against a JSON input in an in-process engine, then prints the per-task execution trace. Connector-backed tasks are answered from `--stubs`; without a matching stub the task fails and names the stub it needs.

```bash
orion-server dry-run -w <workflow.json> -i <input.json> [--stubs <stubs.json>] [--metadata <metadata.json>] [--secrets <secrets.json>]
```

| Flag | Description |
|------|-------------|
| `-w, --workflow` | Path to a workflow JSON file. |
| `-i, --input` | Path to a JSON file used as the message payload. |
| `-s, --stubs` | Path to a JSON file of canned connector responses. The inner key is the task's `connector` (or `channel` for `channel_call`); `"*"` matches any. |
| `--definitions` | Directory holding the set's shared definitions, resolved before validation. |
| `-m, --metadata` | Path to a JSON file used as the message metadata — `headers`, `params`, `query`, `cookies`, `auth.claims`, `channel`, `vars`. Header keys are lowercased and credential headers masked, as at the HTTP ingress. |
| `--secrets` | Path to a JSON object of stand-in values for the `{"secret": "name"}` references the workflow reads: `{"partner_hmac": "test-key"}`. Offline there is no `[secrets]` config to resolve, and an engine with no store refuses a workflow that names one. Values are used verbatim — use throwaway ones. |

The printed document carries `data`, `metadata`, `temp_data`, `audit_trail` and `calls` — the same five documents, in the same shape, that a case's `expect` roots address — plus `output` (an alias of `data`, kept for existing `jq` filters), `trace`, `matched` and `errors`.

Example: `orion-server dry-run -w wf.json -i input.json --stubs stubs.json`

### `test`

Runs a directory of offline workflow test cases. Each `*.case.json` file names a workflow, an input, optional request metadata and connector stubs, and what it expects — output values (`expect`), task-error codes (`expect_errors`), connector calls (`expect_calls`) and executed task ids (`expect_tasks`). Prints a per-case diff and exits non-zero on any failure. See [Test Workflows Offline](../build/testing.md) for the case format.

```bash
orion-server test <path>
```

| Argument | Description |
|----------|-------------|
| `path` | A directory of `*.case.json` files, or a single case file. Paths inside a case resolve relative to the case file. |
| `--definitions` | Directory holding the set's shared definitions, resolved before each case's workflow is validated and run. |

Example: `orion-server test examples/workflow-tests`

### `test-connectivity`

Probes the configured database with a no-op query, and Kafka when `kafka.enabled = true`. Catches wrong credentials before the server tries to start.

Example: `orion-server -c config.toml test-connectivity`

### `preflight`

Scans stored channels and workflows for anything the 1.0 rules refuse: configs that no longer parse, tasks the validator rejects, and `data_query`/`data_write` tasks with no `schema`. Read-only; exits non-zero on findings. Config-file problems are `validate-config`'s job — this reads what only the database knows.

Example: `orion-server -c config.toml preflight`

### `dump-openapi`

Prints the public HTTP API's OpenAPI 3.1 spec as JSON to stdout. Needs no config, database, or running server. See [OpenAPI](./openapi.md).

Example: `orion-server dump-openapi > openapi.json`

### `package`

Exports a package — selected channels, their workflows, and every connector those workflows reference — and promotes it between instances. The artifact is one JSON document. Every subcommand except `lint` calls an instance's admin API, authenticating with the `ORION_ADMIN_TOKEN` environment variable. The model is described in [Promote Between Environments](../operate/promotion.md).

```bash
orion-server package <export|lint|plan|apply|diff> [flags]
```

| Subcommand | Description |
|------------|-------------|
| `export` | Compute the dependency closure from a running instance and write the artifact. |
| `lint` | Validate an artifact offline: entity shapes, closure completeness, content hash, and the cross-reference checks `lint <dir>` runs. Exits non-zero on **errors**; warnings and inventory notes print without failing. |
| `plan` | Pre-flight an artifact against a target with zero writes. |
| `apply` | Stage all entities, activate in dependency order, reload once, record the receipt. Idempotent. |
| `diff` | Report drift between an artifact and a running instance. Exits non-zero when anything differs. |

| Flag | Used by | Description |
|------|---------|-------------|
| `-s, --server <url>` | `export`, `plan`, `apply`, `diff` | Base URL of the source or target instance. |
| `-f, --file <path>` | `lint`, `plan`, `apply`, `diff` | Path to the artifact file. |
| `--tag <tag>` | `export` | Select every channel carrying this tag. |
| `--channels <ids>` | `export` | Select channels by id, comma-separated or repeated. |
| `--name <name>` | `export` | Package name. |
| `--version <ver>` | `export` | Package version. Applied versions are immutable; any content change needs a bump. |
| `-o, --output <path>` | `export` | Write the artifact here instead of stdout. |

Example: `orion-server package export -s https://dev.orion.internal --tag payments --name payments --version 1.4.0 -o pkg.json`

## orion-cli

`orion-cli` manages workflows, channels, connectors, data, traces, and engine operations on a running server over HTTP. Settings resolve in precedence order: CLI flags, then environment variables, then `~/.orion/config.toml`.

Global flags apply to every subcommand:

| Flag | Description |
|------|-------------|
| `--server <url>` | Orion server URL. Overrides the config file and `ORION_SERVER_URL`. |
| `--api-key <key>` | API key for admin authentication. Falls back to `ORION_API_KEY`, then the `api_key` in `~/.orion/config.toml`. When the key would travel over plain `http://` to any host but the local machine, a warning is printed to stderr — use `https://` for a remote server. |
| `--api-key-header <name>` | Header name carrying the key. Default: `Authorization` with a `Bearer` prefix. |
| `--change-context <ctx>` | Audit label for this change, e.g. `ticket=OPS-4412`. Sent as `X-Orion-Change-Context` and recorded under `details.change_context` on every audit row the command writes. Also read from `ORION_CHANGE_CONTEXT`. |
| `--output <format>` | Output format: `table` (default), `json`, or `yaml`. |
| `--quiet` | Print only IDs or minimal info. |
| `--verbose` | Show full response bodies and extra details. |
| `--no-color` | Disable colored output. |
| `--yes` | Skip confirmation prompts. |

### Paging and sorting

Every `list` pages: 50 rows by default, 1000 at most. The count under the table
says `Showing 50 of 3120 …` when the page is short of the total, so a truncated
listing never reads as a complete one.

| Flag | Applies to | Description |
|------|-----------|-------------|
| `--limit <n>` | every `list`, and `workflows`/`channels versions` | Page size, clamped to 1–1000. |
| `--offset <n>` | as above | Rows to skip. |
| `--sort-by <col>` | `workflows`, `channels`, `connectors`, `traces` | Column to order by. The accepted columns differ per resource; see each command below. |
| `--sort-order <dir>` | as above | `asc` or `desc`. Defaults to `desc` for the versioned lists (ordered by `priority`) and `asc` for connectors (ordered by `name`). |

### `config`

Manages the CLI's own settings in `~/.orion/config.toml`.

| Subcommand | Description |
|------------|-------------|
| `set-server <url>` | Set the Orion server URL. |
| `show` | Show the current CLI configuration. |
| `get <key>` | Print a single value, for scripting. |
| `set <key> <value>` | Set a value: `server_url`, `default_output`, `api_key`, or `api_key_header`. |

A stored `api_key` is used by every command, but the flag and
`ORION_API_KEY` both win over it. The file is plain TOML in your home
directory — on a shared machine, prefer the environment variable.

Example: `orion-cli config set-server http://localhost:8080`

### `health`

Checks server health, version, and component status. Exits `1` when any component is degraded.

Example: `orion-cli health`

### `workflows`

Manages workflows. Alias: `rules`.

| Subcommand | Description |
|------------|-------------|
| `list` | List workflows; filter with `--status` and `--tag`. Sorts by `priority`, `name`, `status`, `created_at`, `updated_at`. |
| `get <id>` | Show a workflow; `--verbose` includes condition and tasks. |
| `create` | Create a workflow from JSON: `-f <file>`, `-d <json>`, or `--stdin`; `--id` sets the workflow id instead of generating one. |
| `update <id>` | Replace a workflow definition. Only drafts accept updates. |
| `delete <id>` | Delete a workflow; prompts unless `--yes`. |
| `activate <id>` | Activate a draft workflow. `--dry-run` pre-flights; `--defer-reload` batches. |
| `archive <id>` | Archive an active workflow. Same two flags. |
| `dependencies <id>` | Show the workflow's connectors and `channel_call` targets. Alias: `deps`. |
| `validate` | Validate a definition without creating it. Exits `1` when invalid. |
| `rollout <id> -p <n>` | Update the rollout percentage. `--defer-reload` batches. |
| `versions <id>` | List version history; pages with `--limit` / `--offset`. |
| `new-version <id>` | Create a new draft version from the active one. |
| `test <id>` | Dry-run the workflow with sample data; `--metadata <json>`, `--trace`. |
| `export` | Export workflows as JSON; filter with `--status`, `--tag`. |
| `import -f <file>` | Bulk-import from a JSON array file; `--dry-run` previews, `--on-conflict` sets the collision rule. |
| `diff -f <file>` | Compare a local file against server state. Exits `1` when anything differs. |

`activate` and `archive` take two flags that matter for promotion:

| Flag | Description |
|------|-------------|
| `--dry-run` | Run every gate the real transition would run and report the findings, writing nothing. Exits `1` when the transition would be refused, so it gates a script. |
| `--defer-reload` | Commit the row but leave the running engine serving the previous active set. Batch several changes, then `orion-cli engine reload` once. |

`import` takes `--on-conflict` — what an already-stored id means:

| Value | Behaviour |
|-------|-----------|
| `fail` | Default. The conflicting item is refused and reported. |
| `skip` | The conflicting item is left as it is and counted as skipped. |
| `new_version` | Upsert: the draft is replaced in place, or a new draft version is cut over an active entity. Identical content is a no-op. |

`diff` answers the question `import` would act on: it matches local items to
stored ones by `workflow_id` — the key an import collides on — and compares the
server's `content_hash` when the file carries one (an exported artifact does),
falling back to the importable fields for a hand-authored file. Fields that a
re-import never writes — `version`, `status`, `created_at` — are ignored, so a
file exported and diffed straight back reports every workflow unchanged.

For promoting a whole service rather than one resource kind, use
`orion-server package diff`, which covers channels and connectors too and
compares the closure as a unit.

Example: `orion-cli workflows test order-enrichment -f payload.json --trace`

### `channels`

Manages channels. Alias: `ch`.

| Subcommand | Description |
|------------|-------------|
| `list` | List channels; filter with `--status`, `--channel-type`, `--protocol`, `--tag`. Sorts by `priority`, `name`, `status`, `channel_type`, `protocol`, `created_at`, `updated_at`. |
| `get <id>` | Show a channel. |
| `create` | Create a channel from JSON: `-f <file>`, `-d <json>`, or `--stdin`. |
| `update <id>` | Replace a channel definition. Only drafts accept updates. |
| `delete <id>` | Delete a channel; prompts unless `--yes`. |
| `activate <id>` | Activate a draft channel. `--dry-run` pre-flights; `--defer-reload` batches. |
| `archive <id>` | Archive an active channel. Same two flags. |
| `versions <id>` | List version history; pages with `--limit` / `--offset`. |
| `new-version <id>` | Create a new draft version from the active one. |
| `validate` | Validate a definition without creating it. Exits `1` when invalid. |
| `export` | Export channels as JSON; filter with `--status`, `--tag`, `--channel-type`, `--protocol`. |
| `import -f <file>` | Bulk-import from a JSON array file; `--dry-run` previews, `--on-conflict` sets the collision rule. |

`--dry-run` earns its keep most on channels: activation requires an active
workflow, a route pattern that collides with nothing already serving, and a
stored config that still builds — none of which a client can check for itself.

Example: `orion-cli channels activate orders --dry-run`

### `connectors`

Manages connectors and their circuit breakers. Alias: `conn`.

| Subcommand | Description |
|------------|-------------|
| `list` | List connectors; filter with `--tag`. Sorts by `name`, `connector_type`, `created_at`, `updated_at`. |
| `get <id>` | Show a connector; secrets stay masked. |
| `create` | Create a connector from JSON: `-f <file>`, `-d <json>`, or `--stdin`. |
| `update <id>` | Replace a connector definition. |
| `delete <id>` | Delete a connector; prompts unless `--yes`. |
| `enable <id>` | Enable a disabled connector. |
| `disable <id>` | Disable a connector without deleting it. |
| `test <id>` | Probe the connector's target with the stored config. An `http` connector's probe is one real request. |
| `validate` | Validate a definition without creating it. Checks the shape only — `test` is what reaches the target. Exits `1` when invalid. |
| `export` | Export connectors as JSON; filter with `--tag`. Secrets stay masked, so a re-import needs them supplied again. |
| `import -f <file>` | Bulk-import from a JSON array file; `--dry-run` previews, `--on-conflict` sets the collision rule. |
| `circuit-breakers` | List circuit breaker states: `closed`, `open`, or `half_open`. |
| `reset-breaker <key>` | Reset a tripped circuit breaker to closed. The key is `connector:channel`. |

Connectors are not versioned — there is no draft, no `activate`, and no
`versions`. `update` writes in place and the engine picks it up on reload.

Example: `orion-cli connectors test payment-api`

### `send`

Sends data to a channel. Synchronous by default; `--async-mode` submits for background processing and returns a trace ID.

| Flag | Description |
|------|-------------|
| `<channel>` | Channel name to send data to. |
| `-f, --file <path>` | JSON payload from a file. |
| `-d, --data <json>` | Inline JSON payload. |
| `--stdin` | Read the payload from stdin. |
| `--async-mode` | Submit for async processing; returns a trace ID and trace token. Alias: `--async`. |
| `--wait` | With `--async-mode`, poll until the trace completes. |
| `--timeout <secs>` | Timeout for `--wait`. Default: `60`. |
| `--metadata <json>` | Metadata object attached to the request. Refused with `--raw`. |
| `--raw` | Send the payload as the request body verbatim, with no `{"data": …}` envelope. |
| `--profile` | Request server-side execution profiling; adds an `_orion.profile` breakdown. Sync only, and needs the server's `tracing.debug_profile_enabled`. |

`--raw` is what reaches a channel configured with
`request.body_mode = "payload"`. Such a channel takes the whole body as `data`,
so the default envelope would arrive as a single key literally named `data`.
`--metadata` is refused alongside it rather than silently dropped: a
payload-mode channel stamps metadata server-side and accepts none from the
caller.

Example: `orion-cli send orders -f order.json --async-mode --wait`

### `traces`

Views execution traces.

| Subcommand | Description |
|------------|-------------|
| `list` | List traces; filter with `--status`, `--channel`, and `--mode`. Sorts by `created_at`, `updated_at`, `status`, `channel`, `mode`. |
| `get <id>` | Show trace details, including the result or error. |
| `wait <id>` | Poll until the trace completes. `--interval <secs>` (default `1`), `--timeout <secs>` (default `60`). Exit codes: `0` completed, `1` failed, `2` timeout. |

Reading a trace needs either an admin credential or the per-submission
**trace token** that the async `202` returns alongside the id. Pass it with
`--token <token>` on `get` and `wait`. Without one, any caller who guessed an
id could read another caller's payload.

`list` has two paging controls beyond `--limit` / `--offset`:

| Flag | Description |
|------|-------------|
| `--cursor <c>` | Keyset cursor from a previous page's `next_cursor`; pass it back unmodified. Valid only with the default `created_at` ordering, mutually exclusive with `--offset`, and cheaper on a large table because it never skips rows. |
| `--include-total` | Ask the server to compute `total`. Off by default: the count is a full scan of the filtered set. |

Example: `orion-cli traces list --status failed --channel orders`


### `engine`

Controls the engine. Alias: `eng`.

| Subcommand | Description |
|------------|-------------|
| `status` | Show engine status: version, uptime, workflow and channel counts. |
| `reload` | Hot-reload the engine from the database. |

Example: `orion-cli engine reload`

### `functions`

`list` shows the workflow task functions registered in the engine and their input schemas. Alias: `fn`. The catalog is documented in [Task Functions](./functions.md).

Example: `orion-cli functions list`

### `metrics`

Fetches `GET /metrics` from the server. Default output is a reformatted list; `--raw` prints the Prometheus exposition text. Series are documented in the [Metrics Reference](./metrics.md).

Example: `orion-cli metrics --raw`

### `audit-logs`

`list` shows audit log entries of admin actions. Alias: `audit`.

Filters combine with AND and are applied in the database:

| Flag | Description |
|------|-------------|
| `--action <a>` | Exact match on the action, e.g. `create`, `status_active`, `update_rollout`. |
| `--resource-type <t>` | Exact match on the resource type: `workflow`, `channel`, `connector`, `engine`, `backup`, `circuit_breaker`, `trace_dlq`, `package`. |
| `--resource-id <id>` | Exact match on the resource id. |
| `--principal <p>` | Exact match on the acting principal — the admin key id, or `anonymous` when admin auth is off. |
| `--start-time <ts>` | Inclusive lower bound on `created_at`, RFC 3339. |
| `--end-time <ts>` | Exclusive upper bound on `created_at`, RFC 3339. |

Because the matches are exact, a filter is only as good as the vocabulary
behind it — the full `action` × `resource_type` table is in
[Audit Logs](../operate/audit-logs.md). An unrecognised filter name is rejected
with a `400` rather than answered with unfiltered rows, so a mistyped
compliance query cannot silently widen.

Pair it with `--change-context` on the writing side: label a promotion's
commands with `--change-context ticket=OPS-4412`, then read them back as one
operation.

Example: `orion-cli audit-logs list --action status_active --resource-type workflow --start-time 2026-07-01T00:00:00Z`

### `backups`

Creates and lists database backups. SQLite only.

| Subcommand | Description |
|------------|-------------|
| `create` | Create a backup. |
| `list` | List existing backups. |

Example: `orion-cli backups create`

### `packages`

Inspects package promotion receipts. Alias: `pkg`.

| Subcommand | Description |
|------------|-------------|
| `list` | List package receipts, ordered by name and newest first within a package. |
| `get <name>` | Show a package's current receipt and version history. |

Receipts are written by `orion-server package plan/apply`, not by this command:
`orion-cli packages` is the read side, answering which package versions this
instance has staged or applied.

Example: `orion-cli packages get payments`

### `dlq`

Inspects and drains the trace dead-letter queue.

| Subcommand | Description |
|------------|-------------|
| `list` | List dead-letter entries; filter with `--channel` and `--exhausted <true\|false>`. |
| `get <id>` | Show one entry, including its payload. |
| `requeue <id>` | Reset an entry's retry counter so the next retry pass picks it up. |
| `purge --older-than-hours <n>` | Permanently delete exhausted entries older than the cut-off. The flag is required, and the command prompts unless `--yes`. |

`--exhausted true` narrows to entries whose retries are used up — the ones
nothing will pick up again, and the only ones `purge` deletes.

Example: `orion-cli dlq purge --older-than-hours 168`

### `benchmark`

Runs a performance benchmark against the server. Alias: `bench`.

| Flag | Description |
|------|-------------|
| `-n, --requests <n>` | Requests per scenario. Default: `100`. |
| `-c, --concurrency <n>` | Concurrent requests. Default: `10`. |
| `--timeout <secs>` | Per-request timeout. Default: `30`. |
| `--scenario <name>` | Built-in scenario to run. Default: `all`. |
| `--workflow <id>` | Benchmark an existing workflow instead of the built-in scenarios. |
| `--channel <name>` | Channel to send to; required with `--workflow`. |
| `-f, --file` / `-d, --data` | Payload for `--workflow`. |
| `--cleanup-only` | Only clean up leftover benchmark resources. |

Example: `orion-cli benchmark -n 500 -c 25`

### `completions`

Generates shell completions for `bash`, `zsh`, `fish`, `powershell`, or `elvish`. Alias: `comp`.

Example: `orion-cli completions zsh > ~/.zfunc/_orion-cli`

## Environment variables

| Variable | Read by | Purpose |
|----------|---------|---------|
| `ORION_SERVER_URL` | `orion-cli` | Server URL when `--server` is not given. |
| `ORION_API_KEY` | `orion-cli` | Admin API key when `--api-key` is not given. |
| `ORION_API_KEY_HEADER` | `orion-cli` | Header name carrying the key. |
| `ORION_CHANGE_CONTEXT` | `orion-cli` | Audit change context when `--change-context` is not given. |
| `NO_COLOR` | `orion-cli` | Disables colored output, like `--no-color`. |
| `ORION_ADMIN_TOKEN` | `orion-server package` | Bearer token sent to the target instance's admin API. A warning is printed when it would travel over plain `http://` to any host but the local machine. |
| `ORION_SECTION__KEY` | `orion-server` | Overrides any config setting, e.g. `ORION_SERVER__PORT`. See the [Configuration Reference](./configuration.md#how-settings-are-resolved). |

## Related

- [Configuration Reference](./configuration.md) — every server setting and its `ORION_*` override.
- [Admin API](./admin-api.md) — the HTTP endpoints `orion-cli` drives.
- [Promote Between Environments](../operate/promotion.md) — the promotion model behind `orion-server package`.
- [OpenAPI](./openapi.md) — the spec `dump-openapi` prints.

## Shared definitions

A definition set can say a thing once. Two mechanisms, one resolution pass, both expanded **before** validation — so `lint`, `dry-run` and `test` all check and run the expanded form, and the server, the admin API, traces and the UI never see a reference.

```json
{ "constants": { "db": { "connector": "sias-mongo", "database": "app" } },
  "errors":    { "USER_NOT_FOUND": { "status": 400, "body": "User Not Found !" } } }
```

**`$from` splices a named value** into the object it sits in:

```json
{ "input": { "$from": "constants.db", "collection": "users" } }
```

resolves to `{"connector": "sias-mongo", "database": "app", "collection": "users"}`. It is a **merge, not a substitution**, and **siblings win** — so a call site overrides one field without copying the rest. A `$from` alone in its object, naming a scalar or array, replaces the whole node.

**Fragments are named task sequences**, parameterised:

```json
{ "fragments": { "require-session": {
    "params": { "deny_message": { "default": "Session expired." } },
    "tasks": [ { "id": "check", "name": "Check",
      "function": { "name": "map", "input": { "mappings": [
        { "path": "data.msg", "logic": { "$param": "deny_message" } } ] } } } ] } } }
```

```json
{ "id": "_session", "use": "require-session", "with": { "deny_message": "Please sign in." } }
```

Expanded task ids are namespaced by the call-site id (`_session.check`), so a fragment cannot collide with the including workflow or with a second instance of itself. **Every** id the fragment contributes is prefixed, including those inside a task group — a group's own id and its members' alike, flat rather than one segment per enclosing group, so `refused`/`deny` become `_session.refused` and `_session.deny`. A parameter with no `default` is required at every call site. A fragment cannot include another fragment, at any depth.

A shared document is one carrying `constants`, `errors` or `fragments` and no entity field — found by shape, like entities, and split across as many files as you like. A name defined twice is an error rather than a silent last-write-wins.

Every unresolved reference is a lint error, which is why set mode resolves the catalog with no flag; the single-file commands take `--definitions <dir>`.

> [!NOTE]
> Expansion is an **authoring and deploy** mechanism. The admin API takes one JSON body with no set to resolve against, so `POST /api/v1/admin/workflows` does not accept `$from` or `use`; it refuses them with [`UNCOMPILED_SOURCE`](./errors.md#field-error-codes), naming the reference and its coordinate. [`orion-server compile`](#compile) is the step that produces what it does accept. `package export` needs no inlining step for the same reason: it exports what a server stored, which was already compiled.

Both mechanisms are passes in one pipeline, and the pipeline is the place a future authoring convenience is added — a new pass is compiled by `compile`, reported in its per-pass summary, and named by the admin API's refusal without any of those three learning about it.
