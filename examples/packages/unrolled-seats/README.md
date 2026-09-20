# Unrolled seats

One scoring step per seat, written once.

A four-seat game scores every seat the same way. Written out, that is four
copies of one task that differ only in a number, and a generator script to keep
them in step. Here the workflow says it once:

```json
{ "$each": { "seat": { "$from": "constants.seats" } },
  "do": { "id": "score{{seat}}", "name": "Score seat {{seat}}", "function": { … } } }
```

`constants.seats` in `shared.json` is the list `[0, 1, 2, 3]`. The `do` is
copied once per seat, with `{{seat}}` replaced by its number: four steps,
`score0` to `score3`.

The score itself is a **value fragment**, `seat-score`, with a parameter:

```json
{ "$use": "seat-score", "with": { "seat": "{{seat}}" } }
```

It splices the fragment's JSONLogic in place, with its `weight` parameter left
at its default of `10`.

**The server never sees any of this.** `$each`, `$use`, `{{seat}}` and `$from`
are authoring conveniences: `orion-server compile` expands them into the
four ordinary steps the admin API accepts.

## Deploy it

```bash
orion-server compile examples/packages/unrolled-seats --name unrolled-seats --version content -o unrolled-seats.json
orion-server package apply -s http://localhost:8080 -f unrolled-seats.json
```

## Try it

```bash
curl -s -X POST http://localhost:8080/api/v1/data/score-seats \
  -H 'content-type: application/json' -d '{"points": [3, 5, 0, 2]}'
```

The answer carries `data.scores`:
`{"seat0": 30, "seat1": 50, "seat2": 0, "seat3": 20}`.

To see the expanded workflow without a server:

```bash
orion-server compile examples/packages/unrolled-seats --format dir -o /tmp/unrolled
```
