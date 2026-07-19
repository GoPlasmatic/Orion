# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.1]

MCP registry and directory readiness release. No CLI behavior changes.

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
