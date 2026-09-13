# c4-tournament

A Connect Four tournament for tiny neural networks, run entirely by Orion:
every entrant is an ONNX **model** registered from a bucket and admitted by
the node, the referee is a WebAssembly **plugin**, a match is a workflow
`loop` over `channel_call`, the leaderboard is a SQLite table behind a `db`
connector, and a **cron** channel plays a round every hour. It is the
worked example behind [Serve a Model](https://docs.goplasmatic.io/build/models.html),
and the one package that needs both `models.enabled = true` and
`plugins.enabled = true`.

```
plugin.toml, c4-rules.wasm      the referee: c4.rules.start / view / apply   (source: ../../plugins/c4-rules/)
entrant/model.json, c4-tiny.onnx, build.py
                                the reference entrant — 1479 untrained parameters that answer the contract
connector-leaderboard.json      a db connector on a SQLite file; schema.sql is the table
workflow.json + channel.json    c4-register  POST /c4/register    { "model": "<id>" }
workflow-turn.json …            c4-turn      POST /c4/turn        { "state": {…}, "mover_model": "<id>" }
workflow-match.json …           c4-match     POST /c4/match       { "one": "<id>", "two": "<id>" }
workflow-leaderboard.json …     c4-leaderboard  GET /c4/leaderboard
workflow-round.json …           c4-round     cron, 0 0 * * * *    every ordered pair plays once
request*.json, stubs.json       sample bodies, and the model_infer stub for offline runs
```

## The contract

An entrant is a model whose manifest meets one contract, fixed by the
organiser. Its adapters receive this JSON root:

```json
{ "cells": [42 ints], "legal": [7 flags] }
```

`cells` is the board row-major, six rows of seven with row 0 at the top,
**normalised to the mover's perspective**: `0` empty, `1` the mover's own
discs, `2` the opponent's — so an entrant never needs to know which colour
it is playing. `legal[c]` is `1` while column `c` has room. The result
expression must produce `{ "column": c }`, where `c` is an integer or a
one-element list holding one — the shape `argmax` over a `[1, 7]` policy
yields. A column that is full or outside `0–6` is a **forfeit**: the game
ends and the other entrant wins.

The reference entrant's manifest is exactly that contract for a graph that
takes a `[1, 2, 6, 7]` plane stack:

```json
"adapter": { "reshape": [{ "transpose": [{ "crop": [{ "one_hot": [{ "var": "cells" }, 3, "f32"] }, [0, 1], [42, 2]] }, [1, 0]] }, [1, 2, 6, 7]] },
"result":  { "column": { "argmax": [{ "var": "policy" }, 1] } }
```

Its graph has never been trained — 1479 random parameters that produce a
column — which is all a reference needs. The result expression sees only
the output tensors, so this entrant does not mask by `legal`; one that plays
to win should, inside the graph, or by taking `legal` as a second input.

## The organiser's flow

The `models` entity is **not a member of this package**. Entrants arrive
through a pipeline the organiser runs — a bucket, a registration, an
admission verdict, an activation — and `deploy.sh` deploys everything else
and prints that step. With a server on `http://localhost:8080` started with
`models.enabled = true`, `models.cache_dir` set and `plugins.enabled =
true` (`ORION_MODELS__ENABLED=true ORION_MODELS__CACHE_DIR=/tmp/orion-models
ORION_PLUGINS__ENABLED=true orion-server`):

```bash
# 1. The package: plugin, connector, workflows, channels
./examples/deploy.sh c4-tournament

# 2. A bucket the node can read, behind a storage connector. Any S3-compatible
#    store works; for a local run a static file server does — the node fetches
#    with a signed GET, and a server that ignores the signature still answers it.
mkdir -p /tmp/c4-bucket/models && cp examples/packages/c4-tournament/entrant/c4-tiny.onnx /tmp/c4-bucket/models/
python3 -m http.server 9000 --bind 127.0.0.1 --directory /tmp/c4-bucket &
orion-cli connectors create -d '{"name":"c4-bucket","connector_type":"storage","config":{"type":"storage",
  "endpoint":"http://127.0.0.1:9000","region":"us-east-1","bucket":"models",
  "access_key":"AKIAEXAMPLE","secret_key":"example-secret","force_path_style":true,"allow_private_urls":true}}'

# 3. Register the entrant by reference and follow admission: fetch, digest, parse, probe
orion-cli models create -f examples/packages/c4-tournament/entrant/model.json \
  --connector c4-bucket --key c4-tiny.onnx \
  --digest "sha256:$(shasum -a 256 examples/packages/c4-tournament/entrant/c4-tiny.onnx | cut -d' ' -f1)" --wait
orion-cli models get example.c4-tiny        # admission.state: passed; stats.parameters: 1479
orion-cli models activate example.c4-tiny

# 4. Enter it: the probe on an empty board, then the leaderboard row
curl -s -X POST localhost:8080/api/v1/data/c4/register -H 'Content-Type: application/json' \
  --data @examples/packages/c4-tournament/request.json
```

`register` asks the entrant for its opening move on an empty board, refuses
an entrant whose answer is not a legal column, creates the `leaderboard`
table if it is missing, and upserts the row — `model`, `parameters`,
`artifact_bytes`, and zeroed `wins`/`losses`/`draws`. The parameter count
and the size come from `model_infer`'s `stats_output`, which is what the
node measured at admission, not what the entrant claimed. Every further
entrant is the same three commands with its own manifest and digest.

## A turn, a match, a round

```bash
curl -s -X POST localhost:8080/api/v1/data/c4/turn  -H 'Content-Type: application/json' --data @examples/packages/c4-tournament/request-turn.json
curl -s -X POST localhost:8080/api/v1/data/c4/match -H 'Content-Type: application/json' --data @examples/packages/c4-tournament/request-match.json
curl -s localhost:8080/api/v1/data/c4/leaderboard
orion-cli channels trigger c4-round         # a round now, rather than at the top of the hour
```

- **`c4-turn`** takes a state and the id of the model to move: `c4.rules.view`
  renders the board from the mover's side, `model_infer` runs that model —
  `model` is `{"var": "data.input.mover_model"}`, so one workflow serves every
  entrant — with `timeout_ms: 50` and `stats_output`, and `c4.rules.apply`
  plays the column. The response carries the new `state`, the `answer` and
  the `inference` stats.
- **`c4-match`** takes two ids and is a `loop` with `max: 42`: the first
  sweep starts a game, every sweep calls `c4-turn` in-process through
  `channel_call` for whichever side is to move and takes the state back, a
  `filter` task halts the loop once `state.over` is set — the break is a task,
  never the workflow condition, because `data` starts empty — and the last
  sweep records the result: two `db_write` increments, one per entrant, and
  `{ "winner": 0|1|2, "moves": n, "illegal": bool }` in the response. A
  game is over within 42 sweeps by construction.
- **`c4-leaderboard`** is a `GET` with no body: one `data_query`, most wins
  first, and among equals the smaller network.
- **`c4-round`** is a `protocol: "cron"` channel, hourly, `concurrency:
  forbid`: on the first sweep it reads the entrants, then sweep `i` plays
  `entrants[floor(i / n)]` against `entrants[i mod n]` through `c4-match`,
  skipping an entrant against itself, so every ordered pair plays once per
  round — each pairing twice, once per colour. The pairing is JSONLogic over
  the list (`floor`, `%`, a computed `val` index) and needs no payload.

## What Orion guarantees

- **`parameters` is the node's number.** It is read from the graph at
  admission and carried in `stats_output`; the manifest cannot claim a
  smaller network than it is, and `models.max_parameters` caps the field.
- **The adapters are budgeted.** An entrant's adapter and result are
  JSONLogic compiled on the serving engine and priced by `engine.ops_budget`,
  so a manifest cannot make a turn arbitrarily expensive.
- **The adapters are isolated.** They cannot read `{"secret": …}`, the clock
  or randomness — refused at registration — and the graph sees tensors and
  nothing else: no connectors, no context, no I/O.
- **CPU determinism.** The same bytes over the same board land the same
  policy on every node, so a replayed trace reproduces the game.
- **The referee is the same for everyone.** `c4.rules` runs in the plugin
  sandbox with no way in but its input, and every move goes through it.

## Offline and in CI

```bash
orion-server lint examples/packages/c4-tournament          # the set: every reference resolves, the plugin's field tables checked
orion-server clippy examples/packages/c4-tournament
orion-server test examples/workflow-tests \
  --plugin-dir examples/packages/c4-tournament --model-dir examples/packages/c4-tournament/entrant
```

All three cases run the referee and the reference entrant for real — the
entrant is deterministic on the CPU, so `c4-rules-referee` can assert the
cell its answer lands in and `c4-rules-forfeit` hands it a board on which it
picks a full column and loses by forfeit. Without `--model-dir`, `model_infer`
is answered from a stub instead — `stubs.json` is one, and how the referee
is exercised before any entrant exists:

```bash
jq .data examples/packages/c4-tournament/request-turn.json > /tmp/turn.json   # dry-run takes the bare payload
orion-server dry-run -w examples/packages/c4-tournament/workflow-turn.json -i /tmp/turn.json \
  --plugin-dir examples/packages/c4-tournament --stubs examples/packages/c4-tournament/stubs.json
```

The repository's e2e suite (`./tests/e2e/run.sh 18`) does the
organiser's flow above against a real server, with `python3 -m http.server`
as the bucket. See [`examples/README.md`](../../README.md) for the file
layout and the full example list, and [Models](https://docs.goplasmatic.io/concepts/models.html)
for what a model is and why it is shaped this way.
