<!-- description: Build a production-shaped Orion orders API with validation, PostgreSQL, offline stubs, deployment, traces, failure checks and safe updates. -->
# Orders API: End-to-End Golden Path

**Page type:** Tutorial · **Audience:** Developers taking Orion beyond the quickstart

**Tested with:** Orion 1.5.1 · **Last reviewed:** 2026-09-04

This tutorial follows one orders service from source files to a safe update. It
validates input, writes to PostgreSQL through a restricted connector, reads the
customer's recent order history, runs without dependencies in CI, handles
invalid input, deploys as a package, and exposes execution traces.

All definitions are complete, copyable files under
[`examples/packages/postgres-orders/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/postgres-orders).
The documentation includes those files rather than maintaining a second copy.

## Before you start

You need Git, Docker with Compose, `curl`, `jq`, `orion-server`, and
`orion-cli`. The commands use a POSIX-compatible shell and assume ports 8080
and 5432 are available.

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion
```

## 1. Inspect the complete service

The package contains:

| File | Purpose |
|---|---|
| `workflow.json` | Parse and validate the request, insert the order, then query customer history |
| `channel.json` | Expose the workflow at `POST /record-order` |
| `connector.json` | Connect to PostgreSQL by environment reference and refuse deletes |
| `docker-compose.yml` and `seed.sql` | Provide the local database and seed customers |
| `request.json` | Supply one repeatable request |
| `tests/valid-order.case.json` | Run the workflow offline with connector stubs |

Read the definitions before running them:

```bash
sed -n '1,240p' examples/packages/postgres-orders/workflow.json
sed -n '1,160p' examples/packages/postgres-orders/connector.json
```

The workflow's `validation` task has `halt_on: "failure"`, so an invalid order
cannot reach the write task. The connector obtains its URL from
`ORDERS_DB_URL` and has `operations.delete: false`.

## 2. Validate and test without a database

Validate the complete definition set:

```bash
orion-server lint examples/packages/postgres-orders
```

Expected result: exit status 0 and no validation errors.

Run the regression case. Its stubs replace both database calls, so this step
does not connect to PostgreSQL:

```bash
orion-server test examples/packages/postgres-orders/tests
```

Expected result: one passing case named “valid order is recorded and history is
returned,” with exit status 0.

Confirm that invalid input stops before either connector call:

```bash
orion-server dry-run \
  -w examples/packages/postgres-orders/workflow.json \
  -i <(printf '%s' '{"customer_id":1,"item":"Invalid","total":-1}') \
  | jq '{errors, calls, steps: [.trace.steps[].task_id]}'
```

Expected result: `errors` contains `total must be positive`, `calls` is empty,
and `steps` contains only `parse` and `validate`. A validation task records a
client error in the result while the dry-run command itself exits 0; use
regression-case expectations when that distinction must gate CI. This check
proves the write is not attempted.

## 3. Start the real dependency

```bash
docker compose -f examples/packages/postgres-orders/docker-compose.yml up -d
```

The Compose project starts seeded PostgreSQL and Orion. Verify both containers
before deploying:

```bash
docker compose -f examples/packages/postgres-orders/docker-compose.yml ps
curl --retry 10 --retry-delay 1 --retry-connrefused \
  http://localhost:8080/healthz
```

Expected result: both services report running and the health check exits 0.

## 4. Deploy and invoke the package

```bash
./examples/deploy.sh postgres-orders
```

The script creates the connector, workflow, and channel in dependency order,
activates the definitions, sends `request.json`, and prints the response. Look
for `rows_affected: 1`, a generated order id, and customer `Ada Lovelace` with
the new order in `orders`.

Re-running the deploy is safe: existing definitions are skipped. Each request
still inserts a new row, so the returned history grows.

## 5. Observe the execution

```bash
orion-cli config set-server http://localhost:8080
orion-cli traces list --channel record-order --limit 1
```

Copy the returned trace id, then inspect it:

```bash
orion-cli traces get <trace-id>
```

Expected result: the trace is `completed` and shows `parse`, `validate`,
`record`, and `history` in order. The trace id is variable; do not compare the
whole response byte for byte.

## 6. Verify live failure handling

```bash
curl -sS -X POST http://localhost:8080/api/v1/data/record-order \
  -H 'Content-Type: application/json' \
  -d '{"data":{"customer_id":1,"item":"Invalid","total":-1}}' | jq
```

Expected result: the response contains a validation error stating `total must
be positive`; no order is inserted. If PostgreSQL is unavailable instead, the
connector task returns a server error and Orion records the failed trace. The
[failure-handling guide](../operate/failure-handling.md) explains timeouts,
retries, and circuit breakers for that path.

## 7. Update safely

Do not edit the active workflow. Create a draft version, change the draft file,
test it, preflight activation, then activate it:

```bash
orion-cli workflows new-version record-order
orion-cli workflows update record-order \
  -f examples/packages/postgres-orders/workflow.json
orion-cli workflows test record-order \
  -f examples/packages/postgres-orders/request.json --trace
orion-cli workflows activate record-order --dry-run
orion-cli workflows activate record-order
```

Expected result: the test prints a successful task trace, preflight reports a
valid transition without writing, and activation hot-reloads the new version.
For a real change, edit a copy on a branch and rerun the offline regression
suite before updating the instance.

## Clean up

```bash
docker compose -f examples/packages/postgres-orders/docker-compose.yml down -v
```

This removes the tutorial's containers and database volume, including inserted
orders. The command does not remove your repository checkout.

## Next steps

- [Portable Data Dialect](../reference/data-dialect.md) — exact query and write
  envelopes used by the workflow.
- [CI/CD with Packages](./ci-cd.md) — compile, plan, and apply the same
  definitions between environments.
- [Production Checklist](../operate/production-checklist.md) — replace local
  defaults before accepting external traffic.
