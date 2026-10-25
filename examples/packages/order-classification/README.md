# order-classification

Tiered order classification driven by task-level JSONLogic conditions — each
tier's task runs only when its condition matches. Zero-dependency: runs against
a fresh `orion-server` with no connectors or database.

From the repository root, with a server on `http://localhost:8080`:

```bash
orion-server compile examples/packages/order-classification -o dist/order-classification
orion-cli deployments apply -d dist/order-classification
```

That compiles the workflow and channel into a deployment directory and deploys
it. Then send the sample request:

```bash
curl -s -X POST http://localhost:8080/api/v2/data/order-tiers \
  -H 'Content-Type: application/json' --data @examples/packages/order-classification/request.json
```

See
[`examples/README.md`](../../README.md) for the file layout and the full example
list, and [Run the Examples](https://docs.goplasmatic.io/get-started/tutorials/examples.html)
for the step-by-step walkthrough.
