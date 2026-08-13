# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0] - 2026-08-12

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
