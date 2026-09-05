<!-- description: Upgrading Orion 1.5.x to 1.6.0 — what changes behaviour: plugins are off by default, three new migrations, a larger binary, and an MSRV that tracks Wasmtime. -->
# Upgrading to 1.6.0

This page is for operators upgrading an existing Orion deployment from
**1.5.x** to **1.6.0**. It covers only what *changes behaviour*. The new
capability — [plugins](../concepts/plugins.md), custom task functions in a
WebAssembly sandbox, with the SDK, the package member, the offline tooling
and the trust option that come with it — is in the
[CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/CHANGELOG.md).

**1.6.0 is a minor release and behaves like one.** No config key was renamed
or removed, no API path moved, no metric was renamed, and nothing an existing
definition does changes. Four things can reach you, and none of them is a
break: the release ships **migrations**, the binary is **larger**, the
plugin sandbox is **off** until you turn it on, and the **MSRV rule** changed
without the number changing.

The version-independent procedure — back up, preflight, validate config,
migrate, roll — is on [Upgrades](./upgrades.md).

---

## Before you start

| # | Check | Applies to you if |
|---|-------|-------------------|
| 1 | [Run the migrations as a deploy step](#1-three-migrations-expand-only) | You run a cluster with `auto_migrate = false` — everyone else gets them at startup |
| 2 | [Nothing — plugins are off](#2-plugins-are-off-by-default) | Every deployment; read it to know what turning them on will mean |
| 3 | [Allow for a ~6 MB larger binary and image](#3-the-binary-carries-wasmtime) | You pin image sizes, or build from source on a constrained host |
| 4 | [Note the MSRV policy](#4-the-msrv-now-tracks-wasmtimes) | You build from source |

`orion-server preflight` covers this release only in that it now reads the
active plugins: a stored workflow calling a plugin function is checked
against the plugin's manifest rather than reported as naming an unknown
function. A clean run on 1.5.x is a clean run on 1.6.0.

---

## 1. Three migrations, expand-only

**What changed.** Each backend gains migrations named `plugins` (two new
tables, `plugins` and `plugin_artifacts`) and `plugin_signatures` (one
nullable column on `plugins`). They add schema and touch nothing that exists,
so a 1.5.x binary keeps working against a migrated database and a rollback
needs no schema work.

**What to do.** Nothing, unless `storage.auto_migrate = false`: then
`orion-server migrate` is the deploy step, as it always is in
[cluster mode](./cluster.md). On MySQL, the artifact table's `bytes` column is
a `LONGBLOB`; if you later enable plugins, make sure the server's
`max_allowed_packet` exceeds `plugins.max_component_bytes` (16 MiB by
default) or an upload will fail at write.

```bash
orion-server migrate --dry-run -c config.toml   # names the three by backend
```

## 2. Plugins are off by default

**What changed.** The plugin sandbox exists in every binary, and
`plugins.enabled` defaults to `false`. With it off, no Wasmtime engine is
constructed, no epoch ticker runs, `POST /api/v1/admin/plugins` answers `400`,
and a plugin row that reaches this node's database — through a cluster
peer's activation, or an import — becomes a `disabled` load issue that
quarantines the workflows naming its functions, never an abort.

**What to do.** Nothing. When you turn it on, read the
[production checklist row](./production-checklist.md): the pooling allocator
reserves `max_live_instances × max_memory_bytes` of virtual address space at
startup (16 GiB by default), and `[plugins.trust]` is where signing keys go.

## 3. The binary carries Wasmtime

**What changed.** Wasmtime and Cranelift are compiled into every target,
adding roughly 6 MB to the release binary and to the container images. They
are inert until `plugins.enabled = true`.

**What to do.** Nothing, unless a size budget is pinned somewhere. `cargo
deny check` now allows `Apache-2.0 WITH LLVM-exception`, which Wasmtime and
Cranelift carry.

## 4. The MSRV now tracks Wasmtime's

**What changed.** The minimum supported Rust version is still **1.98**, but
the rule behind it changed: it now also follows
[Wasmtime's policy](https://docs.wasmtime.dev/stability-release.html) (stable
minus two), so a Wasmtime upgrade in a future minor may move it.
[Support & Compatibility](../reference/support.md#rust-toolchain-msrv) states
the policy.

**What to do.** Nothing for the released binaries and images. Building from
source, keep the toolchain at the `rust-version` the checked-out release
declares.

---

## Related

- [Plugins](../concepts/plugins.md), [Build a Plugin](../build/plugins.md) and
  the [Plugins reference](../reference/plugins.md): the capability this release
  adds.
- [Upgrades](./upgrades.md): the version-independent procedure.
