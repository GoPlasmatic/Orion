# Test & Promote a Service

A service you can call is not yet a service you can ship. This tutorial takes a
working workflow, proves it offline, and moves it to a second instance as one
versioned artifact.

In this guide, you will:

- validate a workflow file without a server,
- run it offline against sample input, with connector calls answered from a
  stub file,
- freeze that run as a regression case that fails the build if the logic drifts,
- export the deployed service as a **package** and apply it to a second local
  instance.

You need a clone of the repository — the examples below run against files in it:

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion
```

## 1. Lint the workflow

`lint` reads a workflow JSON file and applies the same validators the admin API
applies on create — task shapes, function names, and each function's `input`
schema:

```bash
orion-server lint examples/packages/high-value-order/workflow.json
```

```
'examples/packages/high-value-order/workflow.json' is valid.
```

It needs no server, no database, and no network, so it is the cheapest gate you
can put in front of a pull request. It exits non-zero on any finding.

Every `orion-server` subcommand run without `-c` also prints
`Note: no config file specified…` on **stderr**. It is not a warning about your
workflow, and `2>/dev/null` or a `-c config.toml` silences it.

## 2. Dry-run it offline

`dry-run` executes the workflow in an in-process engine and prints the per-task
trace. Give it a payload file — the bare payload, without the `{"data": …}`
envelope the HTTP API uses:

```bash
echo '{ "order_id": "ORD-9182", "total": 25000 }' > /tmp/order.json

orion-server dry-run -w examples/packages/high-value-order/workflow.json \
  -i /tmp/order.json
```

It prints one JSON document on stdout — `matched`, the per-task `trace`, the
final `output` context, and any `errors` — so `jq` can read it. Here `flag`
fires and the output carries `data.order.flagged: true`:

```bash
orion-server dry-run -w examples/packages/high-value-order/workflow.json \
  -i /tmp/order.json | jq '.output.order'
```

Run it again with `"total": 50` and the `flag` task is skipped by its condition
instead. That is the fastest way to change logic and see the result: no server, no
restart, no request.

## 3. Stub the calls that leave the process

`high-value-order` only maps data. A workflow that reads a database or calls an
API cannot run offline unless something answers those calls — which is what a
**stub file** is: canned responses, keyed by function and by the connector the
task names.

Write one out, along with a payload for the workflow to run against:

```bash
cat > /tmp/stubs.json <<'JSON'
{
  "data_write": { "orders-db": { "status": "ok", "rows_affected": 1, "returning": [{ "id": 4 }] } },
  "data_query": { "orders-db": [ { "id": 1, "name": "Ada Lovelace", "orders": [] } ] }
}
JSON

# `-i` takes the bare business payload, like step 2 — `request.json` is the
# HTTP body, so unwrap its `data` key.
jq '.data' examples/packages/postgres-orders/request.json > /tmp/order-with-customer.json
```

Then run the workflow against both:

```bash
orion-server dry-run -w examples/packages/postgres-orders/workflow.json \
  -i /tmp/order-with-customer.json --stubs /tmp/stubs.json
```

One rule matters more than the format: **a task with no matching stub fails**,
and the error names the stub that would satisfy it. A half-stubbed run reporting
success would be worse than no stubs at all, because it looks like a pass.

The full stub-file reference — wildcards, inline stubs, and the two mistakes the
parser catches for you — is
[Test Workflows Offline](../build/testing.md#stub-the-calls-that-leave-the-process).

## 4. Freeze the run as a regression case

A dry run you have to read is a demo. A **case file** is the same run with the
answer written down, so a machine can read it instead.

A case is any `*.case.json` file — the suffix is what separates cases from the
workflows and fixtures beside them:

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

```
  ok    flags high-value orders
  FAIL  leaves small orders alone
          data.order.flagged: expected false, got true

1 passed, 1 failed (2 case(s))
```

`workflow` and `stubs_file` resolve relative to the case file. `expect` maps
dotted output paths to expected values, and `expect_errors` defaults to empty —
so a workflow that starts failing its tasks cannot pass silently. Every field is
in [Test Workflows Offline](../build/testing.md#build-a-regression-suite).

Together, `lint` and `test` gate CI without a server, a database, or a secret.

## 5. Deploy it to the first instance

With a server running on `http://localhost:8080`:

```bash
./examples/deploy.sh high-value-order
```

That creates and activates the workflow and the channel, then sends
`request.json` and prints the response. Every entity it creates carries the tag
`pkg:high-value-order`, which is what makes the next step possible.

## 6. Export it as a package

A **package** is the unit Orion promotes: the channels of one service, their
workflows, and every connector those workflows reference — captured as one
versioned JSON artifact.

```bash
orion-server package export -s http://localhost:8080 \
  --tag pkg:high-value-order --name high-value-order --version 1.0.0 \
  -o high-value-order-1.0.0.json
```

Export selects **channels**; each selected channel pulls in its workflow, and
each workflow pulls in its connectors. That set is the package's **closure**.

```
wrote high-value-order@1.0.0 (0 connectors, 1 workflows, 1 channels) to high-value-order-1.0.0.json
```

Validate the artifact offline before it travels:

```bash
orion-server package lint -f high-value-order-1.0.0.json
```

```
'high-value-order-1.0.0.json' is a valid package: high-value-order@1.0.0 — 0 connectors, 1 workflows, 1 channels
```

## 7. Apply it to a second instance

Start a second server on another port with its own database — this stands in for
QA or production:

```bash
ORION_SERVER__PORT=9090 ORION_STORAGE__URL="sqlite:orion-qa.db" orion-server
```

Ask what would happen before anything is written:

```bash
orion-server package plan -s http://localhost:9090 -f high-value-order-1.0.0.json
```

```
  workflows  high-value-order             created
  channels   high-value-orders            created
  workflows  high-value-order             gate pending apply order: Workflow 'high-value-order' not found
  channels   high-value-orders            gate pending apply order: Channel 'high-value-orders' not found
plan: high-value-order@1.0.0 applies cleanly to http://localhost:9090
```

`plan` writes nothing. It reports the per-entity action `apply` would take,
verifies every declared requirement exists on the target, and checks the
activation gates. The `gate pending apply order` lines are not errors: a
channel's activation gate wants an active workflow that this same apply is
about to create, so the gate can only be satisfied in order. The last line is
the verdict.

```bash
orion-server package apply -s http://localhost:9090 -f high-value-order-1.0.0.json
```

```
staged workflows: 1 written, 0 unchanged, 0 failed
staged channels: 1 written, 0 unchanged, 0 failed
activated workflows 'high-value-order'
activated channels 'high-value-orders'
applied high-value-order@1.0.0 to http://localhost:9090
```

`apply` stages every entity, activates them in dependency order — connectors,
then workflows, then channels — and reloads the engine **once** at the end.

> [!NOTE]
> Against an instance with admin auth enabled, `export`, `plan`, `apply` and
> `diff` all read the admin token from the `ORION_ADMIN_TOKEN` environment
> variable. `lint` needs no server and no token.

## Verify it

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

```
  unchanged  workflow 'high-value-order'
  unchanged  channel 'high-value-orders'
no drift: high-value-order@1.0.0 matches http://localhost:9090
```

`diff` compares content hashes and exits non-zero on drift, so it works as a CI
check that production still runs what you shipped.

Re-running the same `apply` is a no-op: the target recognises the receipt and
answers `high-value-order@1.0.0 is already applied with identical content —
nothing to do`. A *changed* artifact reusing an applied version is refused with
a `409` instead: content changes ride a version bump.

## Next steps

- [Packages](../concepts/packages.md) — what a package is, and why the module
  boundary sits there.
- [Promote Between Environments](../operate/promotion.md) — receipts, rollback, secrets
  that survive the trip, and the `requires` boundary.
- [Author Workflows](../build/workflows.md) — the how-to layer, now that you
  can test what you write.
- [CLI Reference](../reference/cli.md) — every flag of `lint`, `dry-run`,
  `test`, and `package`.
