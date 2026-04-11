# Architecture Overview

## Three Primitives

Services in Orion are composed from three building blocks:

| Primitive | Role | Examples |
|-----------|------|----------|
| **Channel** | Service endpoint: sync (REST, HTTP) or async (Kafka) | `POST /orders`, `GET /users/{id}`, Kafka topic `order.placed` |
| **Workflow** | Pipeline of tasks that defines what the service does | Parse → validate → enrich → transform → respond |
| **Connector** | Named connection to an external system with auth and retries | Stripe API, PostgreSQL, Redis, Kafka cluster |

Channels receive traffic. Workflows process it. Connectors reach out to external systems. Everything else (rate limiting, metrics, circuit breakers, versioning) is handled by the platform.

## Deployment Topology

### Before Orion

Every piece of business logic is its own service to build, deploy, and operate, each with its own infrastructure stack:

<div id="diagram-before" class="d3-diagram" style="height:700px"></div>

**4 services x (code + Dockerfile + CI pipeline + health checks + metrics agent + log agent + sidecar proxy + scaling policy + secret config + canary rollout) = dozens of components to build, wire, and keep running.**

### After Orion

One Orion instance replaces all four:

<div id="diagram-after" class="d3-diagram" style="height:420px"></div>

No API gateway needed. Governance is built in. One binary to deploy.

**The best of both worlds:** each channel and workflow is independently versioned, testable, and deployable. The modularity of microservices with the operational simplicity of a monolith.

## Deploy Anywhere

<div id="diagram-deploy" class="d3-diagram" style="height:280px"></div>

Single binary. SQLite by default, no database to provision, no runtime dependencies. Need more scale? Swap to **PostgreSQL** or **MySQL** by changing `storage.url`. No rebuild needed.

Same channel definitions work in any topology: run everything in one instance, split channels across instances with include/exclude filters, or deploy as sidecars.

## Request Processing Flow

<div id="diagram-flow" class="d3-diagram" style="height:200px"></div>

1. **Route Resolution:** REST pattern matching finds the channel, or falls back to name lookup
2. **Channel Registry:** enforces deduplication, rate limits, input validation, backpressure, and checks the response cache
3. **Engine:** the workflow engine sits behind a double-Arc (`Arc<RwLock<Arc<Engine>>>`) allowing zero-downtime swaps
4. **Workflow Matcher:** evaluates JSONLogic conditions and rollout percentages to pick the right workflow
5. **Task Pipeline:** executes functions in order (parse, map, filter, http_call, db_read, etc.)

## Sync and Async

```
Sync     POST /api/v1/data/{channel}         → immediate response
Async    POST /api/v1/data/{channel}/async   → returns trace_id, poll later

REST     GET /api/v1/data/orders/{id}        → matched by route pattern
Kafka    topic: order.placed                 → consumed automatically
```

Sync channels respond immediately. Async channels return a trace ID; poll `GET /api/v1/data/traces/{id}` for results. Kafka channels consume from topics configured in the DB or config file.

**Bridging is a pattern, not a feature.** A sync workflow can `publish_kafka` and return 202. An async channel picks it up from there.

## Service Composition

Most platforms require HTTP calls between services, adding latency, failure modes, and serialization overhead. Orion's `channel_call` invokes another channel's workflow **in-process** with zero network round-trip:

```
POST /orders (order-processing workflow)
  ├── parse_json         → extract order data
  ├── channel_call       → "inventory-check" channel (in-process)
  ├── channel_call       → "customer-lookup" channel (in-process)
  ├── map                → compute pricing with enriched data
  └── publish_json       → return combined result
```

Each composed channel has its own workflow, versioning, and governance, but calls between them are function calls, not network hops. Cycle detection prevents infinite recursion.

## Built-in Task Functions

| Function | Description |
|----------|-------------|
| `parse_json` | Parse payload into the data context |
| `parse_xml` | Parse XML payloads into structured JSON |
| `filter` | Allow or halt processing based on JSONLogic conditions |
| `map` | Transform and reshape JSON using JSONLogic expressions |
| `validation` | Enforce required fields and constraints |
| `http_call` | Invoke downstream APIs via connectors |
| `channel_call` | Invoke another channel's workflow in-process |
| `db_read` / `db_write` | Execute SQL queries, return rows/affected count |
| `cache_read` / `cache_write` | Read/write to in-memory or Redis cache |
| `mongo_read` | Query MongoDB collections |
| `publish_json` / `publish_xml` | Serialize data to JSON or XML output |
| `publish_kafka` | Publish messages to Kafka topics |
| `log` | Emit structured log entries |

<script src="https://d3js.org/d3.v7.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/@dagrejs/dagre@1.1.4/dist/dagre.min.js"></script>
<script>
(function () {
  /* ═══════════════════════════════════════════════════════════
     Shared D3 + Dagre diagram renderer
     ═══════════════════════════════════════════════════════════ */

  var isDark = document.documentElement.classList.contains('coal')
    || document.documentElement.classList.contains('navy')
    || document.documentElement.classList.contains('ayu');

  var COLORS = {
    bg:      isDark ? '#1e1e2e' : '#f5f6fa',
    fg:      isDark ? '#cdd6f4' : '#2d3436',
    edge:    isDark ? '#585b70' : '#b2bec3',
    gwFill:  isDark ? '#45475a' : '#dfe6e9',  gwBorder: isDark ? '#585b70' : '#636e72',
    svcFill: isDark ? '#1e3a5f' : '#74b9ff',  svcBorder: isDark ? '#0984e3' : '#0984e3',
    grpFill: isDark ? '#1e3a5f22' : '#74b9ff22',
    oriFill: isDark ? '#1a4731' : '#55efc4',  oriBorder: isDark ? '#00b894' : '#00b894',
    oriGrp:  isDark ? '#45475a' : '#dfe6e9',
    redFill: isDark ? '#5e2a2a' : '#ff7675',  redBorder: isDark ? '#d63031' : '#d63031',
    purFill: isDark ? '#3e2a5e' : '#a29bfe',  purBorder: isDark ? '#6c5ce7' : '#6c5ce7',
    ylwFill: isDark ? '#5e4e1a' : '#ffeaa7',  ylwBorder: isDark ? '#fdcb6e' : '#fdcb6e',
    dbFill:  isDark ? '#45475a' : '#dfe6e9',  dbBorder: isDark ? '#7f849c' : '#636e72',
    obsGrp:  isDark ? '#3e2a5e22' : '#a29bfe22',
    ciGrp:   isDark ? '#5e2a2a22' : '#ff767522',
    infGrp:  isDark ? '#5e4e1a22' : '#ffeaa722',
    hintFg:  isDark ? '#585b70' : '#b2bec3',
  };

  function renderDiagram(containerId, graphDef) {
    var container = document.getElementById(containerId);
    if (!container) return;

    var nodes = graphDef.nodes;
    var edges = graphDef.edges;
    var groups = graphDef.groups || [];

    /* ── Dagre layout ── */
    var g = new dagre.graphlib.Graph({ compound: true });
    g.setGraph({
      rankdir: graphDef.rankdir || 'TB',
      ranksep: graphDef.ranksep || 60,
      nodesep: graphDef.nodesep || 30,
      edgesep: 20,
      marginx: 20,
      marginy: 20
    });
    g.setDefaultEdgeLabel(function () { return {}; });

    groups.forEach(function (grp) {
      g.setNode(grp.id, { label: grp.label || '', clusterLabelPos: 'top', style: '', paddingTop: 30, paddingBottom: 15, paddingLeft: 15, paddingRight: 15 });
    });

    nodes.forEach(function (n) {
      // Estimate text width
      var tw = (n.label || '').length * 7.5 + 24;
      if (n.sublabel) tw = Math.max(tw, n.sublabel.length * 6 + 24);
      var th = n.sublabel ? 52 : 36;
      g.setNode(n.id, { label: n.label, width: Math.max(tw, 80), height: th });
      if (n.parent) g.setParent(n.id, n.parent);
    });

    edges.forEach(function (e, i) {
      g.setEdge(e.from, e.to, { id: 'e' + i });
    });

    dagre.layout(g);

    /* ── SVG setup with D3 zoom ── */
    var rect = container.getBoundingClientRect();
    var W = rect.width || 800, H = rect.height || 500;

    var svg = d3.select(container).append('svg')
      .attr('width', '100%').attr('height', '100%')
      .style('display', 'block').style('cursor', 'grab');

    var zoomG = svg.append('g');

    var zoomBehavior = d3.zoom()
      .scaleExtent([0.15, 6])
      .on('zoom', function (e) { zoomG.attr('transform', e.transform); });
    svg.call(zoomBehavior);

    // Cursor feedback
    svg.on('mousedown.cursor', function () { svg.style('cursor', 'grabbing'); });
    svg.on('mouseup.cursor mouseleave.cursor', function () { svg.style('cursor', 'grab'); });

    /* ── Draw groups (subgraph backgrounds) ── */
    groups.forEach(function (grp) {
      var gn = g.node(grp.id);
      if (!gn) return;
      // dagre gives us the node center + dimensions for compound nodes
      var children = g.children(grp.id) || [];
      if (!children.length) return;

      // Compute bounding box from children
      var minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
      children.forEach(function (cid) {
        var cn = g.node(cid);
        if (!cn) return;
        minX = Math.min(minX, cn.x - cn.width / 2);
        minY = Math.min(minY, cn.y - cn.height / 2);
        maxX = Math.max(maxX, cn.x + cn.width / 2);
        maxY = Math.max(maxY, cn.y + cn.height / 2);
      });

      var pad = 20, topPad = 35;
      zoomG.append('rect')
        .attr('x', minX - pad).attr('y', minY - topPad)
        .attr('width', maxX - minX + pad * 2).attr('height', maxY - minY + topPad + pad)
        .attr('rx', 8).attr('ry', 8)
        .attr('fill', grp.fill || COLORS.grpFill)
        .attr('stroke', grp.border || COLORS.svcBorder)
        .attr('stroke-width', 1.2);

      if (grp.label) {
        zoomG.append('text')
          .attr('x', (minX + maxX) / 2).attr('y', minY - topPad + 16)
          .attr('text-anchor', 'middle')
          .attr('font-size', 11).attr('font-weight', 600)
          .attr('fill', COLORS.fg).attr('opacity', 0.7)
          .attr('font-family', "'Inter',system-ui,sans-serif")
          .text(grp.label);
      }
    });

    /* ── Draw edges ── */
    var line = d3.line().curve(d3.curveBasis);

    g.edges().forEach(function (e) {
      var edge = g.edge(e);
      if (!edge.points) return;
      var pts = edge.points.map(function (p) { return [p.x, p.y]; });
      zoomG.append('path')
        .attr('d', line(pts))
        .attr('fill', 'none')
        .attr('stroke', COLORS.edge)
        .attr('stroke-width', 1.5)
        .attr('marker-end', 'url(#arrow-' + containerId + ')');
    });

    /* ── Arrow marker ── */
    svg.append('defs').append('marker')
      .attr('id', 'arrow-' + containerId)
      .attr('viewBox', '0 0 10 10')
      .attr('refX', 9).attr('refY', 5)
      .attr('markerWidth', 6).attr('markerHeight', 6)
      .attr('orient', 'auto-start-reverse')
      .append('path')
      .attr('d', 'M 0 0 L 10 5 L 0 10 z')
      .attr('fill', COLORS.edge);

    /* ── Draw nodes ── */
    nodes.forEach(function (n) {
      var nd = g.node(n.id);
      if (!nd) return;
      var fill = n.fill || COLORS.svcFill;
      var border = n.border || COLORS.svcBorder;
      var textColor = n.textColor || '#fff';
      var w = nd.width, h = nd.height;

      if (n.shape === 'cylinder') {
        // Database cylinder
        var ry = 6;
        var gNode = zoomG.append('g').attr('transform', 'translate(' + nd.x + ',' + nd.y + ')');
        gNode.append('path')
          .attr('d', 'M' + (-w/2) + ',' + (-h/2 + ry)
            + ' Q' + (-w/2) + ',' + (-h/2) + ' 0,' + (-h/2)
            + ' Q' + (w/2) + ',' + (-h/2) + ' ' + (w/2) + ',' + (-h/2 + ry)
            + ' L' + (w/2) + ',' + (h/2 - ry)
            + ' Q' + (w/2) + ',' + (h/2) + ' 0,' + (h/2)
            + ' Q' + (-w/2) + ',' + (h/2) + ' ' + (-w/2) + ',' + (h/2 - ry) + ' Z')
          .attr('fill', fill).attr('stroke', border).attr('stroke-width', 1.3);
        // Top ellipse
        gNode.append('ellipse')
          .attr('cx', 0).attr('cy', -h/2 + ry)
          .attr('rx', w/2).attr('ry', ry)
          .attr('fill', fill).attr('stroke', border).attr('stroke-width', 1.3);
        gNode.append('text')
          .attr('y', 4)
          .attr('text-anchor', 'middle').attr('dominant-baseline', 'central')
          .attr('font-size', 10).attr('font-weight', 500)
          .attr('fill', textColor)
          .attr('font-family', "'Inter',system-ui,sans-serif")
          .text(n.label);
      } else {
        // Rounded rectangle (default)
        var rx = n.rx != null ? n.rx : 5;
        zoomG.append('rect')
          .attr('x', nd.x - w/2).attr('y', nd.y - h/2)
          .attr('width', w).attr('height', h)
          .attr('rx', rx).attr('ry', rx)
          .attr('fill', fill).attr('stroke', border)
          .attr('stroke-width', n.strokeWidth || 1.3)
          .attr('stroke-dasharray', n.dashed ? '5 3' : null);

        zoomG.append('text')
          .attr('x', nd.x).attr('y', n.sublabel ? nd.y - 7 : nd.y)
          .attr('text-anchor', 'middle').attr('dominant-baseline', 'central')
          .attr('font-size', n.fontSize || 11).attr('font-weight', n.fontWeight || 600)
          .attr('fill', textColor)
          .attr('font-family', "'Inter',system-ui,sans-serif")
          .text(n.label);

        if (n.sublabel) {
          zoomG.append('text')
            .attr('x', nd.x).attr('y', nd.y + 9)
            .attr('text-anchor', 'middle').attr('dominant-baseline', 'central')
            .attr('font-size', 9).attr('font-weight', 400).attr('font-style', 'italic')
            .attr('fill', textColor).attr('opacity', 0.75)
            .attr('font-family', "'Inter',system-ui,sans-serif")
            .text(n.sublabel);
        }
      }
    });

    /* ── Hint text ── */
    var hint = document.createElement('div');
    hint.className = 'diagram-hint';
    hint.textContent = 'Scroll to zoom \u00b7 Drag to pan \u00b7 Double-click to reset';
    container.appendChild(hint);

    /* ── Fit to container ── */
    function fitToContainer() {
      var box = zoomG.node().getBBox();
      if (!box.width || !box.height) return;
      var pad = 25;
      var cr = container.getBoundingClientRect();
      var cw = cr.width, ch = cr.height;
      var s = Math.min((cw - pad * 2) / box.width, (ch - pad * 2) / box.height);
      var cx = box.x + box.width / 2;
      var cy = box.y + box.height / 2;
      return d3.zoomIdentity.translate(cw / 2 - cx * s, ch / 2 - cy * s).scale(s);
    }

    requestAnimationFrame(function () {
      var t = fitToContainer();
      if (t) svg.call(zoomBehavior.transform, t);
    });

    // Double-click reset
    svg.on('dblclick.zoom', null);
    svg.on('dblclick', function () {
      var t = fitToContainer();
      if (t) svg.transition().duration(400).ease(d3.easeCubicOut).call(zoomBehavior.transform, t);
    });
  }

  /* ═══════════════════════════════════════════════════════════
     DIAGRAM 1: Before Orion — Complex cloud deployment
     ═══════════════════════════════════════════════════════════ */
  renderDiagram('diagram-before', {
    rankdir: 'TB', ranksep: 50, nodesep: 25,
    groups: [
      { id: 'g_svc1', label: 'Pricing Service (Auto-Scaled)', fill: COLORS.grpFill, border: COLORS.svcBorder },
      { id: 'g_svc2', label: 'Fraud Service (Auto-Scaled)',   fill: COLORS.grpFill, border: COLORS.svcBorder },
      { id: 'g_svc3', label: 'Routing Service (Auto-Scaled)', fill: COLORS.grpFill, border: COLORS.svcBorder },
      { id: 'g_svc4', label: 'Notification Service (Auto-Scaled)', fill: COLORS.grpFill, border: COLORS.svcBorder },
      { id: 'g_obs',  label: 'Observability Stack',   fill: COLORS.obsGrp, border: COLORS.purBorder },
      { id: 'g_ci',   label: 'CI/CD per Service (x4)', fill: COLORS.ciGrp,  border: COLORS.redBorder },
      { id: 'g_inf',  label: 'Shared Platform Infrastructure', fill: COLORS.infGrp, border: COLORS.ylwBorder },
    ],
    nodes: [
      { id: 'cdn',  label: 'CDN / Edge Cache', fill: COLORS.gwFill, border: COLORS.gwBorder, textColor: COLORS.fg },
      { id: 'lb',   label: 'Load Balancer',    fill: COLORS.gwFill, border: COLORS.gwBorder, textColor: COLORS.fg },
      { id: 'gw',   label: 'API Gateway', sublabel: 'auth \u00b7 throttle \u00b7 routing', fill: COLORS.gwFill, border: COLORS.gwBorder, textColor: COLORS.fg },
      // Sidecar proxies
      { id: 'sm1', label: 'Mesh Proxy', fill: COLORS.gwFill, border: COLORS.gwBorder, textColor: COLORS.fg, parent: 'g_svc1' },
      { id: 'sm2', label: 'Mesh Proxy', fill: COLORS.gwFill, border: COLORS.gwBorder, textColor: COLORS.fg, parent: 'g_svc2' },
      { id: 'sm3', label: 'Mesh Proxy', fill: COLORS.gwFill, border: COLORS.gwBorder, textColor: COLORS.fg, parent: 'g_svc3' },
      { id: 'sm4', label: 'Mesh Proxy', fill: COLORS.gwFill, border: COLORS.gwBorder, textColor: COLORS.fg, parent: 'g_svc4' },
      // Containers
      { id: 'p1', label: 'Container',  fill: COLORS.svcFill, border: COLORS.svcBorder, parent: 'g_svc1' },
      { id: 'f1', label: 'Container',  fill: COLORS.svcFill, border: COLORS.svcBorder, parent: 'g_svc2' },
      { id: 'r1', label: 'Container',  fill: COLORS.svcFill, border: COLORS.svcBorder, parent: 'g_svc3' },
      { id: 'n1', label: 'Container',  fill: COLORS.svcFill, border: COLORS.svcBorder, parent: 'g_svc4' },
      // Sidecars per container
      { id: 'p1h', label: 'Health Check',  fill: COLORS.oriFill, border: COLORS.oriBorder, textColor: COLORS.fg, fontSize: 9, parent: 'g_svc1' },
      { id: 'p1l', label: 'Log Agent',     fill: COLORS.purFill, border: COLORS.purBorder, fontSize: 9, parent: 'g_svc1' },
      { id: 'p1m', label: 'Metrics Agent', fill: COLORS.purFill, border: COLORS.purBorder, fontSize: 9, parent: 'g_svc1' },
      { id: 'f1h', label: 'Health Check',  fill: COLORS.oriFill, border: COLORS.oriBorder, textColor: COLORS.fg, fontSize: 9, parent: 'g_svc2' },
      { id: 'f1l', label: 'Log Agent',     fill: COLORS.purFill, border: COLORS.purBorder, fontSize: 9, parent: 'g_svc2' },
      { id: 'f1m', label: 'Metrics Agent', fill: COLORS.purFill, border: COLORS.purBorder, fontSize: 9, parent: 'g_svc2' },
      { id: 'r1h', label: 'Health Check',  fill: COLORS.oriFill, border: COLORS.oriBorder, textColor: COLORS.fg, fontSize: 9, parent: 'g_svc3' },
      { id: 'r1l', label: 'Log Agent',     fill: COLORS.purFill, border: COLORS.purBorder, fontSize: 9, parent: 'g_svc3' },
      { id: 'r1m', label: 'Metrics Agent', fill: COLORS.purFill, border: COLORS.purBorder, fontSize: 9, parent: 'g_svc3' },
      { id: 'n1h', label: 'Health Check',  fill: COLORS.oriFill, border: COLORS.oriBorder, textColor: COLORS.fg, fontSize: 9, parent: 'g_svc4' },
      { id: 'n1l', label: 'Log Agent',     fill: COLORS.purFill, border: COLORS.purBorder, fontSize: 9, parent: 'g_svc4' },
      { id: 'n1m', label: 'Metrics Agent', fill: COLORS.purFill, border: COLORS.purBorder, fontSize: 9, parent: 'g_svc4' },
      // External datastores
      { id: 'db',    label: 'Database',      sublabel: 'Primary + Replica', shape: 'cylinder', fill: COLORS.dbFill, border: COLORS.dbBorder, textColor: COLORS.fg },
      { id: 'cache', label: 'Cache Cluster', shape: 'cylinder', fill: COLORS.dbFill, border: COLORS.dbBorder, textColor: COLORS.fg },
      { id: 'mq',    label: 'Message Queue', shape: 'cylinder', fill: COLORS.dbFill, border: COLORS.dbBorder, textColor: COLORS.fg },
      { id: 'smtp',  label: 'Email Service', shape: 'cylinder', fill: COLORS.dbFill, border: COLORS.dbBorder, textColor: COLORS.fg },
      // Observability
      { id: 'log',  label: 'Log Aggregator',  fill: COLORS.purFill, border: COLORS.purBorder, parent: 'g_obs' },
      { id: 'met',  label: 'Metrics Backend', fill: COLORS.purFill, border: COLORS.purBorder, parent: 'g_obs' },
      { id: 'trc',  label: 'Trace Collector', fill: COLORS.purFill, border: COLORS.purBorder, parent: 'g_obs' },
      { id: 'dash', label: 'Dashboards & Alerts', fill: COLORS.purFill, border: COLORS.purBorder, parent: 'g_obs' },
      // CI/CD
      { id: 'reg',    label: 'Container Registry', fill: COLORS.redFill, border: COLORS.redBorder, parent: 'g_ci' },
      { id: 'pipe',   label: 'Build Pipeline',     fill: COLORS.redFill, border: COLORS.redBorder, parent: 'g_ci' },
      { id: 'deploy', label: 'Deploy Controller',  fill: COLORS.redFill, border: COLORS.redBorder, parent: 'g_ci' },
      { id: 'canary', label: 'Canary / Rollout',   fill: COLORS.redFill, border: COLORS.redBorder, parent: 'g_ci' },
      // Infra
      { id: 'sr',   label: 'Service Registry',    fill: COLORS.ylwFill, border: COLORS.ylwBorder, textColor: COLORS.fg, parent: 'g_inf' },
      { id: 'sec',  label: 'Secret Manager',      fill: COLORS.ylwFill, border: COLORS.ylwBorder, textColor: COLORS.fg, parent: 'g_inf' },
      { id: 'cfg',  label: 'Config Server',       fill: COLORS.ylwFill, border: COLORS.ylwBorder, textColor: COLORS.fg, parent: 'g_inf' },
      { id: 'cert', label: 'Certificate Manager', fill: COLORS.ylwFill, border: COLORS.ylwBorder, textColor: COLORS.fg, parent: 'g_inf' },
    ],
    edges: [
      { from: 'cdn', to: 'lb' }, { from: 'lb', to: 'gw' },
      { from: 'gw', to: 'sm1' }, { from: 'gw', to: 'sm2' }, { from: 'gw', to: 'sm3' }, { from: 'gw', to: 'sm4' },
      { from: 'sm1', to: 'p1' }, { from: 'sm2', to: 'f1' }, { from: 'sm3', to: 'r1' }, { from: 'sm4', to: 'n1' },
      { from: 'p1', to: 'p1h' }, { from: 'p1', to: 'p1l' }, { from: 'p1', to: 'p1m' },
      { from: 'f1', to: 'f1h' }, { from: 'f1', to: 'f1l' }, { from: 'f1', to: 'f1m' },
      { from: 'r1', to: 'r1h' }, { from: 'r1', to: 'r1l' }, { from: 'r1', to: 'r1m' },
      { from: 'n1', to: 'n1h' }, { from: 'n1', to: 'n1l' }, { from: 'n1', to: 'n1m' },
      { from: 'p1', to: 'db' }, { from: 'f1', to: 'cache' }, { from: 'r1', to: 'mq' }, { from: 'n1', to: 'smtp' },
      { from: 'p1l', to: 'log' }, { from: 'f1l', to: 'log' }, { from: 'r1l', to: 'log' }, { from: 'n1l', to: 'log' },
      { from: 'p1m', to: 'met' }, { from: 'f1m', to: 'met' }, { from: 'r1m', to: 'met' }, { from: 'n1m', to: 'met' },
      { from: 'log', to: 'dash' }, { from: 'met', to: 'dash' }, { from: 'trc', to: 'dash' },
    ]
  });

  /* ═══════════════════════════════════════════════════════════
     DIAGRAM 2: After Orion — Clean consolidated deployment
     ═══════════════════════════════════════════════════════════ */
  renderDiagram('diagram-after', {
    rankdir: 'LR', ranksep: 80, nodesep: 20,
    groups: [
      { id: 'g_orion', label: 'Orion Runtime', fill: COLORS.oriGrp, border: COLORS.gwBorder },
    ],
    nodes: [
      { id: 'clients', label: 'Clients', fill: COLORS.gwFill, border: COLORS.gwBorder, textColor: COLORS.fg },
      { id: 'ch1', label: '/pricing',  fill: COLORS.oriFill, border: COLORS.oriBorder, textColor: COLORS.fg, parent: 'g_orion' },
      { id: 'ch2', label: '/fraud',    fill: COLORS.oriFill, border: COLORS.oriBorder, textColor: COLORS.fg, parent: 'g_orion' },
      { id: 'ch3', label: '/routing',  fill: COLORS.oriFill, border: COLORS.oriBorder, textColor: COLORS.fg, parent: 'g_orion' },
      { id: 'ch4', label: '/notify',   fill: COLORS.oriFill, border: COLORS.oriBorder, textColor: COLORS.fg, parent: 'g_orion' },
      { id: 'wf1', label: 'workflow', fill: COLORS.svcFill, border: COLORS.svcBorder, parent: 'g_orion' },
      { id: 'wf2', label: 'workflow', fill: COLORS.svcFill, border: COLORS.svcBorder, parent: 'g_orion' },
      { id: 'wf3', label: 'workflow', fill: COLORS.svcFill, border: COLORS.svcBorder, parent: 'g_orion' },
      { id: 'wf4', label: 'workflow', fill: COLORS.svcFill, border: COLORS.svcBorder, parent: 'g_orion' },
      { id: 'db',   label: 'Database', shape: 'cylinder', fill: COLORS.dbFill, border: COLORS.dbBorder, textColor: COLORS.fg },
      { id: 'rd',   label: 'Redis',    shape: 'cylinder', fill: COLORS.dbFill, border: COLORS.dbBorder, textColor: COLORS.fg },
      { id: 'kf',   label: 'Kafka',    shape: 'cylinder', fill: COLORS.dbFill, border: COLORS.dbBorder, textColor: COLORS.fg },
      { id: 'smtp', label: 'SMTP',     shape: 'cylinder', fill: COLORS.dbFill, border: COLORS.dbBorder, textColor: COLORS.fg },
    ],
    edges: [
      { from: 'clients', to: 'ch1' }, { from: 'clients', to: 'ch2' },
      { from: 'clients', to: 'ch3' }, { from: 'clients', to: 'ch4' },
      { from: 'ch1', to: 'wf1' }, { from: 'ch2', to: 'wf2' },
      { from: 'ch3', to: 'wf3' }, { from: 'ch4', to: 'wf4' },
      { from: 'wf1', to: 'db' }, { from: 'wf2', to: 'rd' },
      { from: 'wf3', to: 'kf' }, { from: 'wf4', to: 'smtp' },
    ]
  });

  /* ═══════════════════════════════════════════════════════════
     DIAGRAM 3: Deploy Anywhere
     ═══════════════════════════════════════════════════════════ */
  renderDiagram('diagram-deploy', {
    rankdir: 'LR', ranksep: 100, nodesep: 40,
    groups: [
      { id: 'g_standalone', label: 'Standalone', fill: COLORS.infGrp, border: COLORS.ylwBorder },
      { id: 'g_sidecar',   label: 'Sidecar',    fill: COLORS.grpFill, border: COLORS.svcBorder },
      { id: 'g_docker',    label: 'Docker',      fill: COLORS.obsGrp, border: COLORS.purBorder },
    ],
    nodes: [
      { id: 's1', label: './orion-server', sublabel: "That's it.", fill: COLORS.ylwFill, border: COLORS.ylwBorder, textColor: COLORS.fg, parent: 'g_standalone' },
      { id: 'app', label: 'Your App',  fill: COLORS.svcFill, border: COLORS.svcBorder, parent: 'g_sidecar' },
      { id: 'sc',  label: 'Orion',     fill: COLORS.ylwFill, border: COLORS.ylwBorder, textColor: COLORS.fg, parent: 'g_sidecar' },
      { id: 'd1', label: 'docker run', sublabel: 'orion:latest', fill: COLORS.purFill, border: COLORS.purBorder, parent: 'g_docker' },
    ],
    edges: [
      { from: 'app', to: 'sc' },
    ]
  });

  /* ═══════════════════════════════════════════════════════════
     DIAGRAM 4: Request Processing Flow
     ═══════════════════════════════════════════════════════════ */
  renderDiagram('diagram-flow', {
    rankdir: 'LR', ranksep: 55, nodesep: 20,
    groups: [],
    nodes: [
      { id: 'req',      label: 'HTTP Request',      fill: COLORS.svcFill, border: COLORS.svcBorder },
      { id: 'router',   label: 'Axum Router',       fill: COLORS.gwFill,  border: COLORS.gwBorder, textColor: COLORS.fg },
      { id: 'handler',  label: 'Data Route Handler', fill: COLORS.gwFill,  border: COLORS.gwBorder, textColor: COLORS.fg },
      { id: 'resolve',  label: 'Route Resolution',  sublabel: 'pattern match \u2192 channel', fill: COLORS.gwFill, border: COLORS.gwBorder, textColor: COLORS.fg },
      { id: 'registry', label: 'Channel Registry',  sublabel: 'dedup, rate limit, validation', fill: COLORS.redFill, border: COLORS.redBorder },
      { id: 'engine',   label: 'Engine',            sublabel: 'RwLock<Arc<Engine>>', fill: COLORS.purFill, border: COLORS.purBorder },
      { id: 'matcher',  label: 'Workflow Matcher',  sublabel: 'JSONLogic + rollout', fill: COLORS.gwFill, border: COLORS.gwBorder, textColor: COLORS.fg },
      { id: 'pipeline', label: 'Task Pipeline',     sublabel: 'ordered execution', fill: COLORS.gwFill, border: COLORS.gwBorder, textColor: COLORS.fg },
      { id: 'resp',     label: 'JSON Response',     fill: COLORS.oriFill, border: COLORS.oriBorder, textColor: COLORS.fg },
    ],
    edges: [
      { from: 'req', to: 'router' }, { from: 'router', to: 'handler' },
      { from: 'handler', to: 'resolve' }, { from: 'resolve', to: 'registry' },
      { from: 'registry', to: 'engine' }, { from: 'engine', to: 'matcher' },
      { from: 'matcher', to: 'pipeline' }, { from: 'pipeline', to: 'resp' },
    ]
  });

})();
</script>
