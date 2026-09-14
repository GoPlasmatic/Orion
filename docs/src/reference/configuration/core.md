<!-- description: The core orion-server settings: how settings resolve, the deployment environment, vars and secrets, the HTTP server, storage and cluster mode. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Core settings

Resolution, the deployment environment, vars and secrets, the HTTP server, storage and cluster mode. Every table on these pages carries the wire name, the default the code uses, and the `ORION_*` override.

| Page | Holds |
|---|---|
| [How settings are resolved](./how-settings-are-resolved.md) | struct defaults, the config file with ${VAR} and env:// references, then ORION_SECTION__KEY variables. |
| [Deployment environment](./environment.md) | any value starting with prod turns three warnings into startup errors, and ORION_ENVIRONMENT is the override. |
| [Vars and secrets](./vars-and-secrets.md) | the per-environment values a workflow reads by name, which is recorded in traces, and which value shapes each refuses. |
| [Server settings](./server.md) | bind address and port, shutdown timeouts, the admin body limit, data mounts, verbose errors, TLS, compression and the API docs. |
| [Storage settings](./storage.md) | the storage.url that selects SQLite, PostgreSQL or MySQL, pool sizing, encryption at rest, backups and auto_migrate. |
| [Cluster settings](./cluster.md) | enabling multi-replica mode, the shared Redis, the epoch poll interval and the per-replica instance_id, with their defaults. |

## Related

- [Server configuration](./index.md): every section, by what you are configuring.
- [How settings are resolved](./how-settings-are-resolved.md): defaults, the file, and the environment.
- [Production checklist](../../operate/production-checklist.md): which settings to change before real traffic.
