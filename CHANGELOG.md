# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-05-27

This release upgrades the workflow engine to dataflow-rs 3.0 / datalogic-rs 5 and
adds a large set of governance, validation, and operability features. JSONLogic
compilation now happens at engine-construction time, yielding sizeable throughput
gains (+48% on complex workflows, +120% on multi-workflow scenarios) and lower P99
latency across every benchmark scenario versus the v0.1.x baseline.

### Breaking Changes

- **Engine upgrade to dataflow-rs 3.0 + datalogic-rs 5.** JSONLogic is compiled once
  at engine build time rather than per request.
- **Connector `api_key` field removed** in favour of `api_keys`. Update any connector
  configs still using the singular field.
- **Channel/connector create & update DTOs are now strongly typed enums.** Invalid
  `channel_type`, `protocol`, or connector `type` values are rejected at
  deserialization with `400` (values remain case-insensitive; v0.1 lowercase wire
  values are still accepted).
- **Profile output is namespaced** under `_orion.profile` with `version: 1`.

### Added

- **Configurable trace storage modes** — `sync`, `async`, `batch`, or `off` — as a
  global default with per-channel override via `config.tracing`.
- **Per-request workflow profile mode** for timing/inspecting task execution.
- **Per-task execution traces** captured when a channel opts in.
- **Structured error envelope** with field-pathed `FieldError` details, plus
  collection of all protocol-required-field errors in a single response.
- **Per-function input schema validation** for workflow task functions.
- **Bulk import** for channels and connectors, with `?dry_run=true` preview.
- **Strict validation of channel `config_json` at create time.**
- **Config & connector variable substitution** — `${VAR}` / `${VAR:-default}` in
  config TOML and connector configs.
- **`env://` secret references** resolved in connector configs.
- **New CLI subcommands:** `lint`, `dry-run`, and `test-connectivity`.
- **OpenAPI coverage** for the audit, backup/restore, and functions endpoints.

### Changed

- **Performance:** roughly halved per-request CPU by sharing `AppState` via `Arc` and
  gating compression/metrics work.
- **OpenTelemetry** bumped to 0.32 / 0.33; refreshed transitive dependencies
  (`rand` 0.10.1, `tokio` 1.52, and others).
- Distributed config validation into per-struct implementations; decomposed the
  `main.rs` startup sequence; split oversized handlers and centralised admin reload,
  trace-filter, and error-mapping logic.
- Renamed `connector::types` module to `connector::config`.
- Refreshed README, `docs/`, and `tests/README`, and added v0.2.0 / v3.0.0 benchmark
  result sets alongside the v2.1.5 baseline and trace-mode comparison.

### Fixed

- Clippy lints and formatting cleaned up across the crate and test suite.

## [0.1.1] - 2026-04-11

Earlier release. See the Git history for details.

## [0.1.0]

Initial release.

[0.2.0]: https://github.com/GoPlasmatic/Orion/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/GoPlasmatic/Orion/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/GoPlasmatic/Orion/releases/tag/v0.1.0
