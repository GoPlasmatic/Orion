# End-to-end suite

Drives a real `orion-server` binary over HTTP with the `orion-cli` binary,
both built from this tree — the one place the workspace's API contract
(server ⇄ `orion-api` ⇄ `orion-client` ⇄ CLI) is exercised end to end at
the same commit. Shell-based, not `cargo test`; needs `jq` and `curl`.
CI runs it on every PR (the `cli-e2e` job). The wider test estate is mapped
in [`TESTING.md`](../../TESTING.md).

## Run it

```bash
just e2e                     # from the repo root — builds both binaries, runs everything
./tests/e2e/run.sh           # same thing
./tests/e2e/run.sh 01_health # one suite
./tests/e2e/run.sh 07 08     # several, by number prefix
```

Environment knobs (all optional):

| Variable | Effect |
|----------|--------|
| `ORION_BIN` | orion-server binary (default: `target/debug/orion-server`) |
| `ORION_CLI` | orion-cli binary (default: `target/debug/orion-cli`) |
| `E2E_PORT` | Fixed server port (default: auto-pick a free one) |
| `E2E_SKIP_BUILD=1` | Don't `cargo build`, use existing binaries |
| `E2E_DEBUG=1` | Log every CLI invocation and its output |
| `E2E_KEEP_SERVER=1` | Leave the server running after the tests |

The runner starts a throwaway server (temp SQLite database, temp config,
random port), waits for `/health`, runs the suites, and tears everything
down — nothing to start or clean up manually.

## Layout

```
tests/e2e/
├── run.sh          # entry point: prerequisites, build, server lifecycle, suite discovery
├── helpers.sh      # framework: assertions, CLI wrappers, server control, case runner
├── suites/         # 16 suites, sourced in filename order
├── cases/          # data-driven runtime-behaviour cases (run by suite 13)
└── fixtures/       # workflow / connector / request JSON used by the suites
```

| Suite | Covers |
|-------|--------|
| `01_health` | Health & connectivity |
| `02_workflows_crud` | Workflows CRUD |
| `03_workflows_status` | Workflow status lifecycle (draft → active → archived) |
| `04_workflows_import_export` | Bulk import/export, `--dry-run` |
| `05_workflows_test` | Server-side workflow dry runs |
| `06_connectors_crud` | Connectors CRUD |
| `07_data_sync` | Synchronous data processing through channels |
| `08_data_async` | Async processing, traces, `trace_token` |
| `09_channels_lifecycle` | Channel CRUD, status transitions, versions, export filters |
| `10_engine_control` | Engine status and hot reload |
| `11_error_handling` | Error envelope rendering, failure exit codes |
| `12_full_lifecycle` | A full create→activate→send→archive walk |
| `13_use_cases` | Data-driven cases: scenarios from [`examples/use-cases/`](../../examples/use-cases/) (which deploy the shipped example packages) plus the runtime-behaviour cases in `cases/` |
| `14_read_only_commands` | The read-only CLI verbs — `functions`, `metrics`, `audit-logs`, `dlq`, `completions`, `config show` |
| `15_vars_and_secrets` | `[vars]` and `[secrets]` end to end: the harness's config file declares one of each, so this is the only layer that covers `${VAR}` substitution into a var, an `env://` reference resolved at startup, and both reaching a workflow on a real process |
| `16_plugins` | The plugin entity through the CLI against a server with the sandbox on: upload from a manifest (the component read beside it), activate, a workflow serving through a plugin function including a `template_at` field, the archive gate, export with artifacts, import, delete. Suite 13's `fixed-width-statement` case deploys the example codec the same way (`"plugins"` in the case file) |

The suites speak the v1.0 API: every send goes through a channel bound to
exactly one workflow (`create_channel` in `helpers.sh`), and reading a
trace needs the `trace_token` from the async submit.

## Adding coverage

A new test inside an existing suite is a bash function plus a
`run_test "name" fn` line. A new suite is a new `NN_name.sh` in `suites/`
(sourced in filename order; keep one concern per suite). Data-driven cases
(format: [`examples/use-cases/README.md`](../../examples/use-cases/README.md))
are picked up by suite 13 automatically and split by role: a *scenario*
showing off a shipped example package goes in `examples/use-cases/` with its
workflow referenced by file; a *runtime-behaviour* check (lifecycle,
masking, error paths) goes in `cases/` here.
