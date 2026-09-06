<!-- description: Install Orion on macOS, Linux, Windows or Docker, start its embedded database, and verify the server and optional CLI locally. -->
# Install & Run

**Tested with:** Orion 1.7.0 · **Last reviewed:** 2026-09-04

Orion is a single binary with an embedded database. This guide installs it,
starts it locally, and verifies that it is ready. The time required depends on
the installation method; building from source takes longer than installing a
release binary.

In this guide, you will:

- install `orion-server` and the `orion-cli` companion,
- start a server on the default settings,
- verify it answers `/health`.

## Before you start

Choose an installation method available on your system:

| Method | Platforms | Prerequisite |
|---|---|---|
| Homebrew | macOS Apple Silicon, Linux | Homebrew |
| Shell installer | Linux, macOS Apple Silicon | A POSIX shell and `curl` |
| PowerShell installer | Windows | Windows PowerShell |
| Docker | Any Docker-supported platform | Docker Engine or Docker Desktop |
| Source build | Any Rust-supported platform | Git and the Rust toolchain version declared in the repository's `Cargo.toml` |

You also need `curl` to run the health check shown below. PowerShell users can
use `Invoke-RestMethod http://localhost:8080/health` instead.

## Install the server

Pick one method. All of them produce the same binary.

**Homebrew** (macOS Apple Silicon and Linux — Intel Macs build from source):

```bash
brew install GoPlasmatic/tap/orion-server
```

**Shell installer** (Linux, macOS Apple Silicon):

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

**From source** (this release's `Cargo.toml` currently declares Rust 1.98):

```bash
cargo install --git https://github.com/GoPlasmatic/Orion --locked orion-server
```

Both arguments matter. The repository is a cargo workspace with two binary
crates and `cargo install --git` searches the whole repo, so the package has to
be named — without it the install fails rather than picking a default. `--locked`
builds the dependency set the committed `Cargo.lock` pins, which is the one CI
tested; without it cargo re-resolves every dependency to the newest version its
requirements allow.

To install the server and the CLI together, name both packages:

```bash
cargo install --git https://github.com/GoPlasmatic/Orion --locked orion-server orion-cli
```

## Install the CLI

`orion-cli` drives the same admin API from the terminal, and is what an AI assistant uses through the [Orion agent skill](../ai/skills.md). It is optional — every step in these tutorials also has a `curl` form, but it is shorter to type.

The CLI is versioned in lockstep with the server and ships in the same release.
Use matching CLI and server releases so their supported wire formats agree.
Every method below is the server's, with `orion-server` swapped for
`orion-cli`.

**Homebrew** (macOS Apple Silicon and Linux):

```bash
brew install GoPlasmatic/tap/orion-cli
```

**Shell installer** (Linux, macOS Apple Silicon):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-cli-installer.sh | sh
```

**PowerShell** (Windows):

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-cli-installer.ps1 | iex"
```

**From source** (this release's `Cargo.toml` currently declares Rust 1.98):

```bash
cargo install --git https://github.com/GoPlasmatic/Orion --locked orion-cli
```

## Run the server

Start Orion with its defaults — SQLite in `./orion.db`, HTTP on port 8080:

```bash
orion-server
```

Nothing to provision: the database file is created on first boot and the migrations are embedded in the binary. SQLite is the right backend for one instance; see [which backend to use](../reference/configuration.md#storage) before you deploy more than one.

## Verify it

Ask the server how it is doing:

```bash
curl -s http://localhost:8080/health
```

Expected JSON response:

```json
{
  "status": "ok",
  "version": "<installed-version>",
  "uptime_seconds": 5,
  "components": {
    "database": "ok",
    "engine": "ok",
    "connectors": "ok",
    "channels": "ok"
  },
  "git_hash": "<build-commit>",
  "build_timestamp": "<build-timestamp>",
  "workflows_loaded": 0,
  "connectors": { "circuit_breaker_scope": "node", "circuit_breakers": {}, "failed_to_load": [] },
  "channels": { "quarantined": [] }
}
```

This response is illustrative. Build-identifying values change between
releases, and more health components may appear as Orion evolves. Use
`status`, `workflows_loaded`, and the component states for the checks below
rather than comparing the whole response byte for byte.

A status of `"status": "ok"` with `"workflows_loaded": 0` confirms the server is
ready to accept service definitions — you have not created anything yet.
`git_hash` and `build_timestamp` identify the binary; `failed_to_load` and
`quarantined` stay empty until a stored connector or channel cannot be built.

Two more surfaces are live already:

- **Swagger UI** at [http://localhost:8080/docs](http://localhost:8080/docs): the whole admin API, explorable. It is served outside production environments; see [OpenAPI Specification](../reference/openapi.md).
- **The CLI**, once you point it at the server:

  ```bash
  orion-cli config set-server http://localhost:8080
  orion-cli health
  ```

## Change the defaults

Orion reads a TOML config file, and every key in it can be overridden by an `ORION_SECTION__KEY` environment variable:

```bash
orion-server -c config.toml
ORION_SERVER__PORT=9090 orion-server
```

Check a file before you deploy it — `orion-server validate-config -c config.toml` reports unknown keys and invalid values without starting the server. Every setting, its default, and its environment variable are in the [Configuration Reference](../reference/configuration.md).

## Next steps

- [Understand the HTTP Flow](./first-service.md): create and activate a workflow and
  channel in four administration calls, then invoke the REST endpoint.
- [Orion Console](./console.md): the same flow point-and-click, if you would rather not use a terminal.
- [Run the Examples](./examples.md): deploy a ready-made service from the repository instead of writing one.
