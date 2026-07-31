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

## Deprecations

**1.0.0 ships no deprecated spellings.** Where 1.0 renamed a key, the pre-1.0
name is refused, not quietly accepted — there is no compatibility window to
track and nothing scheduled for removal in a 1.x minor.

That is a deliberate choice, and the reason is what a silently-accepted old name
costs. `cors` → `origin_allow_list` is the clearest case: had the old key parsed
and been dropped, every channel using it would have served with no origin
allow-list, indistinguishable from a channel that deliberately checks nothing.
The failure would have been silent, permanent, and a security regression. The
same argument applies to `backpressure.max_concurrent`, whose replacement means
something different (per node, not per cluster) — accepting it under the new
field would admit N× the intended concurrency.

Deprecations introduced *after* 1.0 follow the normal rule: announced in a minor
release with the old spelling still working, removed no earlier than the next
major.

### How a rename fails, by surface

The two surfaces fail differently, because they are edited at different times by
different people:

| Surface | Owner | How a stale name fails |
|---|---|---|
| Config file and `ORION_*` environment | Operator, edited at deploy time | **Startup error** naming the replacement. Unknown keys are refused (`deny_unknown_fields` throughout), and every renamed environment variable is listed in a retired-names table so the message says what to set instead. |
| Channel config and workflow JSON | Author, stored in the database | **Refused at create/update**, and **quarantined at load** if already stored — the channel is refused at every ingress rather than served with a guard missing. |

Nobody hand-edits stored channel and workflow rows during an upgrade, which is
why that surface cannot rely on a startup error the way the config file does.
Quarantine is the equivalent: loud, fail-closed, and visible on `/health` and
the admin surface.

Run [`orion-server preflight`](../getting-started/upgrading.md) before upgrading.
It reads the stored estate and the environment and names every entity that will
fail, so the failures above are something you see before the rollout rather than
during it.

### Accepted alternate spellings

One spelling is accepted in addition to the documented one, and it is not a
deprecation with a removal date:

- **`http_call.response_path`** — an alias for `output`, carried by the
  `HttpCallConfig` struct in `dataflow-rs`, which Orion does not own. `output` is
  the documented spelling and the one every other function uses; supplying both
  is a duplicate-field error. It will stop being accepted if and when dataflow-rs
  removes it.

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
