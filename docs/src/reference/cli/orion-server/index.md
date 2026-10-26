<!-- description: Every orion-server subcommand: start the server, validate-config, migrate, test-connectivity and dump-openapi. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-19 -->

# orion-server

`orion-server` runs the node and checks its configuration and state database. Use [`orion-cli`](../orion-cli/index.md) to author definitions and deploy them.

| Command | Purpose |
|---|---|
| [Start the server](./start.md) | Run the server itself, with `-c` naming the config file |
| [`validate-config`](./validate-config.md) | Validates the configuration without starting the server, then prints the full effective config with secrets masked |
| [`migrate`](./migrate.md) | Runs database migrations against the configured `storage.url` without starting the server |
| [`test-connectivity`](./test-connectivity.md) | Probes the configured database with a no-op query |
| [`dump-openapi`](./dump-openapi.md) | Prints the public HTTP API's OpenAPI 3.1 spec as JSON to stdout |

## Related

- [`orion-cli` commands](../orion-cli/index.md): the admin client.
- [Shared definitions](../shared-definitions.md): the `$from` and `use` forms the offline commands resolve.
- [Server configuration](../../configuration/index.md): every setting the `-c` file may carry.
