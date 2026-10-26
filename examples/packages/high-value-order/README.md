# high-value-order

Flag orders over a value threshold and build an alert string with the `cat`
operator. Zero-dependency: runs against a fresh `orion-server` with no
connectors or database.

From the repository root, with a server on `http://localhost:8080`:

```bash
orion-cli compile examples/packages/high-value-order -o dist/high-value-order
orion-cli deployments apply -d dist/high-value-order
```

That compiles the workflow and channel into a deployment directory and deploys
it. Then send the sample request:

```bash
curl -s -X POST http://localhost:8080/api/v2/data/high-value-orders \
  -H 'Content-Type: application/json' --data @examples/packages/high-value-order/request.json
```

See
[`examples/README.md`](../../README.md) for the file layout and the full example
list, and [Run the Examples](https://docs.goplasmatic.io/get-started/tutorials/examples.html)
for the step-by-step walkthrough.
