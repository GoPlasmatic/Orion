# webhook-transform

Normalize differing provider webhook payloads into one internal schema with
null-safe `var` mapping. Zero-dependency: runs against a fresh `orion-server`
with no connectors or database.

From the repository root, with a server on `http://localhost:8080`:

```bash
orion-cli compile examples/packages/webhook-transform -o dist/webhook-transform
orion-cli deployments apply -d dist/webhook-transform
```

That compiles the workflow and channel into a deployment directory and deploys
it. Then send the sample request:

```bash
curl -s -X POST http://localhost:8080/api/v2/data/webhooks \
  -H 'Content-Type: application/json' --data @examples/packages/webhook-transform/request.json
```

See
[`examples/README.md`](../../README.md) for the file layout and the full example
list, and [Run the Examples](https://docs.goplasmatic.io/get-started/tutorials/examples.html)
for the step-by-step walkthrough.
