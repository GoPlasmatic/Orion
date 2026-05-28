# Documentation recordings

The terminal demos in the READMEs and the mdBook are generated from this folder,
not captured by hand. Each flow has a `demo-*.sh` script that drives a real
`orion-server` + `orion-cli`; [`record.sh`](record.sh) boots a throwaway server,
records the session with [asciinema](https://asciinema.org), and produces **both**
artifacts from that single cast:

| Flow | Script | GIF (READMEs) | Cast (mdBook player) |
|------|--------|---------------|----------------------|
| Quickstart (curl/HTTP) | `demo-quickstart.sh` | `../../media/quickstart.gif` | `../src/casts/quickstart.cast` |
| CLI lifecycle | `demo-cli-lifecycle.sh` | `../../../Orion-cli/media/cli-lifecycle.gif` | `../src/casts/cli-lifecycle.cast` |
| MCP session | `demo-mcp.sh` | `../../../Orion-cli/media/mcp.gif` | `../src/casts/mcp.cast` |

Recording the cast once and rendering the GIF from it (via `agg`) means the GIF and
the interactive player can never drift apart.

## Regenerate

```bash
brew install asciinema agg jq      # one-time: recorder, cast→gif renderer, JSON tool
./record.sh                        # rebuilds binaries if needed, records all three
```

Useful overrides:

```bash
DRY=1 ./record.sh                  # run the demo scripts live (no recording) to eyeball output
ORION_PORT=8090 ./record.sh        # use a different port (the quickstart GIF shows :8080)
```

`record.sh` uses the debug binaries at `../../target/debug/orion-server` and
`../../../Orion-cli/target/debug/orion-cli`, building them if they are missing.
