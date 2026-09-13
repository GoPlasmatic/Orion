# c4.rules — a plugin, built from source

The source of the referee the [`c4-tournament`](../../packages/c4-tournament/)
package deploys: the rules of Connect Four as three pure functions, written
against [`orion-plugin-sdk`](../../../crates/orion-plugin-sdk/). A model
picks the column; this plugin says what the move did — win, draw, or forfeit
— and shows the board to the next mover from its own perspective. Rules are
what a plugin is for: they are compiled, they are pure, and they run on every
move where a JSONLogic spelling of "four in a row in any direction" would be
the longest thing in the workflow.

```
guest/            the Rust crate: a `cdylib` implementing `Plugin` and calling `export_plugin!`
build.sh          builds it for wasm32-unknown-unknown and writes the component beside the manifest
```

The manifest (`plugin.toml`) and the built component live in the package
directory, not here: they are what deploys, and a manifest names its
component relative to itself.

## The functions

The state every function speaks is one object:

```json
{ "board": [42 ints], "to_move": 1, "moves": 0, "over": false, "winner": 0, "illegal": false }
```

`board` is row-major, six rows of seven with **row 0 at the top**; a dropped
disc lands in the empty cell with the largest row index. `0` is empty, `1`
player one's disc, `2` player two's. `moves` is the number of discs on the
board and is recomputed from it, never read.

| Function | Input | Output |
|---|---|---|
| `c4.rules.start` | nothing | the initial state — empty board, player one to move |
| `c4.rules.view` | `state` | `{ "cells": [42 ints], "legal": [7 flags] }` from the mover's perspective: the mover's discs are `1`, the opponent's `2`; `legal[c]` is `1` while column `c` has room and the game is on. This is the contract an entrant model is written against |
| `c4.rules.apply` | `state`, `column` | the next state. `column` is an integer or a one-element list holding one (what `argmax` over a `[1, 7]` policy yields). Four in a row in any direction wins; a 42nd disc with no winner is a draw (`winner: 0`); a column that is full or out of range is a **forfeit** — `illegal` and `over` are set, the board is unchanged, and the other player wins |

Every refusal is `caller-input` with a stable code — `BAD_STATE`,
`BAD_COLUMN`, `GAME_OVER`, `UNKNOWN_FUNCTION` — because the same input cannot
succeed on a retry.

## Rebuild and test

```bash
cd examples/plugins/c4-rules/guest && cargo test    # the rules, on the host: wins in every direction, the draw, the forfeits, the perspective flip
rustup target add wasm32-unknown-unknown
cargo install wasm-tools
./examples/plugins/c4-rules/build.sh                # the component, committed beside the manifest
```

The export is gated on the wasm target, which is what lets `cargo test` link
the same source into a host test binary.

## Try it without a server

```bash
orion-server test examples/workflow-tests \
  --plugin-dir examples/packages/c4-tournament --model-dir examples/packages/c4-tournament/entrant
jq .data examples/packages/c4-tournament/request-turn.json > /tmp/turn.json   # dry-run takes the bare payload
orion-server dry-run -w examples/packages/c4-tournament/workflow-turn.json -i /tmp/turn.json \
  --plugin-dir examples/packages/c4-tournament --stubs examples/packages/c4-tournament/stubs.json
```

Both run the rules for real in the same sandbox the server uses. With
`--model-dir` the entrant in between runs for real too; without it, the
`model_infer` is what the stub file answers, which is how the referee is
exercised before any entrant exists. See
[Build a plugin](https://docs.goplasmatic.io/build/plugins.html) for how a
plugin is written and [Serve a model](https://docs.goplasmatic.io/build/models.html)
for the half of the tournament that is a model.
