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

Dependency advisories are tracked automatically (`cargo deny check` runs in
CI — advisories, licenses, bans, and sources per `deny.toml` — and Dependabot
keeps crate and Actions versions current per `.github/dependabot.yml`) — no
need to report those unless you can show Orion is exploitable through one.
