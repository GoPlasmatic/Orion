# webhook-transform

Normalize differing provider webhook payloads into one internal schema with
null-safe `var` mapping. Zero-dependency: runs against a fresh `orion-server`
with no connectors or database.

From the repository root, with a server on `http://localhost:8080`:

```bash
./examples/deploy.sh webhook-transform
```

That creates and activates the workflow and channel, POSTs `request.json` to
`POST /api/v1/data/webhooks`, and prints the response. See
[`examples/README.md`](../../README.md) for the file layout and the full example
list, and [Run the Examples](https://docs.goplasmatic.io/getting-started/examples.html)
for the step-by-step walkthrough.
