<!-- description: The two Orion binaries: orion-server, the runtime with its diagnostic and authoring subcommands, and orion-cli, the admin client. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# CLI

Orion ships two binaries. `orion-server` is the runtime, with diagnostic and authoring subcommands that need no running server. `orion-cli` is the admin client, driving a running server over HTTP. 

| Page | Holds |
|---|---|
| [orion-server](./orion-server/index.md) | Start the server; `validate-config`, `migrate`, `lint`, `compile`, `fmt`, `clippy`, `dry-run`, `test`, `test-connectivity`, `sql check`, `dump-openapi` and `plugin` |
| [orion-cli](./orion-cli/index.md) | The global flags, and `config`, `health`, `send`, `traces`, `engine`, `functions`, `metrics`, `audit-logs`, `backups`, `cache`, `dlq`, `cron`, `deployments`, `provider`, `connections` and `completions` |
| [Shared definitions](./shared-definitions.md) | `$from` and `use`, the two forms a definition set may use and `compile` resolves |

## Related

- [Admin API](../admin-api/index.md): the HTTP endpoints `orion-cli` drives.
- [Server configuration](../configuration/index.md): every server setting and its `ORION_*` override.
- [Deployment endpoints](../admin-api/deployments.md#deploy-a-compiled-directory): how `compile` and `orion-cli deployments apply -d` deploy a definition set.
