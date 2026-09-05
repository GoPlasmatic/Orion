# Security Policy

## Reporting a vulnerability

**Please do not report security vulnerabilities in public issues, discussions,
or pull requests.**

Report them privately via GitHub's vulnerability reporting:
[github.com/GoPlasmatic/Orion/security/advisories/new](https://github.com/GoPlasmatic/Orion/security/advisories/new)
(repository **Security** tab → *Report a vulnerability*).

Include what you can of: an impact description, steps to reproduce (workflow /
channel / connector JSON and requests), the Orion version
(`orion-server --version`), and any relevant configuration (with secrets
redacted).

You can expect an acknowledgement within a few days. Please give us a
reasonable window to ship a fix before disclosing publicly — we'll credit you
in the release notes unless you prefer otherwise.

## Supported versions

| Version            | Supported                                  |
|--------------------|--------------------------------------------|
| Latest 1.x release | ✅ Receives all fixes, including security   |
| Older 1.x releases | ⬆️ Upgrade to the latest release for fixes  |
| 0.x releases       | ❌ End of life                              |

Security fixes land on `main` and ship as a **patch release of the current
minor version**. Older releases are not back-patched: migrations are embedded
in the binary and the upgrade path between releases is documented, so the
supported response to a vulnerability is always to upgrade to the newest
release. The full statement of what "supported" covers — versioning, upgrade
guarantees, MSRV, database and platform compatibility — is the
[Support & Compatibility policy](https://docs.goplasmatic.io/reference/support.html).

## Scope

Reports are especially welcome for Orion's security-relevant surfaces:

- Admin API authentication and authorization
- SSRF protections on `http_call` and HTTP connectors
- Injection resistance of the portable data dialect (`data_query` /
  `data_write`) and the SQL/Mongo/ES renderers
- Connector secret storage, masking, and `env://` / `${VAR}` resolution
- Input validation, payload limits, and deserialization of untrusted JSON
- Rate limiting, backpressure, and other abuse-prevention mechanisms
- The plugin sandbox: a WebAssembly component escaping its world (which
  imports nothing), exceeding a configured ceiling (memory, wall clock, fuel,
  input/output size, concurrency) without being stopped, reaching another
  message's state, or getting guest-controlled text into a metric label or a
  client response; and `[plugins.trust]` signature verification

Dependency advisories are tracked automatically (`cargo deny check` runs in
CI — advisories, licenses, bans, and sources per `deny.toml` — and Dependabot
keeps crate and Actions versions current per `.github/dependabot.yml`) — no
need to report those unless you can show Orion is exploitable through one.

### Wasmtime update policy

The plugin sandbox is [Wasmtime](https://wasmtime.dev) with Cranelift, and
both are part of Orion's trusted computing base whenever `plugins.enabled`
is on. Wasmtime publishes its own
[security advisories](https://github.com/bytecodealliance/wasmtime/security/advisories)
and supports its latest release plus the previous one. Orion tracks the
current major: Dependabot raises the update, `cargo deny check` fails CI on an
advisory against the pinned version, and a Wasmtime advisory that affects the
component model, the pooling allocator, fuel or epoch interruption ships in
the next Orion patch release rather than waiting for a minor. The MSRV moves
with Wasmtime's when it has to (`CONTRIBUTING.md`). Operators who do not run
plugins are unaffected: with `plugins.enabled = false` no Wasmtime engine is
constructed and no component is ever compiled.
