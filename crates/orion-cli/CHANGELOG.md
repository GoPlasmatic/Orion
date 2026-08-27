# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.3.1] - 2026-08-27

`orion-cli` and `orion-server` release in lockstep; 1.3.0 was server-side only
(`[vars]` and `[secrets]`, described in the server changelog).

### Security

- A warning on stderr when the API key would be sent over plain `http://` to
  any host but the local machine (`localhost`, `*.localhost`, `127.0.0.0/8`,
  `::1`) — use `https://` for a remote server. The check lives in
  `orion-client` 1.0.5 (`OrionClient::sends_credential_in_clear()`), which
  this release pins.

### Changed

- `--verbose` prints its `Server:` line from the client builder, once, instead
  of from each of fourteen subcommands. No visible change.

## [1.2.1] - 2026-08-26

`orion-cli` and `orion-server` release in lockstep, so this version moves with
the server's. The cycle's change is server-side — `orion-server compile <dir>`,
which turns a definition set into files the admin API accepts, and the
fragment-namespacing fix behind it — and is described in the server changelog.
The CLI itself is unchanged; deploying a compiled set still goes through
`orion-cli workflows import` (`--format dir` / `--format bulk`) or
`orion-server package apply` (`--format artifact`).

Its manifest moves: the rider crates `orion-api` and `orion-client` go to
1.0.3, so the requirements here follow them.

## [1.2.0] - 2026-08-26

`orion-cli` and `orion-server` release in lockstep. This cycle's server-side
work — the offline test runner, definition-set lint, shared definitions, and
dataflow-rs 3.6/3.7 — is in the server changelog; the CLI's own change is the
removal below.

Its manifest also moves: the rider crates `orion-api` and `orion-client` go to
1.0.2, so the requirements here follow them.

### Removed

- **The built-in MCP server is gone** — `orion-cli mcp serve`, the whole
  `src/mcp/` tree (58 tools, ~2,000 LOC), `server.json`, the MCP-registry
  publish job, and the `rmcp`/`schemars`/`axum`/`tracing`/`tracing-subscriber`
  dependencies it alone pulled in.

  Two reasons, and the first is the one that forced it. **The HTTP transport
  had no authentication of its own.** `mcp serve --http` bound `0.0.0.0:8081`
  and served the full admin API — create, activate, delete, read every trace —
  to anything that could reach the port, carrying the operator's own
  `ORION_API_KEY` upstream. The documentation warned about it; a warning is not
  a control, and the shipped `docker-compose.yml` published the port.

  Second, it earned nothing it cost. Every one of the 58 tools was a
  hand-written mirror of a command `commands::` already had, over the same
  `orion-client` transport, with no test at any layer — so each new flag had to
  be added twice, and the drift was caught by discipline rather than CI.

  **What replaces it:** the agent skill at `skills/orion/`, plus this CLI. An
  assistant reads the skill and runs `orion-cli`, so it inherits the operator's
  access instead of holding its own, every admin write lands in the audit log
  under the operator's principal, and nothing listens on a port. See
  [Agent Skill Setup](https://docs.goplasmatic.io/ai/skills.html).

  If you drive Orion from Claude Desktop, Cursor's chat, or another client that
  cannot run a shell, this is a breaking change with no in-product replacement:
  use the [prompt pack](https://docs.goplasmatic.io/ai/prompt-pack.html) against
  the REST API, or pin `orion-cli` 1.2.0.

### Fixed

- **`config set api_key` documented a resolver that no longer existed.**
  `OrionConfig::resolve_server_url()` was a second server-URL resolver used only
  by `mcp serve`; `--server` already carries `env = "ORION_SERVER_URL"`, so clap
  had applied the variable before `build_client` ever read the flag. Removed
  with the module that called it.

## [1.1.0] - 2026-08-21

### Fixed

- **`activate|archive --dry-run` reported a refused transition as a success.**
  The pre-flight answers with the `/validate` envelope inside a **200** —
  `valid: false` with findings, so a promotion can pre-flight a whole package
  without stopping at the first missing entity — and the command printed
  "can change to active" and exited `0` regardless. It now renders the errors
  and warnings and exits `1`, which is what makes the flag usable as a gate.
- **`workflows diff` could never report a workflow unchanged.** It compared the
  export's `condition`/`tasks` against fields named `condition_json`/`tasks_json`
  that no response carries, so every item came back modified — including a file
  diffed straight back against the export it came from. It now matches on
  `workflow_id` (the key an import collides on) and compares the server's
  `content_hash`, falling back to the importable fields for a hand-authored
  file. It also exits `1` on drift, like `orion-server package diff`.
- **A stored `api_key` was only ever sent by `mcp serve`.** `orion-cli config
  set api_key <key>` wrote a key that every other command ignored, so they went
  out unauthenticated. All commands now resolve the flag, then `ORION_API_KEY`,
  then `~/.orion/config.toml` — the order `mcp serve` already used.
- **`audit-logs list` reported the page size as the total**, reading a
  `pagination.total` field the admin envelope has never had.
- **The CLI and MCP channel-config help listed keys the server rejects**
  ([#271]). Four of the five names in `channels create` guidance were pre-1.0
  spellings — `cors` (retired in favour of `origin_allow_list`),
  `input_validation` (`validation_logic`), `rate_limit.rps`
  (`requests_per_second`) and `backpressure.max_concurrent`
  (`max_concurrent_per_node`). Since `ChannelConfig` denies unknown fields, an
  LLM following that description produced a config the server refused.
  Corrected, and the remaining valid keys listed.

### Added

- **`send --raw`** ([#282]) — send the payload as the request body verbatim,
  with no `{"data": …}` envelope.

  The server's `request.body_mode = "payload"` shipped without a client that
  could address it: every CLI and MCP data path wrapped unconditionally, so a
  payload-mode channel received `data = {"data": …}` and the documentation told
  you to reach for `curl`. `--raw` closes that, and the MCP `send` tools take an
  equivalent `raw` parameter.

  `--raw` and `--metadata` are refused together rather than one being silently
  dropped: a payload-mode channel stamps its own metadata and accepts none from
  the caller, so there is nowhere for it to go.

  This also unblocks e2e coverage of `body_mode`, which had integration tests
  and reference docs but no end-to-end case, because the suite drives every send
  through the CLI.

- **Every audit-log filter the endpoint accepts** — `--action`,
  `--resource-type`, `--resource-id`, `--principal`, `--start-time`,
  `--end-time` — on `audit-logs list` and the `audit_logs_list` MCP tool.
  Previously only `--limit`/`--offset` were reachable.
- **`--change-context <ctx>`** (or `ORION_CHANGE_CONTEXT`), a global flag that
  stamps `X-Orion-Change-Context` on every request, so the audit rows of one
  multi-command operation group under `details.change_context`.
- **`--limit`/`--offset`/`--sort-by`/`--sort-order` on `workflows list`**, which
  had none of them and so silently showed only the first 50; `--sort-by` and
  `--sort-order` on `channels list` and `connectors list`; `--limit`/`--offset`
  on `workflows|channels versions`; `--channel-type`/`--protocol` on
  `channels export`; `--defer-reload` on `workflows rollout`.
- **MCP parity with the CLI:** `channels_validate`, `channels_export`,
  `connectors_validate`, `connectors_export` and `trace_dlq_purge` tools;
  `on_conflict` on all three import tools; `tag` and sorting on the list tools;
  `cursor`/`include_total` on `traces_list`.

### Changed

- **A truncated listing says so.** The count under a table now reads
  `Showing 50 of 3120 workflow(s) -- page with --limit / --offset` when the page
  is short of the server's total, instead of printing the total alone under 50
  rows.
- **Channel and connector `validate` render field-pathed issues** the way
  `workflows validate` always has, rather than dumping raw JSON objects, and
  report warnings as well as errors.

## [1.0.0] - 2026-08-14

Orion server v1.0 compatibility release, developed in the Orion monorepo
(the CLI moved from GoPlasmatic/Orion-cli to GoPlasmatic/Orion as
`crates/orion-cli`; the end-to-end suite that drives both binaries now
lives at the repo root — `tests/e2e/`, with its data-driven use cases in
`examples/use-cases/` — and runs against the in-tree server on every PR).

### Added

- **`packages list|get`** — package promotion receipts (staged/applied
  versions with content hashes), plus `packages_list`/`packages_get` MCP tools.
- **`dlq list|get|requeue|purge`** — the trace dead-letter queue, plus
  `trace_dlq_list`/`trace_dlq_get`/`trace_dlq_requeue` MCP tools.
- **`workflows dependencies <id>`** (alias `deps`) — connector references and
  `channel_call` targets, plus a `workflows_dependencies` MCP tool.
- **`connectors test <id>`** — reachability probe with the stored config,
  plus a `connectors_test` MCP tool; `channels|connectors validate` and
  `channels|connectors export`, completing parity with workflows.
- **`--on-conflict fail|skip|new_version`** on all three bulk imports;
  **`--dry-run`** and **`--defer-reload`** on activate/archive;
  **`--tag`** filters on list commands; **`--cursor`/`--include-total`**
  keyset pagination on `traces list`; **`--token`** on `traces get|wait`.

### Changed

- **Shared workspace crates under the hood** — the wire contract
  (`orion-api`: error envelope, status vocabulary, typed import report) and
  the HTTP transport (`orion-client`: auth, envelope parsing, every endpoint
  path) are now the same code the server serializes with and drives its own
  `package` promotion CLI through. Error hints key on the server's real
  error-code registry — the `AUTH_FAILED`/`INVALID_INPUT`/`ALREADY_EXISTS`
  hint branches matched codes the server never emits and are gone. Rendered
  output is unchanged.
- **v1.0 response envelope** — every admin 2xx wraps its payload in
  `{"data": …}`; engine status/reload, workflow test/validate, import
  results, and trace reads unwrap it (older bare responses still parse).
- **Import summaries** read the v1.0 `imported`/`unchanged`/`skipped`/
  `failed` counts (dry-run included) instead of `would_create`/`would_fail`.
- **Traces moved to `/api/v1/admin/traces`** — `/api/v1/data/traces` is a
  channel route in v1.0. Reading a trace requires its `trace_token` or an
  admin credential; `send --async --wait` threads the token automatically,
  and `send --async-mode --output json` now prints the full submit response
  (`trace_id` + `trace_token`) instead of a human-format line.
- **E2E suites speak v1.0**: sends go through channels bound to exactly one
  workflow, unknown channels are refused, archiving a channel's workflow
  quarantines it, and the server config template uses `storage.url` /
  `[trace_queue]`.
- **Dependency major upgrades**: `rmcp` 1.4 → 3.1 (keeping the MCP server
  reachable) and `tabled` 0.20 → 0.21, plus the workspace-wide sweep
  recorded in the server's changelog. No CLI behaviour change is intended.

### Distribution

- **`orion-cli` is not published to crates.io.** The name there belongs to an
  unrelated crate registered in 2021, so `cargo install orion-cli` does not
  and will not reach this tool. Install it from the Homebrew tap
  (`brew install GoPlasmatic/tap/orion-cli`), the shell/PowerShell installers
  attached to each GitHub release, `ghcr.io/goplasmatic/orion-cli`, or
  `cargo install --git https://github.com/GoPlasmatic/Orion --locked orion-cli`.
  This matches how every previous CLI release shipped; only the server crate
  and the two shared libraries go to crates.io.

## [0.2.1]

MCP registry and directory readiness release. No CLI behaviour changes.

### Added

- **Official MCP registry publishing** — `server.json` manifest
  (`io.github.goplasmatic/orion`, OCI package on GHCR), the
  `io.modelcontextprotocol.server.name` image label, and a `publish-mcp`
  job in the Docker release workflow that publishes each tagged release
  to the registry via GitHub OIDC.
- **`.mcp.json`** at the repo root (Open Plugins standard) so directories
  like cursor.directory can auto-detect the MCP server.
- **`glama.json`** for Glama directory ownership.
- **Cursor one-click install** badge in the README.

### Changed

- crates.io metadata: fuller description, keywords, and categories.
- `mcp serve` help now states the correct tool count (46) and the correct
  Claude Desktop config path on macOS.

## [0.2.0]

Adds support for the Orion v0.2.0 server runtime.

### Added

- **`functions` command** and `functions_list` MCP tool — list the workflow task
  functions registered in the engine, with their input JSON Schemas
  (`GET /api/v1/admin/functions`).
- **`send --profile`** — request server-side execution profiling and render the
  `_orion.profile` breakdown (total time, per-phase split, slowest handlers).
  Requires `tracing.debug_profile_enabled` on the server.
- **Bulk import for channels and connectors** (`channels import`, `connectors import`)
  plus matching `channels_import` / `connectors_import` MCP tools.
- **`traces get`** now displays the per-task execution trace (`task_trace_json`)
  when a channel opts in via `config.tracing.task_details`.

### Changed

- **Structured error output** — `OrionClient` now surfaces the v0.2 `error.details[]`
  field-pathed validation errors (with `expected`/`got`) and `request_id`, in
  addition to the existing `[CODE] message`. v0.1 servers are unaffected.
- **Workflow import `--dry-run`** is now validated server-side via `?dry_run=true`
  (reports `would_create`/`would_fail`) instead of a local count.
- **Async send** handles a null `trace_id` (returned when a channel's trace storage
  mode is `off`): it reports submission and skips polling instead of failing.

## [0.1.1]

Earlier release. See the Git history for details.

## [0.1.0]

Initial release.

<!-- Releases through 0.2.1 were tagged in the former GoPlasmatic/Orion-cli
     repository; 1.0.0 onward are tagged in the GoPlasmatic/Orion monorepo.
     The history spans two repositories, so pre-1.0 entries link to their
     release pages rather than to a compare range. -->

[#271]: https://github.com/GoPlasmatic/Orion/issues/271
[#282]: https://github.com/GoPlasmatic/Orion/issues/282

[Unreleased]: https://github.com/GoPlasmatic/Orion/compare/v1.3.1...HEAD
[1.3.1]: https://github.com/GoPlasmatic/Orion/compare/v1.2.1...v1.3.1
[1.2.1]: https://github.com/GoPlasmatic/Orion/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/GoPlasmatic/Orion/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/GoPlasmatic/Orion/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/GoPlasmatic/Orion/releases/tag/v1.0.0
[0.2.1]: https://github.com/GoPlasmatic/Orion-cli/releases/tag/v0.2.1
[0.2.0]: https://github.com/GoPlasmatic/Orion-cli/releases/tag/v0.2.0
[0.1.1]: https://github.com/GoPlasmatic/Orion-cli/releases/tag/v0.1.1
[0.1.0]: https://github.com/GoPlasmatic/Orion-cli/releases/tag/v0.1.0
