<!-- description: Find Orion's exact API, CLI, schema, function, connector, configuration, metric and error contracts from one task-oriented index. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Reference

Reference pages state the wire format, the fields, the defaults and the constraints, and nothing else. Every page follows one skeleton, described in [Reference conventions](./conventions.md). Every fact a page restates from the product is asserted against the product by a test.

For a guided task, start from [Guides](../guides/index.md); for the idea behind a contract, start from [Concepts](../concepts/index.md).

| I need to… | Reference |
|---|---|
| Manage workflows, channels, connectors, plugins or models over HTTP | [Admin API](./admin-api/index.md) and [OpenAPI specification](./openapi.md) |
| Invoke a channel or poll an asynchronous result | [Data API](./data-api.md) |
| Write workflow JSON | [Workflow definition](./workflows.md) |
| Choose or configure a task function | [Task functions](./functions/index.md) |
| Write a JSONLogic condition or mapping | [Expression language](./expressions.md) |
| Configure authentication, limits, caching or response behaviour on a channel | [Channel configuration](./channel-config/index.md) |
| Configure an external system | [Connector types](./connectors/index.md) |
| Query SQL, MongoDB or Elasticsearch portably | [Portable data dialect](./data-dialect.md) |
| Package a WebAssembly task function | [Plugin manifest and ABI](./plugin-manifest.md) |
| Configure an Orion server | [Server configuration](./configuration/index.md) and [Environment variables](./environment-variables.md) |
| Look up a CLI command | [CLI](./cli/index.md) |
| Format definition files, or read an advisory rule | [Definition style (`fmt`)](./fmt.md) and [Advisory checks (`clippy`)](./clippy/index.md) |
| Build a dashboard or an alert | [Metrics](./metrics.md) |
| Interpret a response or a failure | [Errors and response envelopes](./errors.md) |
