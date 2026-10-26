<!-- description: The two Orion binaries: orion-server runs and diagnoses the node; orion-cli authors definitions offline and administers a running server. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# CLI

Orion ships two binaries. `orion-server` runs the node and diagnoses its configuration and state database. `orion-cli` authors definitions offline and administers a running server over HTTP.

| Page | Holds |
|---|---|
| [orion-server](./orion-server/index.md) | Start the server; `validate-config`, `migrate`, `test-connectivity` and `dump-openapi` |
| [orion-cli](./orion-cli/index.md) | Offline authoring: `lint`, `compile`, `fmt`, `clippy`, `dry-run`, `test`, `sql check` and `plugin`. Remote administration: `health`, `send`, `traces`, `engine`, `functions`, `metrics`, `audit-logs`, `backups`, `cache`, `dlq`, `cron`, `deployments`, `provider` and `connections`. CLI setup: `config` and `completions` |
| [Shared definitions](./shared-definitions.md) | `$from` and `use`, the two forms a definition set may use and `compile` resolves |

## Related

- [Admin API](../admin-api/index.md): the HTTP endpoints `orion-cli` drives.
- [Server configuration](../configuration/index.md): every server setting and its `ORION_*` override.
- [Deployment endpoints](../admin-api/deployments.md#deploy-a-compiled-directory): how `compile` and `orion-cli deployments apply -d` deploy a definition set.
