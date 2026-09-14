<!-- description: Author an Orion service in the order it is written: the workflow, the channel, the connectors, the offline tests, and the versioned rollout. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Author a service

These five guides are the sequence a service is written in, and each may assume the one before it. For a complete worked example first, follow [Build an orders API end to end](../../get-started/tutorials/orders-api.md). Then come back here for the task you are on.

- [Author a workflow](./workflows.md) writes the logic: reaching request data, branching, grouping tasks and stopping early, calling out of process, and deciding what a failure does.
- [Configure a channel](./channels.md) exposes it: the route, sync or async, and only the guards the endpoint needs.
- [Connect a database or API](./connectors.md) reaches outside: a connector with its credentials by reference, its operation gates, and the two data APIs to choose between.
- [Test a workflow offline](./testing.md) proves it with no server: format, lint, dry-run with stubs, and a case-file regression suite for CI.
- [Version and roll out changes](./versioning.md) ships a change: cut a version, preflight the activation, roll it out to a share of traffic, and roll back.
