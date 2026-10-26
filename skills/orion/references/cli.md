# Orion command workflows

Read this reference for offline validation, compilation, deployments, live
operations, output handling, and diagnosis. Run the relevant command's
`--help` before using flags not shown here.

## Connection and output

Connection precedence is CLI flags, then environment, then user config:

```bash
orion-cli config set-server http://localhost:8080
orion-cli health
```

Use `--server` or `ORION_SERVER_URL` for automation, and `--api-key` or
`ORION_API_KEY` when admin auth is enabled. `--change-context` labels audit
events, for example `ticket=OPS-4412`. Keep secrets out of command history.

Global `--output json` keeps the API response envelope. Arrays from list
commands are generally at `.data`. `--quiet` prints minimal output. Always
check the exit status. Page through a list until it is exhausted when you need
a full inventory.

## Offline authoring

Run local checks before anything touches an instance:

```bash
orion-cli fmt --check ./definitions
orion-cli lint ./definitions
orion-cli clippy ./definitions
orion-cli dry-run -w definitions/workflow.json -i sample.json --stubs stubs.json
orion-cli test ./definitions/tests
```

- `fmt` applies the one house style. Use `--check` in CI.
- `lint` validates shapes and the references between files. Point it at the
  directory, so it sees the whole set.
- `clippy` reports proven issues beyond validity. Read a rule's `--explain`
  before changing behaviour only to satisfy it.
- `dry-run` executes one workflow offline. `--stubs` answers connector calls,
  and a task with no stub fails naming the stub it needs.
- `test` runs `*.case.json` regression cases and exits non-zero on a failure.
  It refuses a directory that holds no case files.
- `sql check` prepares every `db_read`/`db_write` statement against a real
  database. It executes nothing, but it does connect.
- `orion-server validate-config` and `orion-server test-connectivity` are operator commands. They read
  config, and `test-connectivity` contacts the state database.
- `orion-server dump-openapi` prints the built server's API contract.
- `lint`, `clippy`, `compile`, `dry-run` and `test` take `--plugin-dir <dir>`,
  and find a `plugin.toml` in the tree without it. With a manifest in hand,
  plugin task inputs are validated, and `dry-run` and `test` run the component
  for real. A function no manifest covers is reported unverifiable, not
  invalid. A function whose component is absent fails as
  `PLUGIN_ARTIFACT_UNAVAILABLE`.

Assert trace and task state and the intermediate context, not only the final
output.

## Compile a definition set

A set holds workflows, channels, connectors, shared values, fragments and
plugins. Compile it into a deployment directory:

```bash
orion-cli compile ./definitions -o dist/deployment
```

`compile` runs every `lint` check first and writes nothing if one fails. It
resolves `$from` and `use`, inlines any `plugin.toml` component, and turns each
connector into a connection file. The output is `deployment.json`,
`definitions/<hex>.json` and `connections/<id>.json`, with no revision
numbers in it. The same set compiles to the same bytes.

Files that are not definitions, such as `request.json`, are skipped with a
note. A connector holding a literal credential, or a `db` connector without a
`driver`, is an error.

## Plan, apply and verify

```bash
orion-cli deployments plan -d dist/deployment
orion-cli deployments apply -d dist/deployment
orion-cli engine status
```

`plan` uploads the definitions and connection revisions, assigns member
revisions against the target's head, and prints what would change. It commits
nothing. Lines read:

- `+ kind id@N`: deployed by this apply.
- `= kind id@N`: unchanged.
- `- kind id@N`: removed. It stops serving on apply.

A changed definition shows as `+ id@2` beside `- id@1`. An apply replaces the
head with the directory's members, so read every `-` line before applying.

`apply` prints the same plan, asks before committing (`--yes` skips the
prompt), and commits under compare-and-swap on the head. Its idempotency key
derives from the head and the manifest digest, so a retry replays the first
answer. `--base-revision <N>` refuses to go ahead unless the head is `N`.
`--reason` is recorded in the audit log. A directory the head already deploys
is reported as up to date and not applied.

The apply answer carries `committed_revision` and `evaluation.converged`.
Then check `orion-cli engine status`: a member the node could not load is
quarantined and listed under its load issues.

Read history with `orion-cli deployments get <revision>`, or the head and every
revision with `GET /api/v2/admin/deployments`.

## Clusters and convergence

Every node builds the head on its own. Wait until they all serve it:

```bash
orion-cli deployments wait 8 --target observed --timeout-ms 30000
```

`--target this-node` waits only for the node that answered. A node that cannot
build a committed head keeps serving its previous generation and reports
`engine_reload` as degraded on `/health`.

## Rollback

A rollback commits an earlier revision's manifest as a new revision:

```bash
orion-cli deployments rollback --target-revision 3 \
  --expected-head-revision 4 --idempotency-key rollback-4-to-3 \
  --reason "revert threshold change"
```

`--expected-head-revision` refuses the rollback if someone deployed in the
meantime. It restores configuration only. Rows written and calls made under
the newer revision stay. Recompiling and applying an older commit of the set
works too: a member whose definition an earlier revision deployed takes that
revision back.

## Traffic splits

`compile` deploys one revision per workflow. To split a channel's traffic
between two revisions of one workflow, write the manifest by hand. List both
workflow revisions as members, and give the channel member `rollouts`
intervals that partition `0..100`:

```json
"rollouts": [
  { "workflow_revision": 1, "start_bucket": 0, "end_bucket": 75 },
  { "workflow_revision": 2, "start_bucket": 75, "end_bucket": 100 }
]
```

Plan it with `deployments plan -f manifest.json --base-revision <head>` and
apply it with `deployments apply -f manifest.json --base-revision <head>
--idempotency-key <key> --reviewed-digest <digest>`. Watch metrics, errors and
traces between ramps.

## Data, traces, and retries

```bash
orion-cli send orders -f request.json
orion-cli send payload-channel --raw -f body.json
orion-cli send reports --async-mode --wait -f request.json
orion-cli traces list --channel orders
orion-cli traces get <trace-id>
orion-cli traces wait <trace-id>
```

An async send acknowledges admission. Fetch or wait for the trace. Trace policy
can make a trace id unavailable, so handle a null or missing id.

`orion-cli dlq list|get|requeue|purge` reads and drains the trace dead-letter
queue. A requeue repeats external effects. Inspect the failure and the
destination's idempotency before retrying, and honour confirmation prompts.

## Scheduled work

A cron channel has no caller, so `send` cannot reach it. Read its runs from the
occurrence ledger, and start one by hand with `cron trigger`:

```bash
orion-cli cron status                  # what is scheduled, next fire, last result
orion-cli cron list --channel-id nightly-order-rollup --status failed
orion-cli cron get <occurrence-id>
orion-cli cron retry <occurrence-id>   # same occurrence: same id, same scheduled_for
orion-cli cron trigger nightly-order-rollup   # a new manual occurrence, now
orion-cli cron cancel <occurrence-id>  # stop one that has not finished
```

`retry` and `trigger` answer different questions. `retry` is another attempt at
work due at a past instant. `trigger` is new work now. Both take the same claim
and singleton a scheduled run takes, so neither runs beside a `forbid`
schedule's live occurrence.

A backlog of `pending` occurrences means the scheduler is behind or off. Check
`components.cron` on `/health` and the node's `cron.enabled`.

## Connections and breakers

A connection revision is created by `apply -d` from the set's connector file.
`orion-cli connections list` shows the stored revisions.
`orion-cli connections probe <id>` runs an audited health probe against its
latest revision (`--revision N` for another) and exits 2 when the backend is
unhealthy.
Circuit breakers are read and reset through
`GET /api/v2/admin/connectors/circuit-breakers` and
`POST /api/v2/admin/connectors/circuit-breakers/{channel}:{connector}`.

## Diagnosis

Use this order:

1. Read the exit status and the structured field errors.
2. Rerun `lint` and `clippy` on the set, and `plan` against the target.
3. Inspect `orion-cli health` and `orion-cli engine status`.
4. Inspect the live function schema with `orion-cli functions list`.
5. Inspect the latest trace and its stable task error code.
6. Check connection probes and circuit-breaker state.
7. Check the DLQ for async work.

Common causes:

- `UNCOMPILED_SOURCE`: the document still holds `$from` or `use`. Compile the
  set.
- `DEPLOYMENT_WOULD_NOT_BUILD`: the plan names why, such as two channels on one
  route or a function no member serves. Fix the set and re-plan.
- `DEPLOYMENT_HEAD_CONFLICT`: another apply landed first. Re-plan against the
  new head.
- `UNRESOLVED_SECRET_REF`: use a field that accepts a reference, a connector,
  or a declared secret.
- request fields are absent: parse `payload` into `data`.
- payload-mode input is nested: resend with `--raw`.
- unexpected object output: check operator spelling and template behaviour.
- a `404` on a route: the head does not carry the channel, or this node has not
  converged yet. Read the head and the node's engine status.
- a channel is quarantined: read its reason in `engine status`. Typical causes
  are a missing connection, an unresolvable secret on this node, and a plugin
  function on a node with `plugins.enabled = false`.
- a cron channel never fires: check `cron.enabled` on every node,
  `components.cron` on `/health`, and the channel's quarantine reason.
- `engine_reload` degraded: the node could not build the committed head and
  serves its previous generation. Read `/health` for the reason.

Do not blindly retry deterministic validation failures. Fix the named field or
configuration.
