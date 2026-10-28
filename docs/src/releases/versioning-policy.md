<!-- description: What supported means for Orion 2.x: which versions receive fixes, what an upgrade guarantees, and which toolchains, databases and platforms are tested. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Versioning and support policy

What "supported" means for Orion 2.x. Which versions receive fixes, what an upgrade guarantees, and which toolchains, databases and platforms each release is tested against.

## Supported versions

**The latest release is the supported release.** Fixes land on `main` and ship as a patch release of the current minor version. That includes security fixes. Older releases are not back-patched.

This is a deliberate consequence of how Orion ships: a single binary with embedded, per-backend database migrations and a documented upgrade path. There is no supported configuration that requires staying on an old release. The answer to a bug in an old release is always the newest one.

| Version            | Status                                     |
|--------------------|--------------------------------------------|
| Latest 2.x release | Supported — receives all fixes             |
| Older 2.x releases | Upgrade to the latest release for fixes    |
| 1.x releases       | Not back-patched — [upgrade to 2.x](./upgrade-to-2.0.md); there is no in-place path, so plan the move |
| 0.x releases       | End of life                                |

Security vulnerabilities should be reported privately. See the [security policy](https://github.com/GoPlasmatic/Orion/blob/main/SECURITY.md).

## Versioning

Orion follows [semantic versioning](https://semver.org):

- **Patch** (`2.0.x`) — bug and security fixes. No config, API, or database
  schema changes beyond what the fix requires; always safe to roll forward.
- **Minor** (`2.x.0`) — new capabilities, new configuration keys (with
  defaults that preserve existing behaviour), additive database migrations,
  and possibly an MSRV bump (see [Rust toolchain](#rust-toolchain-and-the-msrv)).
- **Major** (`3.0.0`) — reserved for breaking changes to the Admin/Data APIs,
  configuration semantics, or workflow/channel definitions. A major release
  may start the state database over, as 2.0.0 did; there is no promise of an
  in-place upgrade between majors.

The versioned API prefix (`/api/v2/`) names the contract generation, and the crate version tracks it. `/api/v2/` endpoints keep their request and response contracts for the life of the 2.x line. Endpoint additions and new optional fields are not considered breaking.

The v2 admin contract also reports an `api_version`, and `orion-cli` refuses a server reporting another. It moves whenever a change could make the previous client misread an answer or send a refused request, which can be inside a 2.x minor. Upgrade the server and the CLI together.

### What the 2.x promise covers

| Surface | Covered? |
|---|---|
| HTTP Admin and Data APIs under `/api/v2/` | **Yes** — the contract above |
| Configuration keys and their semantics | **Yes** — a minor may add keys with behaviour-preserving defaults, never repurpose one |
| Workflow, channel and connection definitions | **Yes** — a document that validates on 2.0 validates for the 2.x line |
| Prometheus metric names and labels | **Yes** — renames wait for a major |
| The `orion-api` / `orion-client` **Rust** APIs | **No** — see [What an upgrade guarantees](#upgrade-guarantees) |
| The **database schema** | **No** — internal |

**The client crates share the server's version.** `orion-api` and
`orion-client` are published so `orion-cli` and third-party tools can share the server's exact wire types. Every crate in the workspace carries the same version number — `orion-api` 2.1.0 is the contract `orion-server` 2.1.0 serializes. Their *Rust* surface is still outside the support contract. A version bump is not a promise about it: pin an exact version if you build against these crates directly. What *is* promised, whichever crate version you read it with, is the **wire format** they describe — that is the HTTP contract above.

Releases up to and including 1.3.1 predate this rule. The two crates were versioned independently until then, and `orion-api` 1.0.4 / `orion-client` 1.0.5 are the last published under the old scheme. An existing pin on one of those keeps resolving; it no longer matches its server's number.

**The database schema is internal.** Tables, views, triggers and column
spellings may change in any minor through a migration. Read Orion's data through the HTTP API. If you query the tables directly, for dashboards, ETL or reporting, pin what you read to a specific server version. Re-check it on every upgrade: a 2.x minor may rename a column.

## Deprecations

**2.0.0 accepts no 1.x spellings.** Where 2.0 renamed or removed a key, the old name is refused rather than silently accepted. Nothing is scheduled for removal in a 2.x minor. Deprecations introduced *after* 2.0 are announced in a minor release with the old spelling still working, and removed no earlier than the next major.

How a refused name fails differs by surface. The config file and environment give a startup error. A definition is refused when it is uploaded and when a node loads it. [Upgrade to 2.0.0](./upgrade-to-2.0.md) lists what 2.0 refused.

## Upgrade guarantees

- Each release that changes behaviour documents its upgrade path from the
  previous one ([Upgrading to 2.0.0](./upgrade-to-2.0.md)). Upgrades are
  supported **release to release**; when skipping releases, read each
  intermediate upgrade page. Orion 2.0.0 starts the state database over, so
  nothing upgrades a 1.x database in place.
- Database migrations are embedded in the binary for all three backends and
  run at boot (`storage.auto_migrate`, default `true`) or explicitly through
  `orion-server migrate`. Shipped migrations are frozen — a released
  migration file is never edited, only appended to.
- **Take a database backup before upgrading.** Migrations are applied
  forward-only; rolling back to an older Orion after a migration has run is
  not supported.

## Rust toolchain and the MSRV

The minimum supported Rust version is **1.98**, declared as `rust-version` in `Cargo.toml` and enforced by a dedicated CI job. An MSRV bump is a **minor** release at most and is called out in the changelog. This matters only when building from source — the released binaries and images are self-contained.

Since 1.6.0 the MSRV also tracks [Wasmtime's policy](https://docs.wasmtime.dev/stability-release.html) (stable minus two), because the [plugin sandbox](../reference/plugin-manifest.md) links it. The number is unchanged by that, because Wasmtime's floor is currently below Orion's. A Wasmtime upgrade in a future minor can move it even when nothing in Orion's own code needs a newer language feature. Wasmtime and Cranelift add roughly 6 MB to a release binary. They are compiled in on every target, and off by default at runtime (`plugins.enabled = false` constructs no engine).

## Database backends

The storage backend is selected at runtime from the `storage.url` scheme. All three are covered by CI. The full suite runs on SQLite, and dedicated jobs run the storage and cluster suites against real PostgreSQL and MySQL servers.

| Backend    | Notes |
|------------|-------|
| SQLite     | Default; embedded, zero-configuration. The reference backend. Not usable in [cluster mode](../operate/deploy/cluster.md). |
| PostgreSQL | Supports cluster mode. Project deployment artifacts and examples use PostgreSQL 16. |
| MySQL      | MySQL 8+; supports cluster mode. |

## Platforms

- **Docker images**: published to `ghcr.io/goplasmatic/orion` for
  `linux/amd64` and `linux/arm64` (Debian trixie-slim base).
- **Prebuilt binaries**: GitHub release artifacts (shell/PowerShell
  installers and Homebrew) for `aarch64-apple-darwin`,
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and
  `x86_64-pc-windows-msvc`. The rationale for what is and is not on that
  list lives on the [Deploy with Docker](../operate/deploy/docker.md) page.
- **From source**: use the Rust version declared by the checked-out release's
  `Cargo.toml` (`rust-version`; 1.98 in the release documented here). Linux and
  macOS are exercised routinely, and CI runs on Linux.

## What support is (and is not)

Orion is open source under the Apache-2.0 license. "Supported" here describes where fixes ship, not a service-level commitment. Issues and vulnerability reports are triaged on a best-effort basis by the maintainers, with security reports prioritized. There is no commercial support offering at this time.

## Related

- [Upgrades](../operate/maintain/upgrades.md): the standing procedure, and how a
  renamed key fails on each surface.
- [Upgrading to 2.0.0](./upgrade-to-2.0.md): the per-version guide
  for the current release.
- [Configuration Reference](../reference/configuration/index.md): the settings this policy
  freezes.
