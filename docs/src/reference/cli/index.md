<!-- description: The two Orion binaries: orion-server, the runtime with its diagnostic, authoring and promotion subcommands, and orion-cli, the admin client. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# CLI

Orion ships two binaries. `orion-server` is the runtime, with diagnostic, authoring and promotion subcommands that need no running server. `orion-cli` is the admin client, driving a running server over HTTP. 

| Page | Holds |
|---|---|
| [orion-server](./orion-server/index.md) | Start the server; `validate-config`, `migrate`, `lint`, `compile`, `fmt`, `clippy`, `dry-run`, `test`, `test-connectivity`, `preflight`, `dump-openapi` and `package` |
| [orion-cli](./orion-cli/index.md) | The global flags, and one command per entity kind plus `send`, `traces`, `engine`, `cron`, `dlq`, `audit-logs`, `backups`, `packages`, `metrics`, `functions`, `benchmark` and `completions` |
| [Shared definitions](./shared-definitions.md) | `$from` and `use`, the two forms a definition set may use and `compile` resolves |

## Related

- [Admin API](../admin-api/index.md): the HTTP endpoints `orion-cli` drives.
- [Server configuration](../configuration/index.md): every server setting and its `ORION_*` override.
- [Promote between environments](../../operate/maintain/promotion.md): the promotion model behind `orion-server package`.
