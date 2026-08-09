# iot-sensor-alert

Range-based sensor severity using `and` / `or` JSONLogic conditions.
Zero-dependency: runs against a fresh `orion-server` with no connectors or
database.

From the repository root, with a server on `http://localhost:8080`:

```bash
./examples/deploy.sh iot-sensor-alert
```

That creates and activates the workflow and channel, POSTs `request.json` to
`POST /api/v1/data/sensors`, and prints the response. See
[`examples/README.md`](../README.md) for the file layout, a step-by-step curl
walkthrough, and the full example list.
