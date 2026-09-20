<!-- description: Prove an Orion workflow offline with lint, dry-run and regression cases, then export, plan and apply it to a second instance as one versioned package. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-09-14 -->

# Test and promote a service

A service you can call is not yet a service you can ship. This tutorial takes a working workflow, proves it offline, and moves it to a second instance as one versioned artifact.

## What you will learn

- How to validate and run a workflow file with no server.
- What a stub file is, and why a half-stubbed run fails rather than passes.
- How a regression case turns a dry run into a CI gate.
- What a package is, and what `plan`, `apply` and `diff` each promise.

## Before you start

Tested with Orion 1.9.0. You need:

- `orion-server`, `curl`, `jq` and Git
- one Orion instance on `http://localhost:8080`; the promotion steps start a second one on port `9090`
- a POSIX shell (on Windows, WSL)
- [Build your first service](./first-service.md) behind you, so the definitions make sense

The commands run against files in the repository, so clone it first:

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion
```

## 1. Lint the workflow

`lint` reads a workflow file and applies the validators the admin API applies on create: task shapes, function names, and each function's `input` schema:

```bash
orion-server lint examples/packages/high-value-order/workflow.json
```

Output:

```text
'examples/packages/high-value-order/workflow.json' is valid.
```

Point it at a directory instead and it validates the whole definition set plus the references between its files, which a per-file run cannot check. That is the form to use in CI. It needs no server, no database and no network, and it exits non-zero on any finding.

`lint` has two neighbours. `orion-server fmt` writes every definition file in the one house style (`--check` in CI). `orion-server clippy` reports what `lint` accepts but an author would want to know, such as a condition that can never match. It reports only when it is certain. See [Definition style](../../reference/fmt.md) and [Advisory checks](../../reference/clippy/index.md).

Every `orion-server` subcommand run without `-c` prints `Note: no config file specified…` on stderr. It is not about your workflow; `-c config.toml` or `2>/dev/null` silences it.

## 2. Dry-run it offline

`dry-run` executes the workflow in an in-process engine and prints the per-task trace. Give it a payload file holding the bare payload, without the `{"data": …}` envelope the HTTP API uses:

```bash
echo '{ "order_id": "ORD-9182", "total": 25000 }' > /tmp/order.json

orion-server dry-run -w examples/packages/high-value-order/workflow.json \
  -i /tmp/order.json
```

It prints one JSON document: `matched`, the per-task `trace`, the final `output` context, and any `errors`. Read the part that matters with `jq`:

```bash
orion-server dry-run -w examples/packages/high-value-order/workflow.json \
  -i /tmp/order.json | jq '.output.order'
```

Output:

```json
{
  "order_id": "ORD-9182",
  "total": 25000,
  "flagged": true,
  "alert": "High-value order: $25000"
}
```

Run it again with `"total": 50` and the `flag` task is skipped by its condition. That is the fastest loop for changing logic and seeing the result: no server, no restart, no request.

## 3. Stub the calls that leave the process

`high-value-order` only maps data. A workflow that reads a database or calls an API cannot run offline unless something answers those calls. A *stub file* is that something: canned responses, keyed by function and by the connector the task names.

Write one, along with a payload for the workflow to run against:

```bash
cat > /tmp/stubs.json <<'JSON'
{
  "data_write": { "orders-db": { "status": "ok", "rows_affected": 1, "returning": [{ "id": 4 }] } },
  "data_query": { "orders-db": [ { "id": 1, "name": "Ada Lovelace", "orders": [] } ] }
}
JSON

jq '.data' examples/packages/postgres-orders/request.json > /tmp/order-with-customer.json
```

The second command unwraps the `data` key, because `request.json` is an HTTP body and `-i` takes the bare payload. Then run the workflow against both files:

```bash
orion-server dry-run -w examples/packages/postgres-orders/workflow.json \
  -i /tmp/order-with-customer.json --stubs /tmp/stubs.json
```

One rule matters more than the format: a task with no matching stub fails, and the error names the stub that would satisfy it. A half-stubbed run that reported success would be worse than no stubs, because it would look like a pass. The full stub-file reference is in [Test a workflow offline](../../guides/author/testing.md#stub-the-calls-that-leave-the-process).

## 4. Freeze the run as a regression case

A dry run you have to read is a demo. A *case file* is the same run with the answer written down, so a machine can read it instead. A case is any `*.case.json` file; the suffix is what separates cases from the workflows beside them:

```json
{
  "name": "flags high-value orders",
  "workflow": "../packages/high-value-order/workflow.json",
  "input": { "order_id": "ORD-9182", "total": 25000 },
  "expect": {
    "data.order.flagged": true,
    "data.order.alert": "High-value order: $25000"
  }
}
```

`orion-server test` runs a directory of them and exits non-zero on any failure:

```bash
orion-server test examples/workflow-tests
```

Output, for a suite with one failing case:

```text
  ok    flags high-value orders
  FAIL  leaves small orders alone
          data.order.flagged: expected false, got true

1 passed, 1 failed (2 case(s))
```

`workflow` and `stubs_file` resolve relative to the case file. `expect` maps dotted output paths to expected values, and `expect_errors` defaults to empty, so a workflow that starts failing its tasks cannot pass silently. Every field is in [Test a workflow offline](../../guides/author/testing.md#build-a-regression-suite). Together, `lint` and `test` gate CI without a server, a database or a secret.

## 5. Deploy it to the first instance

With a server on `http://localhost:8080`:

```bash
./examples/deploy.sh high-value-order
```

The script creates and activates the workflow and the channel, sends `request.json`, and prints the response. Every entity it creates carries the tag `pkg:high-value-order`, which is what makes the next step possible.

## 6. Export it as a package

A *package* is the unit Orion promotes. It holds the channels of one service, their workflows, and every connector those workflows reference, as one versioned JSON artifact:

```bash
orion-server package export -s http://localhost:8080 \
  --tag pkg:high-value-order --name high-value-order --version 1.0.0 \
  -o high-value-order-1.0.0.json
```

Output:

```text
wrote high-value-order@1.0.0 (0 connectors, 1 workflows, 1 channels) to high-value-order-1.0.0.json
```

Export selects channels. Each selected channel pulls in its workflow, and each workflow pulls in its connectors; that set is the package's *closure*. Validate the artifact offline before it travels:

```bash
orion-server package lint -f high-value-order-1.0.0.json
```

Output:

```text
'high-value-order-1.0.0.json' is a valid package: high-value-order@1.0.0 — 0 connectors, 1 workflows, 1 channels
```

> [!TIP]
> If your definitions live in a directory rather than on an instance, [`orion-server compile`](../../reference/cli/orion-server/compile.md) builds the same artifact straight from the files and resolves shared values and fragments on the way. Everything from here on is identical.

## 7. Apply it to a second instance

Start a second server on another port with its own database. It stands in for QA or production:

```bash
ORION_SERVER__PORT=9090 ORION_STORAGE__URL="sqlite:orion-qa.db" orion-server
```

Ask what would happen before anything is written:

```bash
orion-server package plan -s http://localhost:9090 -f high-value-order-1.0.0.json
```

Output:

```text
  workflows  high-value-order             created
  channels   high-value-orders            created
  workflows  high-value-order             gate pending apply order: Workflow 'high-value-order' not found
  channels   high-value-orders            gate pending apply order: Channel 'high-value-orders' not found
plan: high-value-order@1.0.0 applies cleanly to http://localhost:9090
```

`plan` writes nothing. It reports the action `apply` would take per entity, verifies every declared requirement exists on the target, and checks the activation gates. The `gate pending apply order` lines are not errors. A channel's gate wants an active workflow that this same apply is about to create, so it can only be satisfied in order. The last line is the verdict.

```bash
orion-server package apply -s http://localhost:9090 -f high-value-order-1.0.0.json
```

Output:

```text
staged workflows: 1 written, 0 unchanged, 0 failed
staged channels: 1 written, 0 unchanged, 0 failed
activated workflows 'high-value-order'
activated channels 'high-value-orders'
applied high-value-order@1.0.0 to http://localhost:9090
```

`apply` stages every entity, activates them in dependency order (plugins, connectors, models, workflows, channels), and reloads the engine once at the end. This package carries only the last two kinds.

> [!NOTE]
> Against an instance with admin auth enabled, `export`, `plan`, `apply` and `diff` read the admin token from the `ORION_ADMIN_TOKEN` environment variable. `lint` needs no server and no token.

## Verify

Call the second instance. The service you built on 8080 answers on 9090:

```bash
curl -s -X POST http://localhost:9090/api/v1/data/high-value-orders \
  -H 'Content-Type: application/json' \
  --data @examples/packages/high-value-order/request.json
```

Then confirm the two instances agree:

```bash
orion-server package diff -s http://localhost:9090 -f high-value-order-1.0.0.json
```

Output:

```text
  unchanged  workflow 'high-value-order'
  unchanged  channel 'high-value-orders'
no drift: high-value-order@1.0.0 matches http://localhost:9090
```

`diff` compares content hashes and exits non-zero on drift, so it works as a CI check that production still runs what you shipped. Re-running the same `apply` is a no-op: the target recognizes the receipt and answers that the version is already applied with identical content. A *changed* artifact reusing an applied version is refused with `409`; content changes ride a version bump.

## Clean up

Stop the port-9090 server with `Ctrl-C` in its terminal. From the repository root, remove only the files this tutorial created:

```bash
rm -i orion-qa.db high-value-order-1.0.0.json
```

The interactive prompt protects similarly named files. Nothing under `examples/` was modified.

## Recap

- `lint`, `dry-run` and `test` run with no server. A stub file answers connector calls, and a missing stub is a failure, never a pass.
- A case file is a dry run with the answer written down. `expect_errors` defaults to empty, so silent failure cannot pass.
- A package is a service's channels plus their closure, versioned. `plan` writes nothing, `apply` activates in dependency order and reloads once, `diff` detects drift.
- An applied version is content-immutable. A changed artifact needs a new version.

## Next steps

- [Packages](../../concepts/packages.md): what a package is, and why the module boundary sits there.
- [Promote between environments](../../operate/maintain/promotion.md): receipts, rollback, secrets that survive the trip, and the `requires` boundary.
- [Author a workflow](../../guides/author/workflows.md): the how-to layer, now that you can test what you write.
- [CLI](../../reference/cli/index.md): every flag of `lint`, `dry-run`, `test` and `package`.
