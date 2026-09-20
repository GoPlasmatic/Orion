<!-- description: The [packages] settings: package artifacts a node applies to itself at startup, the signatures directory, and the time budget for applying them. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# Package settings

Package artifacts this node applies to itself when it starts, before `/readyz` reports it ready. Each entry is a compiled artifact, the file [`orion-server compile`](../cli/orion-server/compile.md) or `package export` writes.

## Synopsis

```toml
[packages]
apply = ["/pkg/orders/orders.json"]
signatures_dir = "/run/plugin-signatures"
apply_timeout_secs = 1800
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `packages.apply` | `[]` | `ORION_PACKAGES__APPLY` | The artifacts an image carries, applied in order. The variable is comma-separated. A later entry may `require` what an earlier one carries. |
| `packages.signatures_dir` | — | `ORION_PACKAGES__SIGNATURES_DIR` | A mounted directory of `.sig` files, when the target trusts only signed plugins or models. They are attached as [`package apply --signatures`](../cli/orion-server/package.md#signatures-at-deploy-time) attaches them. |
| `packages.apply_timeout_secs` | `1800` | `ORION_PACKAGES__APPLY_TIMEOUT_SECS` | The whole startup apply's budget, model admission included. Past it the node exits. `0` means no limit. |

## What happens at startup

The node applies the artifacts once it has published its first generation and started its background tasks. The listener is already up. `/healthz` answers throughout, while `/readyz` answers `503` with `components.packages: "applying"`.

Every artifact is checked before the first is applied. Each must exist, parse, match its `content_hash` and lint clean. Each then goes through the sequence [`orion-server package apply`](../cli/orion-server/package.md) runs, through the node's own admin routes. It writes the same receipts and audit rows, with principal `system:boot-packages`. `/readyz` turns ready once every package is applied and nothing it carries is quarantined.

- **A restart is a no-op.** An artifact whose version is the package's current one writes nothing. The node still checks that it serves.
- **A superseded version is left alone.** When a later version of the package is current, the node does not roll it back. It logs a warning and serves what the newer version left of the older one. A rollback is a deliberate `package apply`.
- **A failure stops the process.** An artifact that does not check, a refused import, or a member the reload quarantines fails the boot. The node logs the reason, stops serving without the drain grace, and exits non-zero, for the orchestrator to restart.

In a cluster every node may list the same artifact. One node at a time applies a given package, holding a lease named `package-apply:<name>`. The others wait until its receipt says applied. Each then reloads its own generation and checks that it serves. A node that dies mid-apply leaves the lease to expire within a minute, and a peer takes over.

`validate-config` runs the same file checks and names the entry that fails:

```console
$ orion-server validate-config
error: packages.apply[0] '/pkg/orders/orders.json': read '/pkg/orders/orders.json': No such file or directory (os error 2)
Error: 1 problem(s) with the [packages] artifacts
```

## Related

- [Promote between environments](../../operate/maintain/promotion.md): the apply sequence and the receipts it writes.
- [Monitoring](../../operate/run/monitoring.md): `components.packages` on `/readyz` and `/health`.
- [Run on Kubernetes](../../operate/deploy/kubernetes.md): sizing the startup probe for a long apply.
- [Server configuration](./index.md): every section, by what you are configuring.
