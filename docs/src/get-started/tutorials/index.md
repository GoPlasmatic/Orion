<!-- description: Five tutorials that follow the quickstart: build a service by hand, add a PostgreSQL connector, complete an orders API, test and deploy it, run the examples. -->
<!-- type: hub -->
<!-- last_verified: 2026-10-25 -->

# Tutorials

Each tutorial builds something bounded and ends with a runnable state. They assume you completed the [quickstart](../quickstart.md) and have a server on `http://localhost:8080`. Read them in order; each one may assume the one before it and never the one after.

- [Build your first service](./first-service.md) writes a service as two files, compiles them into a deployment, applies it with the CLI, and explains every response.
- [Add your first connector](./first-connector.md) adds a PostgreSQL connector file to the set and builds a service that writes an order and reads the customer's history back, with no password in the deployment.
- [Build an orders API end to end](./orders-api.md) takes one service from source files to a safe update: validation, a restricted connector, offline tests, deployment, traces and failure handling.
- [Test and deploy a service](./test-and-promote.md) proves a workflow offline with lint, dry run and regression cases, then compiles it into a deployment and applies it.
- [Run the example packages](./examples.md) deploys the repository's ready-made services, from a threshold check to a plugin-backed codec and a nightly schedule.
