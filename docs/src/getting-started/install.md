# Install & Run

Orion is a single binary with an embedded database. Installing it and getting a
running server takes about a minute.

In this guide, you will:

- install `orion-server` and the `orion-cli` companion,
- start a server on the default settings,
- verify it answers `/health`.

## Install the server

Pick one method. All of them produce the same binary.

**Homebrew** (macOS Apple silicon and Linux — Intel Macs build from source):

```bash
brew install GoPlasmatic/tap/orion-server
```

**Shell installer** (Linux, macOS Apple silicon):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.sh | sh
```

**PowerShell** (Windows):

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.ps1 | iex"
```

**Docker:**

```bash
docker run -p 8080:8080 ghcr.io/goplasmatic/orion:latest
```

**From source** (requires Rust 1.88+):

```bash
cargo install --git https://github.com/GoPlasmatic/Orion
```

## Install the CLI

`orion-cli` drives the same admin API from the terminal, and carries the MCP
server that AI assistants connect to. It is optional — every step in these
tutorials also has a `curl` form — but it is shorter to type.

**Homebrew** (macOS and Linux):

```bash
brew install GoPlasmatic/tap/orion-cli
```

**Shell / PowerShell installers:** attached to each `orion-cli-v*` release on the
[releases page](https://github.com/GoPlasmatic/Orion/releases) — copy the
`orion-cli-installer.sh` (Linux/macOS) or `orion-cli-installer.ps1` (Windows)
one-liner from the release notes.

**From source** (requires Rust 1.88+):

```bash
cargo install --git https://github.com/GoPlasmatic/Orion orion-cli
```

## Run the server

Start Orion with its defaults — SQLite in `./orion.db`, HTTP on port 8080:

```bash
orion-server
```

Nothing to provision: the database file is created on first boot and the
migrations are embedded in the binary. SQLite is the right backend for one
instance; see [which backend to
use](../reference/configuration.md#storage) before you deploy more than one.

## Verify it

```bash
curl -s http://localhost:8080/health
```

```json
{
  "status": "ok",
  "version": "1.0.0",
  "uptime_seconds": 5,
  "components": {
    "database": "ok",
    "engine": "ok",
    "connectors": "ok",
    "channels": "ok"
  },
  "git_hash": "1a2b3c4d",
  "build_timestamp": "1786365371",
  "workflows_loaded": 0,
  "connectors": { "circuit_breaker_scope": "node", "circuit_breakers": {}, "failed_to_load": [] },
  "channels": { "quarantined": [] }
}
```

`"status": "ok"` with zero workflows loaded is the expected state of a fresh
install — you have not created anything yet. `git_hash` and `build_timestamp`
identify the binary; `failed_to_load` and `quarantined` stay empty until a
stored connector or channel cannot be built.

Two more surfaces are live already:

- **Swagger UI** at [http://localhost:8080/docs](http://localhost:8080/docs) —
  the whole admin API, explorable. It is served outside production
  environments; see [OpenAPI Specification](../reference/openapi.md).
- **The CLI**, once you point it at the server:

  ```bash
  orion-cli config set-server http://localhost:8080
  orion-cli health
  ```

## Change the defaults

Orion reads a TOML config file, and every key in it can be overridden by an
`ORION_SECTION__KEY` environment variable:

```bash
orion-server -c config.toml
ORION_SERVER__PORT=9090 orion-server
```

Check a file before you deploy it — `orion-server validate-config -c config.toml`
reports unknown keys and invalid values without starting the server. Every
setting, its default, and its environment variable are in the
[Configuration Reference](../reference/configuration.md).

## Next steps

- [Your First Service](./first-service.md) — turn a JSON document into a live
  REST endpoint, in four calls.
- [The Console (Orion UI)](./console.md) — the same flow point-and-click, if you
  would rather not use a terminal.
- [Run the Examples](./examples.md) — deploy a ready-made service from the
  repository instead of writing one.
