# channel-composition

Two services, one of which calls the other **in-process**. `order-enrichment`
receives an order, looks the customer up by calling the `customer-lookup`
channel through `channel_call`, and merges the tier and discount into its
response. No network hop, no serialization round-trip.

This is the only package here that ships two of everything — two workflows and
two channels — because that is the point: the callee is an ordinary service with
its own lifecycle, not a subroutine of the caller.

| File | What it is |
|------|------------|
| `workflow.json` / `channel.json` | `order-enrichment` — the caller, at `POST /api/v2/data/order-enrichment` |
| `workflow-lookup.json` / `channel-lookup.json` | `customer-lookup` — the callee, independently callable at `POST /api/v2/data/customer-lookup` |

Zero-dependency: runs against a fresh `orion-server` with no connectors or
database.

From the repository root, with a server on `http://localhost:8080`:

```bash
orion-cli compile examples/packages/channel-composition -o dist/channel-composition
orion-cli deployments apply -d dist/channel-composition
```

That deploys both workflows and both channels. POST `request.json` to
`POST /api/v2/data/order-enrichment` to call the caller:

```bash
curl -s -X POST http://localhost:8080/api/v2/data/order-enrichment \
  -H 'Content-Type: application/json' --data @examples/packages/channel-composition/request.json
```

The callee is live too, so this works on its own:

```bash
curl -s -X POST http://localhost:8080/api/v2/data/customer-lookup \
  -H 'Content-Type: application/json' -d '{"data":{"customer_id":42}}'
```

Two things the example is built to show, both easy to get wrong: the sub-request
is assembled in `temp_data` with `map` first, because `channel_call`'s
`data` is a single expression and JSONLogic has no object constructor; and
`output` receives the callee's whole **data context**, not its HTTP envelope —
so the caller reads `data.customer.lookup.tier`, not `data.customer.data.…`.

See [Compose channels in-process](https://docs.goplasmatic.io/guides/workflow-patterns.html)
for the pattern in full, and [`examples/README.md`](../../README.md) for the file
layout and the full example list.
