<!-- description: What changed in each Orion release, what breaks, and how to move: every 1.x version with a Breaking flag, the support policy and the upgrade guides. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Releases

Every 1.x release, what it added, and whether it breaks anything you have. A release with a guide is one that changes behaviour you may be relying on.

| Version | Date | Breaking | What it added |
|---|---|:---:|---|
| [1.8.0](./upgrade-to-1.8.md) | 2026-09-13 | Yes | ONNX models as a governed entity and the fifth package member, the JSONLogic tensor operators, and `engine.ops_budget`. |
| 1.7.0 | 2026-09-06 | No | PostgreSQL binds parameters to the declared type, and `oauth2_login` takes per-environment values. |
| [1.6.0](./upgrade-to-1.6.md) | 2026-09-05 | Yes | The WebAssembly plugin sandbox (off by default) and cron channels (the scheduler is on). |
| 1.5.1 | 2026-09-01 | No | Fixes on top of 1.5.0. |
| 1.5.0 | 2026-09-01 | No | Multiple response cookies, `var://` in stored config, JSONLogic in every `http_call` parameter, and secrets in channel guards. |
| 1.4.0 | 2026-08-29 | No | Address-checked JWKS fetches, a failed-attempt budget on channel `auth.keys`, and serialised engine reloads. |
| 1.3.1 | 2026-08-27 | No | The CodeQL triage fixes. |
| 1.3.0 | 2026-08-27 | No | `[vars]` and `[secrets]`, the `secret` JSONLogic operator, and offline stand-ins for both. |
| [1.2.0](./upgrade-to-1.2.md) | 2026-08-26 | Yes | Task groups and terminal steps, shared definition sources, and the MCP server's removal. |
| [1.1.0](./upgrade-to-1.1.md) | 2026-08-21 | Yes | JWT channel auth, the `smtp` and `storage` connectors, and managed OAuth2. |
| [1.0.0](./upgrade-to-1.0/index.md) | 2026-08-14 | Yes | The frozen 1.x surfaces: the data plane, the admin API, the config keys and the metric names. |

The [CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/CHANGELOG.md) is the full record of what was added, as opposed to what broke.

| Page | Holds |
|---|---|
| [Versioning and support policy](./versioning-policy.md) | which versions receive fixes, what an upgrade guarantees, and the tested toolchains, databases and platforms. |
| [Upgrades](../operate/maintain/upgrades.md) | the version-independent procedure: back up, preflight, validate config, migrate, roll. |

## Related

- [Upgrades](../operate/maintain/upgrades.md): the order that works, whatever version you are on.
- [`orion-server preflight`](../reference/cli/orion-server/preflight.md): the read-only scan that finds stored breaks before a rollout.
- [Back up and restore](../operate/maintain/backup-restore.md): the step every upgrade starts with.
- [The entity lifecycle](../concepts/lifecycle.md): why a stored channel survives a binary swap.
