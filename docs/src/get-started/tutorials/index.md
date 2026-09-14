<!-- description: Five tutorials that follow the quickstart: build a service by hand, add a PostgreSQL connector, complete an orders API, test and promote it, run the examples. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Tutorials

Each tutorial builds something bounded and ends with a runnable state. They assume you completed the [quickstart](../quickstart.md) and have a server on `http://localhost:8080`. Read them in order; each one may assume the one before it and never the one after.

- [Build your first service](./first-service.md) makes the quickstart's four administration calls one at a time, shows the CLI form of each, and explains what every response means.
- [Add your first connector](./first-connector.md) connects Orion to PostgreSQL and builds a service that writes an order and reads the customer's history back, without storing a password.
- [Build an orders API end to end](./orders-api.md) takes one service from source files to a safe update: validation, a restricted connector, offline tests, deployment, traces and failure handling.
- [Test and promote a service](./test-and-promote.md) proves a workflow offline with lint, dry run and regression cases, then ships it to a second instance as one versioned package.
- [Run the example packages](./examples.md) deploys the repository's ready-made services, from a threshold check to Kafka ingress and a model-backed tournament.
