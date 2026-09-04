<!-- description: Run Orion locally, deploy a tested order-processing workflow and channel, and call your first live API endpoint in one short path. -->
# Quickstart: Your First Live API

**Page type:** Tutorial · **Audience:** Developers evaluating Orion

**Tested with:** Orion 1.5.1 · **Last reviewed:** 2026-09-04

This is the shortest path from an empty machine to a working Orion service. You
will start Orion, inspect and run a tested setup script, then call the endpoint
yourself. The service flags orders whose total exceeds $10,000.

## Before you start

You need Docker, `curl`, and a POSIX-compatible shell. The commands use port
8080 and create a container named `orion-quickstart`.

## 1. Start Orion

```bash
docker run --name orion-quickstart -d -p 8080:8080 \
  ghcr.io/goplasmatic/orion:latest
```

Wait until the server is ready:

```bash
curl --retry 10 --retry-delay 1 --retry-connrefused \
  http://localhost:8080/healthz
```

A successful check exits with status 0.

## 2. Inspect the service definition

Download the repository's repeatable quickstart script before running it:

```bash
curl -fsSLo /tmp/orion-quickstart.sh \
  https://raw.githubusercontent.com/GoPlasmatic/Orion/main/examples/quickstart.sh
less /tmp/orion-quickstart.sh
```

The script contains four administration calls: create and activate one
workflow, then create and activate its channel. It finishes by sending a test
request. It is safe to run again; definitions that already exist are left in
place.

## 3. Deploy and call it

```bash
bash /tmp/orion-quickstart.sh
```

The output ends with an order containing `"flagged": true` and an alert. Send a
second request yourself:

```bash
curl -fsS -X POST http://localhost:8080/api/v1/data/orders \
  -H 'Content-Type: application/json' \
  -d '{ "data": { "order_id": "ORD-0001", "total": 12500 } }'
```

You now have a live API. The **workflow** contains the business logic; the
**channel** exposes it at `POST /orders`. Orion supplies the routing, lifecycle,
validation, tracing, and other runtime capabilities around those definitions.

## If it does not work

- “No Orion instance” means the container is not ready or port 8080 is already
  in use. Run `docker logs orion-quickstart`.
- “Already in use” from Docker means the named container exists. Start it with
  `docker start orion-quickstart`, or use your existing Orion instance.
- An HTTP `409` usually means definitions with the quickstart identifiers
  already exist in a different state. Follow [Troubleshooting](../operate/troubleshooting.md)
  or use a clean local instance.

## Clean up

```bash
docker stop orion-quickstart
docker rm orion-quickstart
```

The container uses its internal SQLite database without a mounted volume, so
removing it also removes the definitions created in this tutorial.

## Next steps

- [Understand the HTTP flow](./first-service.md): make each administration
  call by hand and inspect the definitions.
- [Choose a use case](./use-cases.md): follow a path for REST, webhooks, Kafka,
  databases, or AI-assisted authoring.
- [How Orion Works](../concepts/how-orion-works.md): build the complete mental
  model in one page.
