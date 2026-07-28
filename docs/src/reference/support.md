# Support & Compatibility

This page states what "supported" means for Orion 1.x: which versions receive
fixes, what an upgrade guarantees, and which toolchains, databases, and
platforms each release is tested against.

## Supported versions

**The latest release is the supported release.** Fixes — including security
fixes — land on `main` and ship as a patch release of the current minor
version. Older releases are not back-patched.

This is a deliberate consequence of how Orion ships: a single binary with
embedded, per-backend database migrations and a documented upgrade path. There
is no supported configuration that requires staying on an old release, so the
answer to a bug in an old release is always the newest one.

| Version            | Status                                     |
|--------------------|--------------------------------------------|
| Latest 1.x release | Supported — receives all fixes             |
| Older 1.x releases | Upgrade to the latest release for fixes    |
| 0.x releases       | End of life                                |

Security vulnerabilities should be reported privately — see the
[security policy](https://github.com/GoPlasmatic/Orion/blob/main/SECURITY.md).

## Versioning

Orion follows [semantic versioning](https://semver.org):

- **Patch** (`1.0.x`) — bug and security fixes. No config, API, or database
  schema changes beyond what the fix requires; always safe to roll forward.
- **Minor** (`1.x.0`) — new capabilities, new configuration keys (with
  defaults that preserve existing behaviour), additive database migrations,
  and possibly an MSRV bump (see below).
- **Major** (`2.0.0`) — reserved for breaking changes to the Admin/Data APIs,
  configuration semantics, or workflow/channel definitions.

The versioned API prefix (`/api/v1/`) is independent of the crate version:
`v1` endpoints keep their request/response contracts for the life of the 1.x
line. Endpoint additions and new optional fields are not considered breaking.

## Upgrade guarantees

- Each release documents its upgrade path from the previous release (for
  1.0.0: [Upgrading to 1.0.0](../getting-started/upgrading.md)). Upgrades are
  supported **release to release**; when skipping releases, read each
  intermediate upgrade page.
- Database migrations are embedded in the binary for all three backends and
  run at boot (`storage.auto_migrate`, default `true`) or explicitly via
  `orion-server migrate`. Shipped migrations are frozen — a released
  migration file is never edited, only appended to.
- **Take a database backup before upgrading.** Migrations are applied
  forward-only; rolling back to an older Orion after a migration has run is
  not supported.

## Rust toolchain (MSRV)

The minimum supported Rust version is **1.88**, declared as `rust-version` in
`Cargo.toml` and enforced by a dedicated CI job. An MSRV bump is a **minor**
release at most and is called out in the changelog. This matters only when
building from source — the released binaries and images are self-contained.

## Database backends

The storage backend is selected at runtime from the `storage.url` scheme. All
three are covered by CI: the full suite runs on SQLite, and dedicated jobs run
the storage and cluster suites against real PostgreSQL and MySQL servers.

| Backend    | Notes |
|------------|-------|
| SQLite     | Default; embedded, zero-configuration. The reference backend. Not usable in [cluster mode](../features/scalability.md). |
| PostgreSQL | Supports cluster mode. Project deployment artifacts and examples use PostgreSQL 16. |
| MySQL      | MySQL 8+; supports cluster mode. New in 1.0.0 — no 0.x MySQL deployment can exist to upgrade ([why](../getting-started/upgrading.md#which-backend-were-you-actually-on)). |

## Platforms

- **Docker images** — published to `ghcr.io/goplasmatic/orion` for
  `linux/amd64` and `linux/arm64` (Debian trixie-slim base).
- **From source** — any platform with a Rust 1.88+ toolchain; Linux and macOS
  are exercised routinely, and CI runs on Linux.

## What support is (and is not)

Orion is open source under the Apache-2.0 license. "Supported" here describes
where fixes ship, not a service-level commitment: issues and vulnerability
reports are triaged on a best-effort basis by the maintainers, with security
reports prioritized. There is no commercial support offering at this time.
