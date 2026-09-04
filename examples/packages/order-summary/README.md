# order-summary

The complete, dependency-free files used by the documentation's “Understand
the HTTP Flow” tutorial. The workflow parses an order and adds a human-readable
summary; the channel exposes it at `POST /order-summary`.

From the repository root with Orion running on port 8080:

```bash
./examples/deploy.sh order-summary
```

The deploy script is repeatable: existing definitions are skipped and the
sample request is sent again.
