# c4-referee

The rules of Connect Four as a WebAssembly plugin, behind one HTTP route. A
caller sends a game state and the column it wants to play; the `c4.rules`
plugin drops the disc, settles the result (a win, a draw at 42 discs, or a
forfeit on a full or out-of-range column) and the workflow returns the next
state with the board as the next mover sees it.

```
plugin.toml, c4-rules.wasm   the c4.rules plugin: its manifest and the committed component
workflow.json + channel.json c4-move   POST /c4/move   { "state": {...}, "column": 0-6 }
request.json                 a sample move
```

The component is built from [`examples/plugins/c4-rules`](../../plugins/c4-rules/)
and committed here, so the package deploys without a wasm toolchain.

From the repository root, with a server on `http://localhost:8080` started
with `ORION_PLUGINS__ENABLED=true`:

```bash
./examples/deploy.sh c4-referee
```

That uploads and activates the plugin, creates and activates the workflow and
channel, POSTs `request.json` to `POST /api/v2/data/c4/move` and prints the
response: `data.state` is the state after the move and `data.next` the board
from the next mover's side, with the seven legal-column flags.

Without a server:

```bash
jq .data examples/packages/c4-referee/request.json > /tmp/move.json   # dry-run takes the bare payload
orion-server dry-run -w examples/packages/c4-referee/workflow.json -i /tmp/move.json \
  --plugin-dir examples/packages/c4-referee
```

The plugin runs for real in the same sandbox the server uses. The
`c4-rules-*` cases in [`examples/workflow-tests`](../../workflow-tests/) run
this workflow too.
