# iot-sensor-alert

Range-based sensor severity using `and` / `or` JSONLogic conditions.
Zero-dependency: runs against a fresh `orion-server` with no connectors or
database.

From the repository root, with a server on `http://localhost:8080`:

```bash
orion-server compile examples/packages/iot-sensor-alert -o dist/iot-sensor-alert
orion-cli deployments apply -d dist/iot-sensor-alert
```

That compiles the workflow and channel into a deployment directory and deploys
it. Then send the sample request:

```bash
curl -s -X POST http://localhost:8080/api/v2/data/sensors \
  -H 'Content-Type: application/json' --data @examples/packages/iot-sensor-alert/request.json
```

See
[`examples/README.md`](../../README.md) for the file layout and the full example
list, and [Run the Examples](https://docs.goplasmatic.io/get-started/tutorials/examples.html)
for the step-by-step walkthrough.
