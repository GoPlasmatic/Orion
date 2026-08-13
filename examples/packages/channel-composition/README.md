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
| `workflow.json` / `channel.json` | `order-enrichment` — the caller, at `POST /api/v1/data/order-enrichment` |
| `workflow-lookup.json` / `channel-lookup.json` | `customer-lookup` — the callee, independently callable at `POST /api/v1/data/customer-lookup` |

Zero-dependency: runs against a fresh `orion-server` with no connectors or
database.

From the repository root, with a server on `http://localhost:8080`:

```bash
./examples/deploy.sh channel-composition
```

That creates and activates both workflows and both channels, POSTs
`request.json` to `POST /api/v1/data/order-enrichment`, and prints the response.
The callee is live too, so this works on its own:

```bash
curl -s -X POST http://localhost:8080/api/v1/data/customer-lookup \
  -H 'Content-Type: application/json' -d '{"data":{"customer_id":42}}'
```

Two things the example is built to show, both easy to get wrong: the sub-request
is assembled in `temp_data` with `map` first, because `channel_call`'s
`data_logic` is a single expression and JSONLogic has no object constructor; and
`output` receives the callee's whole **data context**, not its HTTP envelope —
so the caller reads `data.customer.lookup.tier`, not `data.customer.data.…`.

See [Compose channels in-process](https://docs.goplasmatic.io/guides/workflow-patterns.html)
for the pattern in full, and [`examples/README.md`](../../README.md) for the file
layout and the full example list.
