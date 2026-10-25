# order-summary

The complete, dependency-free files used by the documentation's “Understand
the HTTP Flow” tutorial. The workflow parses an order and adds a human-readable
summary; the channel exposes it at `POST /order-summary`.

From the repository root with Orion running on port 8080:

```bash
orion-server compile examples/packages/order-summary -o dist/order-summary
orion-cli deployments apply -d dist/order-summary
```

Applying the same directory again is a no-op: the CLI reports the deployment as
up to date.
