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
./tests/e2e/run.sh 03 04     # several, by number prefix
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
random port, plugins on, one `[vars]` entry and one `[secrets]` entry),
waits for `/health`, runs the suites, and tears everything down — nothing to
start or clean up manually.

## How a test deploys

A node serves exactly its committed deployment head, so every test deploys
what it needs the way an operator does: it writes a definition set into its
temp directory (`new_set`, `set_workflow`, `set_channel`, `set_plugin` in
`helpers.sh`), compiles it with `orion-cli compile` and applies it with
`orion-cli deployments apply -d` (`deploy_set`, or `deploy_one` for one
workflow behind one channel). An apply **replaces** the head's members with
the directory's, so a test needing several services deploys them as one set,
and nothing needs cleaning between tests; `undeploy_all` commits an empty
head when a test needs nothing serving. A refused compile or apply leaves
`CLI_EXIT` non-zero and the reason in `CLI_STDERR`, so a refusal is asserted
the same way either way. Each test runs under errexit, so its first failing
assertion fails it.

A plugin and the workflows that call it deploy in one apply: upload leaves a
qualified function name the node does not serve yet to the deployment, whose
build check compiles the workflow against the plugins it carries. An apply
whose head would not build is refused before it commits.

## Layout

```
tests/e2e/
├── run.sh          # entry point: prerequisites, build, server lifecycle, suite discovery
├── helpers.sh      # framework: assertions, CLI wrappers, server control, case runner
├── suites/         # 11 suites, sourced in filename order
├── cases/          # data-driven runtime-behaviour cases (run by suite 06)
└── fixtures/       # workflow / request JSON used by the suites
```

| Suite | Covers |
|-------|--------|
| `01_health` | Health (JSON, quiet, YAML) and engine status: nothing deployed, then a two-service set |
| `02_definition_test` | `POST /definitions/test` — a compiled workflow definition run once on the node without deploying it: its output, a skipped task in the trace, a refused definition, nothing deployed after |
| `03_data_sync` | Synchronous sends through deployed channels; an unknown channel refused; a redeploy replacing what serves |
| `04_data_async` | Async submits, `traces wait`/`get`, `trace_token`, `send --wait`, the timeout contract |
| `05_error_handling` | Failure exit codes: a malformed payload, sets `compile` refuses (a channel naming a missing workflow, an unknown function, an inline credential), an apply of a non-deployment, and a refused deploy leaving the head serving |
| `06_use_cases` | Data-driven cases: scenarios from [`examples/use-cases/`](../../examples/use-cases/) (which deploy the shipped example packages whole) plus the runtime-behaviour cases in `cases/` |
| `07_read_only_commands` | The read-only CLI verbs — `functions`, `metrics`, `audit-logs`, `backups`, `connections`, `deployments capabilities`, `dlq`, the list paging flags, `completions`, `config show` |
| `08_vars_and_secrets` | `[vars]` and `[secrets]` end to end: the harness's config file declares one of each, so this is the only layer that covers `${VAR}` substitution into a var, an `env://` reference resolved at startup, and both reaching a workflow on a real process; the stored definition holds the secret's name; an undeclared secret refused at upload; a stray reference refused at compile |
| `09_plugins` | A plugin as a deployment member: the fixture manifest and component compiled in, its functions in the catalogue and serving two workflows (a `template_at` field included), a re-apply committing nothing, a deployment without it dropping its functions, and a workflow calling it refused on a node that does not serve it. Suite 06's `fixed-width-statement` case deploys the example codec the same way |
| `10_cron` | A schedule through the CLI: a deployed `protocol: "cron"` channel firing every second, occurrences read back with `cron status\|list\|get`, `cron trigger` running one now, an undeploy removing the schedule, and caller-shaped config and five-field expressions refused at compile |
| `11_deployments` | Deployment revisions on a server of its own (so its revision numbers are exact): the `order-classification` example compiled and deployed with `deployments plan\|apply -d`; one workflow changed and only its member's revision moving; an unchanged directory applying nothing; a member left out of the next apply stopping; `deployments rollback` serving the earlier revision again |

## Adding coverage

A new test inside an existing suite is a bash function plus a
`run_test "name" fn` line. A new suite is a new `NN_name.sh` in `suites/`
(sourced in filename order; keep one concern per suite). Data-driven cases
(format: [`examples/use-cases/README.md`](../../examples/use-cases/README.md))
are picked up by suite 06 automatically and split by role: a *scenario*
showing off a shipped example package goes in `examples/use-cases/` and
deploys the package whole (`"packages"`); a *runtime-behaviour* check
(secret locators, error paths, ingress modes) goes in `cases/` here.
