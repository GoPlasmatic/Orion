# Orion command workflows

Read this reference for local validation, live administration, packages, output
handling, and diagnosis. Run the relevant command's `--help` before using
flags not demonstrated here.

## Connection and output

Connection precedence is CLI flags, environment, then user config:

```bash
orion-cli config set-server http://localhost:8080
orion-cli health
```

Use `--server` or `ORION_SERVER_URL` for automation, and `--api-key` or
`ORION_API_KEY` when admin auth is enabled. `--change-context` labels audit
events. Avoid putting secrets directly in command history.

Global `--output json` preserves the API response envelope. Arrays from list
commands are generally at `.data`. `--quiet` gives minimal output. Always
check exit status. Paginate until exhausted when computing a full inventory.

## Offline authoring

Run local checks before a server write:

```bash
orion-server fmt --check ./definitions
orion-server lint ./definitions
orion-server clippy ./definitions
orion-server dry-run -w workflow.json -i sample.json
orion-server test ./workflow-tests
```

- `fmt` applies canonical style; use `--check` in CI.
- `lint` validates shapes and cross-definition dependencies. Directory lint
  understands the whole set.
- `clippy` reports proof-based issues beyond validity. Read a rule's
  explanation before changing behavior solely to satisfy it.
- `dry-run` executes one workflow offline with deterministic egress stubs.
- `test` runs declarative regression cases.
- `validate-config`, `preflight`, and `test-connectivity` are operator
  workflows; inspect help because they may load config or contact dependencies.
- `dump-openapi` emits the built server's API contract.
- All five offline commands take `--plugin-dir <dir>` (and find a
  `plugin.toml` already in the tree without it). With a manifest in hand,
  plugin task inputs are validated as the admin API validates them and
  `dry-run` / `test` run the component for real — plugin functions are never
  stubbed. A function no manifest covers is reported unverifiable, not
  invalid; a function whose component is absent fails as
  `PLUGIN_ARTIFACT_UNAVAILABLE`.

Assert trace/task state and intermediate context, not only final output.

## Definition sets and compilation

A set may include workflows, channels, connectors, shared values, and fragments.
Lint the directory, then compile it:

```bash
orion-server compile ./definitions \
  --name orders --version 2.3.0 -o orders-2.3.0.json
```

Compilation resolves `$from` and `use`, validates the expanded set, and emits
a deployable artifact — inlining any `plugin.toml` in the tree together with
its component. The admin API refuses unresolved source syntax. Other output
formats exist for per-file/bulk import; inspect `compile --help`.

## Live lifecycle

Safe workflow sequence:

```bash
orion-cli workflows get order-processing
orion-cli workflows dependencies order-processing
orion-cli workflows new-version order-processing
orion-cli workflows update order-processing -f workflow.json
orion-cli workflows test order-processing -f sample.json --trace
orion-cli workflows activate order-processing --dry-run
orion-cli workflows activate order-processing
```

For a new ID, use `create`. Channels and plugins follow the same
draft/version/activation model. Activate a channel's workflow first, and a
workflow's plugins before the workflow:

```bash
orion-cli plugins create -f plugin.toml      # the component is read beside it
orion-cli plugins activate <plugin-id>
orion-cli plugins dependencies <plugin-id>   # what would break on archive
```

A plugin version whose schema an active dependant no longer satisfies is
refused activation with a `409`. `plugins create --signature` is required when
the server configures `[plugins.trust]`. All of it answers `400` when
`plugins.enabled` is off.

Transition dry-runs return findings in HTTP 200; the CLI maps invalid findings
to exit code 1, so gate automation on the exit code.

Connectors have no drafts. Validate before create/update, test them, and reload
when required. Treat connector updates as live dependency changes.

## Atomic visibility and rollout

Batch related transitions with one reload:

```bash
orion-cli workflows activate order-processing --defer-reload
orion-cli channels activate orders --defer-reload
orion-cli engine reload
```

Until reload, stored state and the serving generation deliberately differ.
Do not leave a deferred batch unfinished.

For an already active multi-version rollout, change a version's share with:

```bash
orion-cli workflows rollout order-processing -p 50
orion-cli workflows rollout order-processing -p 100
```

The CLI's activation command does not currently accept an initial percentage;
use the documented status API request with `rollout_percentage` when introducing
a canary. Preflight it first. Monitor metrics, errors, and traces between ramps.
Active versions must form a valid traffic partition.

## Data, traces, and retries

```bash
orion-cli send orders -f request.json
orion-cli send payload-channel --raw -f body.json
orion-cli traces list --channel orders
orion-cli traces get <trace-id>
orion-cli traces wait <trace-id>
```

Async sends acknowledge admission; fetch or wait for the trace. Trace policy can
make a trace ID unavailable, so handle null/missing IDs.

DLQ retry repeats external effects. Inspect the failure and idempotency
guarantees before retrying and honor confirmation prompts.

## Scheduled work

A cron channel has no caller, so `send` cannot reach it. Its runs are read from
the occurrence ledger and started, when started by hand, through the channel:

```bash
orion-cli cron status                  # what is scheduled, next fire, last result
orion-cli cron list --channel-id nightly-rollup --status failed
orion-cli cron get <occurrence-id>
orion-cli cron retry <occurrence-id>   # same occurrence: same id, same scheduled_for
orion-cli channels trigger nightly-rollup   # a new manual occurrence
```

`retry` and `trigger` answer different questions: `retry` is another attempt at
work that was due at a past instant, `trigger` is new work now. Both take the
same claim and singleton a scheduled run takes, so neither can run beside a
`forbid` schedule's live occurrence.

A backlog of `pending` occurrences means the scheduler is behind or off; check
`components.cron` on `/health` and the node's `cron.enabled`.

## Packages and promotion

Packages compute dependency closure, plan conflicts, apply in dependency order,
reload once, and record a receipt:

```bash
orion-server package lint -f orders-2.3.0.json
orion-server package plan -s https://staging.example -f orders-2.3.0.json
orion-server package diff -s https://staging.example -f orders-2.3.0.json
orion-server package apply -s https://staging.example -f orders-2.3.0.json
```

Before apply, confirm the target, authentication source, referenced secrets and
vars, connector gates, and plan. Preserve the exact promoted artifact. Rollback
is reapplying a known-good artifact.

## Diagnosis

Use this order:

1. Read exit status and structured field errors.
2. Run validate/lint or transition `--dry-run`.
3. Inspect health, engine status, and readiness.
4. Inspect dependencies and the live function schema.
5. Inspect the latest trace and stable task error code.
6. Check connector connectivity and circuit-breaker state.
7. Check the DLQ for async work.
8. Reload only after deferred changes or confirmed serving-generation drift.

Common causes:

- `UNCOMPILED_SOURCE`: compile the set.
- `UNRESOLVED_SECRET_REF`: use a supported key field, connector, or declared
  secret.
- request fields are absent: parse `payload` into `data`.
- payload-mode input is nested: resend with `--raw`.
- unexpected object output: check operator spelling/template behavior.
- unavailable channel: inspect workflow activity, rollout partition, connector
  references, and quarantine findings.
- a cron channel never fires: check `cron.enabled` on every node,
  `components.cron` on `/health`, and the channel's quarantine finding.
- a workflow naming a plugin function is quarantined: check `plugins.enabled`,
  the plugin's status, and that its active version still declares that
  function.
- imported connector fails: supply secrets masked from the export.
- stored state differs from traffic: finish the deferred reload.

Do not blindly retry deterministic validation failures; fix the named field or
configuration.
