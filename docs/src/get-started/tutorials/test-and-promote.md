<!-- description: Prove an Orion workflow offline with lint, dry-run and regression cases, then compile it into a deployment and apply it to a running instance. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-10-25 -->

# Test and deploy a service

A service you can call is not yet a service you can ship. This tutorial takes a working workflow, proves it offline, and deploys it to an instance as one reviewed revision.

## What you will learn

- How to validate and run a workflow file with no server.
- What a stub file is, and why a half-stubbed run fails rather than passes.
- How a regression case turns a dry run into a CI gate.
- How `compile` turns a definition set into a deployment, and what `plan`, `apply` and `rollback` each do.

## Before you start

Tested with Orion 1.12.0. You need:

- `orion-server`, `orion-cli`, `curl`, `jq` and Git
- one Orion instance on `http://localhost:8080` for the deployment steps
- a POSIX shell (on Windows, WSL)
- [Build your first service](./first-service.md) behind you, so the definitions make sense

The commands run against files in the repository, so clone it first:

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion
```

## 1. Lint the workflow

`lint` reads a workflow file and applies the checks the server runs on an upload. Those cover task shapes, function names, and each function's `input` schema:

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

`orion-server test` runs a directory of them and exits non-zero on any failure. Two example packages call plugin functions, so hand the runner their directories too:

```bash
orion-server test examples/workflow-tests \
  --plugin-dir examples/packages/fixed-width-statement \
  --plugin-dir examples/packages/c4-referee
```

Without the two `--plugin-dir` flags, those four cases fail with `PLUGIN_ARTIFACT_UNAVAILABLE` rather than pass on a stub.

Output, for a suite with one failing case:

```text
  ok    flags high-value orders
  FAIL  leaves small orders alone
          data.order.flagged: expected false, got true

1 passed, 1 failed (2 case(s))
```

`workflow` and `stubs_file` resolve relative to the case file. `expect` maps dotted output paths to expected values, and `expect_errors` defaults to empty, so a workflow that starts failing its tasks cannot pass silently. Every field is in [Test a workflow offline](../../guides/author/testing.md#build-a-regression-suite). Together, `lint` and `test` gate CI without a server, a database or a secret.

## 5. Compile it into a deployment

A node serves exactly what its committed deployment names. `compile` turns a definition set into the directory a deployment is made from. It runs every `lint` check first and writes nothing if one fails:

```bash
orion-server compile examples/packages/high-value-order -o dist/high-value-order
```

Output:

```text
note: examples/packages/high-value-order/request.json is not a channel, workflow or connector — skipped
examples/packages/high-value-order: 0 connector(s), 1 workflow(s), 1 channel(s) — 0 error(s), 0 warning(s)
wrote a deployment of 1 workflow(s), 1 channel(s), 0 plugin(s) and 0 connection(s) to dist/high-value-order
```

The directory holds `deployment.json`, which lists the members, and one file per definition under `definitions/`, named by its digest. It carries no revision numbers. The target assigns those when you apply it. The layout is in [Deployment directories](../../reference/cli/orion-server/compile.md#deployment-directories).

## 6. Plan and apply it

Ask what would change before anything is committed:

```bash
orion-cli --server http://localhost:8080 deployments plan -d dist/high-value-order
```

Output, on an instance with nothing deployed yet (the JSON plan that follows is cut):

```text
Plan: dist/high-value-order against head revision 0
  + channel high-value-orders@1
  + workflow high-value-order@1
  2 to deploy, 0 unchanged, 0 removed; reviewed manifest digest sha256:036d4cc2…
```

`plan` uploads the definitions, checks the manifest against the instance's current head, and prints what would be added and removed. It commits nothing.

A deployment is everything the node serves. Anything the head deploys that this directory does not carry shows as removed (`-`) and stops serving on apply. If you deployed the quickstart service on this instance, its workflow and channel appear that way. Then apply it:

```bash
orion-cli --server http://localhost:8080 deployments apply -d dist/high-value-order
```

`apply` prints the same plan, then commits the manifest as the next revision, under compare-and-swap on the head. The answer's `committed_revision` is the new head, and `evaluation.converged` says the node built it. Applying the same directory again does nothing: the CLI reports the head as up to date. [Deploy a compiled directory](../../reference/admin-api/deployments.md#deploy-a-compiled-directory) lists every step it takes.

> [!NOTE]
> Against an instance with admin auth enabled, pass `--api-key` or set `ORION_API_KEY`. `lint`, `dry-run`, `test` and `compile` need no server and no key.

## Verify

Call the channel the deployment added:

```bash
curl -s -X POST http://localhost:8080/api/v2/data/high-value-orders \
  -H 'Content-Type: application/json' \
  --data @examples/packages/high-value-order/request.json
```

The answer's `data.order` carries `"flagged": true` and the alert string, as the dry run did.

Then read the head the instance now serves:

```bash
curl -s http://localhost:8080/api/v2/admin/deployments | jq '{head_revision, applied_revision}'
```

## Roll back

History only moves forward. A rollback commits an earlier revision's manifest as a new revision, so the revision you leave stays as it was:

```bash
curl -s -X POST http://localhost:8080/api/v2/admin/deployments/rollback \
  -H 'Content-Type: application/json' \
  -d '{"idempotency_key": "rollback-1", "expected_head_revision": 2, "target_revision": 1}'
```

`expected_head_revision` must be the current head, and `target_revision` the revision to restore. Pick the target from the history `GET /api/v2/admin/deployments` lists. The example assumes the head is revision 2, such as after a second apply. `orion-cli deployments rollback` takes the same three values as flags.

## Clean up

From the repository root, remove the directory this tutorial wrote:

```bash
rm -ri dist/high-value-order
```

The interactive prompt protects similarly named files. Nothing under `examples/` was modified.

## Recap

- `lint`, `dry-run` and `test` run with no server. A stub file answers connector calls, and a missing stub is a failure, never a pass.
- A case file is a dry run with the answer written down. `expect_errors` defaults to empty, so silent failure cannot pass.
- `compile` writes a definition set as a deployment directory. `plan` commits nothing, `apply` commits the next revision, and the node serves it.
- A rollback is a new revision with an earlier manifest. History is never rewritten.

## Next steps

- [Deployment endpoints](../../reference/admin-api/deployments.md): definitions, connections, plans, apply, rollback and convergence.
- [CI/CD with deployments](../../guides/patterns/ci-cd.md): the same steps as a pipeline.
- [Author a workflow](../../guides/author/workflows.md): the how-to layer, now that you can test what you write.
- [CLI](../../reference/cli/index.md): every flag of `lint`, `dry-run`, `test` and `compile`.
