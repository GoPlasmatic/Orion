<!-- description: Orion UI is the operations console over the admin API: live dashboards, a channel to workflow to connector system map, trace drill-downs and a data console. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Use the Orion Console

Orion itself is API-first; everything in this book is plain HTTP. [Orion UI](https://github.com/GoPlasmatic/Orion-ui) is the operations console on top of that API. It has live dashboards, a System Map of every channel, workflow and connector, workflow logic visualization, trace drill-downs, and a Data Console for test requests.

## Before you start

Tested with Orion 1.8.0. You need Docker and a running Orion server on port 8080. The command below starts the console on port 8081. On Linux, `host.docker.internal` may need Docker's `host-gateway` mapping; the Orion UI repository documents the alternatives.

The video shows the full creation loop. You import a workflow (paste, validate, dry-run, activate) and watch its logic render as a graph. Then you give it an endpoint with the channel form, send a request from the Data Console, and see the service on the System Map.

<div class="themed-media">
  <video class="media-dark" controls muted playsinline preload="metadata" src="../../videos/ui-quickstart-dark.webm"></video>
  <video class="media-light" controls muted playsinline preload="metadata" src="../../videos/ui-quickstart-light.webm"></video>
</div>
<span class="asciinema-caption">The same flow as <a href="../../get-started/tutorials/first-service.html">Build your first service</a>, as clicks instead of curl. Click to play.</span>

## Run it

The UI ships as a container image (nginx, multi-arch):

```bash
docker run --name orion-console -p 8081:8080 \
  -e ORION_URL=http://host.docker.internal:8080 \
  ghcr.io/goplasmatic/orion-ui:latest
```

Open `http://localhost:8081`. `ORION_URL` points at your Orion server, and the bundled nginx reverse-proxies all `/api/` requests to it. To develop against a local checkout, `npm install && npm run dev` in the [Orion-ui repository](https://github.com/GoPlasmatic/Orion-ui) does the same through the Vite dev server. A `docker-compose.yml` that brings up server and UI together is in that repository as well.

> [!NOTE]
> Keep the console and the server in step. The console talks to the admin API, and 1.0 moved ten of its endpoints under a `{"data": …}` envelope ([details](../../releases/upgrade-to-1.0/api-and-response-shape.md#every-admin-response-is-now-wrapped-in-data)). A console image built before 1.0 renders empty values against a 1.0 server rather than erroring. `:latest` is fine for a first look; for anything you depend on, pin the tag and move both together.

## Verify

Open the Operations dashboard. It shows live request rate, error rate, latency percentiles, outcomes by channel, top channels and recent traces. It also lists anything that needs attention: open circuit breakers, idle channels, recent failures.

<div class="themed-media">
  <img class="media-dark" src="../../images/ui-operations-dark.png" alt="Operations dashboard: request rate, error rate, latency percentiles, outcomes by channel, top channels, and recent traces">
  <img class="media-light" src="../../images/ui-operations-light.png" alt="Operations dashboard: request rate, error rate, latency percentiles, outcomes by channel, top channels, and recent traces">
</div>

## What you get

### System Map

Pick any channel and trace it through the workflow it runs, the channels it calls in-process, and the connectors it touches. The view is a live topology graph. Every node links to its detail page.

<div class="themed-media">
  <img class="media-dark" src="../../images/ui-system-map-dark.png" alt="System Map: a channel traced through its workflow and connectors as a topology graph">
  <img class="media-light" src="../../images/ui-system-map-light.png" alt="System Map: a channel traced through its workflow and connectors as a topology graph">
</div>

### Workflow logic, visualized

Workflows are managed through a guided import wizard: paste JSON, validate it, import as a draft, dry-run it against a sample payload, then activate. On the detail page, each task's JSONLogic renders as a flow graph, with tabs for relationships, dry-run testing, version history, and the raw JSON.

<div class="themed-media">
  <img class="media-dark" src="../../images/ui-workflow-dag-dark.png" alt="Workflow detail: task explorer with the selected task's JSONLogic rendered as a flow graph">
  <img class="media-light" src="../../images/ui-workflow-dag-light.png" alt="Workflow detail: task explorer with the selected task's JSONLogic rendered as a flow graph">
</div>

### Data Console

Send test requests to any channel, sync or async, with optional per-task profiling. Inspect the response, the request profile with per-function and per-connector timings, and the resulting trace, one click away.

<div class="themed-media">
  <img class="media-dark" src="../../images/ui-console-dark.png" alt="Data Console: send a test request to a channel and inspect the response, per-task timings, and trace">
  <img class="media-light" src="../../images/ui-console-light.png" alt="Data Console: send a test request to a channel and inspect the response, per-task timings, and trace">
</div>

The console also holds channel and connector management with lifecycle actions, circuit-breaker monitoring and reset, the audit log, and trace search and drill-down. A command palette (<kbd>⌘K</kbd>) jumps anywhere. Every visual on this page is generated from a live instance by the [recording pipeline](https://github.com/GoPlasmatic/Orion/tree/main/docs/recordings); re-run `record-ui.sh` and they regenerate.

## Clean up

Stop the foreground container with `Ctrl-C`, then remove it:

```bash
docker rm orion-console
```

The Console stores Orion definitions through the server, so removing the UI container does not delete workflows, channels, connectors or traces.

## Next steps

- [Build your first service](../../get-started/tutorials/first-service.md): the same flow as four administration calls and one endpoint request, if you would rather see the wire format.
- [Run the example packages](../../get-started/tutorials/examples.md): services to import and click through.
- [Monitor and alert](../../operate/run/monitoring.md): the metrics behind the dashboard, and what to alert on.
