<!-- description: Install orion-server and orion-cli on macOS, Linux or Windows, or run the Docker image, then start a server on its defaults and verify it is ready. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Install and run Orion

Orion is a single binary with an embedded database. This guide installs it, starts it on its defaults, and verifies that it is ready to accept definitions.

## Before you start

Tested with Orion 1.8.2. Choose one installation method. Every method produces the same binary.

| Method | Platforms | You need |
|---|---|---|
| Homebrew | macOS Apple Silicon, Linux | Homebrew |
| Shell installer | Linux, macOS Apple Silicon | A POSIX shell and `curl` |
| PowerShell installer | Windows | Windows PowerShell |
| Docker | Anywhere Docker runs | Docker Engine or Docker Desktop |
| Source build | Anywhere Rust runs | Git and Rust 1.98 or newer |

You also need `curl` for the verification step. In PowerShell, `Invoke-RestMethod http://localhost:8080/health` does the same job.

## Install the server

<div class="tabs">
<section data-tab="Homebrew">

macOS Apple Silicon and Linux. Intel Macs build from source:

```bash
brew install GoPlasmatic/tap/orion-server
```

</section>
<section data-tab="Shell">

Linux and macOS Apple Silicon:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.sh | sh
```

</section>
<section data-tab="PowerShell">

Windows:

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-server-installer.ps1 | iex"
```

</section>
<section data-tab="Docker">

Any platform. The image is the server with no CLI:

```bash
docker run -p 8080:8080 ghcr.io/goplasmatic/orion:latest
```

</section>
<section data-tab="Source">

Any platform with a Rust toolchain at the version `Cargo.toml` declares, currently 1.98:

```bash
cargo install --git https://github.com/GoPlasmatic/Orion --locked orion-server
```

Both arguments matter. The repository is a workspace with two binary crates, so the package must be named. `--locked` builds the dependency set the committed `Cargo.lock` pins, which is the one CI tested. To install the CLI in the same command, name both packages: `orion-server orion-cli`.

</section>
</div>

## Install the CLI

`orion-cli` drives the same admin API from the terminal. It is what an AI assistant uses through the [agent skill](../guides/ai/agent-skill.md), and it is optional: every step in the tutorials also has a `curl` form.

The CLI ships in the same release as the server. Use matching versions so their wire formats agree. Every method is the server's with `orion-server` swapped for `orion-cli`:

<div class="tabs">
<section data-tab="Homebrew">

```bash
brew install GoPlasmatic/tap/orion-cli
```

</section>
<section data-tab="Shell">

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-cli-installer.sh | sh
```

</section>
<section data-tab="PowerShell">

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/GoPlasmatic/Orion/releases/latest/download/orion-cli-installer.ps1 | iex"
```

</section>
<section data-tab="Source">

```bash
cargo install --git https://github.com/GoPlasmatic/Orion --locked orion-cli
```

</section>
</div>

## Run the server

Start Orion on its defaults, which are SQLite in `./orion.db` and HTTP on port 8080:

```bash
orion-server
```

There is nothing to provision. The database file is created on first boot and the migrations are embedded in the binary. SQLite is the right backend for one instance; read [which backend to use](../reference/configuration/storage.md) before you run more than one.

## Verify

Ask the server how it is doing:

```bash
curl -s http://localhost:8080/health
```

Output, with build values that differ per release:

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

`"status": "ok"` with `"workflows_loaded": 0` means the server is ready and holds no definitions yet. Read `status`, `workflows_loaded` and the component states rather than comparing the whole body: more components may appear in later releases.

Two more surfaces are live already. Swagger UI at [http://localhost:8080/docs](http://localhost:8080/docs) shows the whole admin API; it is served outside production environments (see [OpenAPI specification](../reference/openapi.md)). And the CLI, once you point it at the server:

```bash
orion-cli config set-server http://localhost:8080
orion-cli health
```

## Change the defaults

Orion reads a TOML file, and any key in it can be overridden by an `ORION_SECTION__KEY` environment variable:

```bash
orion-server -c config.toml
ORION_SERVER__PORT=9090 orion-server
```

Check a file before you deploy it. `orion-server validate-config -c config.toml` reports unknown keys and invalid values without starting the server. Every setting, its default and its environment variable are in [Server configuration](../reference/configuration/index.md).

## Next steps

- [Quickstart](./quickstart.md): define one service and call it, in about five minutes.
- [Build your first service](./tutorials/first-service.md): the same four administration calls, one at a time, with the CLI beside each.
- [Use the Orion Console](../guides/patterns/console.md): the same flow in a browser, if you would rather not use a terminal.
- [Run the example packages](./tutorials/examples.md): deploy a ready-made service from the repository instead of writing one.
