# high-value-order

Flag orders over a value threshold and build an alert string with the `cat`
operator. Zero-dependency: runs against a fresh `orion-server` with no
connectors or database.

From the repository root, with a server on `http://localhost:8080`:

```bash
./examples/deploy.sh high-value-order
```

That creates and activates the workflow and channel, POSTs `request.json` to
`POST /api/v1/data/high-value-orders`, and prints the response. See
[`examples/README.md`](../../README.md) for the file layout and the full example
list, and [Run the Examples](https://goplasmatic.github.io/Orion/getting-started/examples.html)
for the step-by-step walkthrough.
