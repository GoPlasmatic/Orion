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

Orion is pre-1.0: security fixes land on `main` and ship in the **latest
release**. Older minor versions are not patched — upgrade to the newest
release to receive fixes.

## Scope

Reports are especially welcome for Orion's security-relevant surfaces:

- Admin API authentication and authorization
- SSRF protections on `http_call` and HTTP connectors
- Injection resistance of the portable data dialect (`data_query` /
  `data_write`) and the SQL/Mongo/ES renderers
- Connector secret storage, masking, and `env://` / `${VAR}` resolution
- Input validation, payload limits, and deserialization of untrusted JSON
- Rate limiting, backpressure, and other abuse-prevention mechanisms

Dependency advisories are tracked automatically (`cargo audit` runs in CI and
Dependabot security updates are enabled) — no need to report those unless you
can show Orion is exploitable through one.
