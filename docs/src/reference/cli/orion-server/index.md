<!-- description: Every orion-server subcommand: validate-config, migrate, lint, compile, fmt, clippy, dry-run, test, test-connectivity, preflight, dump-openapi and package. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# orion-server

`orion-server` is the runtime; run with no subcommand it starts the server. Its subcommands validate configuration, run migrations, check and format definition sets offline, execute workflows with no server, and promote packages. The global `-c, --config <path>` flag applies to every one of them.

| Command | Purpose |
|---|---|
| [Start the server](./start.md) | Run the server itself, with `-c` naming the config file |
| [`validate-config`](./validate-config.md) | Validates the configuration without starting the server, then prints the full effective config with secrets masked |
| [`migrate`](./migrate.md) | Runs database migrations against the configured `storage.url` without starting the server |
| [`lint`](./lint.md) | Statically validates a workflow JSON file with the same checks the admin `POST /workflows` endpoint runs |
| [`compile`](./compile.md) | Compiles a definition set into files the admin API accepts, resolving the authoring conveniences a set may use — `$from` for a shared value, `use` for a task fragment |
| [`fmt`](./fmt.md) | Formats definition files to the house style, the way `cargo fmt` formats Rust |
| [`clippy`](./clippy.md) | Advisory checks beyond `lint`, said only when certain — the `cargo clippy` to `lint`'s `cargo check` |
| [`dry-run`](./dry-run.md) | Executes a workflow against a JSON input in an in-process engine, then prints the per-task execution trace |
| [`test`](./test.md) | Runs a directory of offline workflow test cases |
| [`test-connectivity`](./test-connectivity.md) | Probes the configured database with a no-op query, and Kafka when `kafka.enabled = true` |
| [`preflight`](./preflight.md) | Scans stored channels and workflows for anything the 1.0 rules refuse: configs that no longer parse, tasks the validator rejects, and `data_query`/`data_write` tasks with no `schema` |
| [`dump-openapi`](./dump-openapi.md) | Prints the public HTTP API's OpenAPI 3.1 spec as JSON to stdout |
| [`package`](./package.md) | Exports a package — selected channels, their workflows, and every connector those workflows reference, and promotes it between instances |

## Related

- [`orion-cli` commands](../orion-cli/index.md): the admin client.
- [Shared definitions](../shared-definitions.md): the `$from` and `use` forms the offline commands resolve.
- [Server configuration](../../configuration/index.md): every setting the `-c` file may carry.
