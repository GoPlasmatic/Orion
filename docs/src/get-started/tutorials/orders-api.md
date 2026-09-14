<!-- description: Build a production-shaped Orion orders API with validation, PostgreSQL, offline stubs, deployment, traces, failure checks and safe updates. -->
<!-- type: tutorial -->
<!-- last_verified: 2026-09-14 -->

# Build an orders API end to end

This tutorial follows one orders service from source files to a safe update. It validates input, writes to PostgreSQL through a restricted connector, and reads the customer's recent history. It runs without dependencies in CI, handles invalid input, deploys as a package, and exposes its traces.

## What you will learn

- How `halt_on: "failure"` keeps invalid input away from a write.
- How to prove a connector-backed workflow offline, with the database stubbed.
- What a trace shows after a live request.
- How to update an active workflow without editing it.

## Before you start

Tested with Orion 1.8.0. You need:

- Git, and Docker with Compose
- `curl`, `jq`, `orion-server` and `orion-cli`
- ports `8080` and `5432` free
- a POSIX shell (on Windows, WSL)

Every definition is a complete, copyable file under [`examples/packages/postgres-orders/`](https://github.com/GoPlasmatic/Orion/tree/main/examples/packages/postgres-orders). This page includes those files rather than keeping a second copy. Clone the repository first:

```bash
git clone https://github.com/GoPlasmatic/Orion.git
cd Orion
```

## 1. Read the complete service

The package contains:

| File | Purpose |
|---|---|
| `workflow.json` | Parse and validate the request, insert the order, then query customer history |
| `channel.json` | Expose the workflow at `POST /record-order` |
| `connector.json` | Connect to PostgreSQL by environment reference and refuse deletes |
| `docker-compose.yml` and `seed.sql` | Provide the local database and seed customers |
| `request.json` | Supply one repeatable request |
| `tests/valid-order.case.json` | Run the workflow offline with connector stubs |

Read the two definitions that carry the decisions:

```bash
cat examples/packages/postgres-orders/workflow.json
cat examples/packages/postgres-orders/connector.json
```

The workflow's `validation` task has `halt_on: "failure"`, so an invalid order cannot reach the write task. The connector takes its URL from `ORDERS_DB_URL` and sets `operations.delete: false`.

## 2. Validate and test without a database

Validate the whole definition set, including the references between its files:

```bash
orion-server lint examples/packages/postgres-orders
```

It exits `0` with no findings. Then run the regression case. Its stubs replace both database calls, so this step never connects to PostgreSQL:

```bash
orion-server test examples/packages/postgres-orders/tests
```

Output:

```text
  ok    valid order is recorded and history is returned

1 passed, 0 failed (1 case(s))
```

Confirm that invalid input stops before either connector call:

```bash
orion-server dry-run \
  -w examples/packages/postgres-orders/workflow.json \
  -i <(printf '%s' '{"customer_id":1,"item":"Invalid","total":-1}') \
  | jq '{errors, calls, steps: [.trace.steps[].task_id]}'
```

`errors` contains `total must be positive`, `calls` is empty, and `steps` lists only `parse` and `validate`. The write was never attempted. A validation failure is a client error recorded in the result, so `dry-run` itself exits `0`. Use a regression case when that distinction has to gate CI.

## 3. Start the real dependency

Bring up seeded PostgreSQL and an Orion container together:

```bash
docker compose -f examples/packages/postgres-orders/docker-compose.yml up -d
```

Verify both before deploying:

```bash
docker compose -f examples/packages/postgres-orders/docker-compose.yml ps
curl --retry 10 --retry-delay 1 --retry-all-errors -fsS \
  http://localhost:8080/healthz
```

Both services report `running`, and the health check prints `{"status":"ok"}`.

## 4. Deploy and invoke the package

The deploy script creates the connector, workflow and channel in dependency order, activates them, sends `request.json` and prints the response:

```bash
./examples/deploy.sh postgres-orders
```

Look for `rows_affected: 1`, a generated order id, and customer `Ada Lovelace` with the new order in `orders`. Re-running the script is safe: existing definitions are skipped. Each request still inserts a new row, so the returned history grows.

## 5. Observe the execution

Point the CLI at the server and list the newest trace on the channel:

```bash
orion-cli config set-server http://localhost:8080
orion-cli traces list --channel record-order --limit 1
```

Copy the trace id it prints, then inspect it:

```bash
orion-cli traces get <trace-id>
```

The trace is `completed` and shows `parse`, `validate`, `record` and `history` in order. The id is different on every run.

## 6. Verify live failure handling

Send an invalid order to the live endpoint:

```bash
curl -sS -X POST http://localhost:8080/api/v1/data/record-order \
  -H 'Content-Type: application/json' \
  -d '{"data":{"customer_id":1,"item":"Invalid","total":-1}}' | jq
```

The response carries a validation error stating `total must be positive`, and no order is inserted. If PostgreSQL were unavailable instead, the connector task would answer a server error and Orion would record the failed trace. [Timeouts, retries and circuit breakers](../../operate/run/failure-handling.md) covers that path.

## 7. Update safely

Never edit the active workflow. Create a draft version, change it, test it, preflight the activation, then activate:

```bash
orion-cli workflows new-version record-order
orion-cli workflows update record-order \
  -f examples/packages/postgres-orders/workflow.json
orion-cli workflows test record-order \
  -f examples/packages/postgres-orders/request.json --trace
orion-cli workflows activate record-order --dry-run
orion-cli workflows activate record-order
```

The test prints a successful task trace, the preflight reports a valid transition without writing anything, and the activation hot-reloads the new version. For a real change, edit a copy on a branch and rerun the offline suite before touching the instance.

## Clean up

Remove the containers and the database volume, including every inserted order:

```bash
docker compose -f examples/packages/postgres-orders/docker-compose.yml down -v
```

Your repository checkout is untouched.

## Recap

- A `validation` task with `halt_on: "failure"` is the barrier between input and a write.
- `lint`, `test` and `dry-run` prove a connector-backed workflow with no database, because stubs answer the connector calls.
- Every live request leaves a trace, and the trace names each task that ran.
- An active workflow is immutable. Change ships as a new version, tested and preflighted before it serves.

## Next steps

- [Portable data dialect](../../reference/data-dialect.md): the exact query and write envelopes the workflow uses.
- [CI/CD with packages](../../guides/patterns/ci-cd.md): compile, plan and apply these definitions between environments.
- [Production checklist](../../operate/production-checklist.md): replace the local defaults before accepting external traffic.
