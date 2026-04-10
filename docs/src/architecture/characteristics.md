# Architectural Characteristics

Orion provides production-grade capabilities across eight architectural dimensions. Each subcategory below links to its detailed documentation.

> **C** Creational · **S** Structural · **B** Behavioral
>
> *Click a node to expand its capabilities. Click any leaf node to jump to its documentation.*

<div id="mindmap" class="d3-diagram" style="height:600px"></div>

---

## Observability — S

| Area | Capabilities |
|------|-------------|
| [Structured Logging](../features/observability.md#structured-logging) | JSON & pretty-print formats · Configurable log levels · Per-request context · Per-crate filtering |
| [Prometheus Metrics](../features/observability.md#prometheus-metrics) | Request counters & error rates · Latency histograms · Circuit breaker metrics · Rate limit rejections |
| [Distributed Tracing](../features/observability.md#distributed-tracing) | W3C Trace Context · OpenTelemetry OTLP export · Configurable sampling rate · Per-task span tracking |
| [Health Monitoring](../features/observability.md#health-monitoring) | Component-level health checks · Automatic degradation · Request ID propagation · Kubernetes liveness & readiness probes |

## Resilience — S

| Area | Capabilities |
|------|-------------|
| [Circuit Breakers](../features/resilience.md#circuit-breakers) | Lock-free state machine · Per-connector isolation · Auto-recovery after cooldown · Admin API to inspect & reset |
| [Retry & Backoff](../features/resilience.md#retry-backoff) | Exponential backoff (capped 60 s) · Configurable max retries · Retryable error detection |
| [Timeouts](../features/resilience.md#timeouts) | Per-channel enforcement · Workflow execution limits · Per-connector query timeout |
| [Fault Tolerance](../features/resilience.md#fault-tolerance) | Graceful shutdown (SIGTERM/SIGINT) · Connection draining · Dead letter queue with retry · Panic recovery middleware |

## Security — B

| Area | Capabilities |
|------|-------------|
| [Secret Management](../features/security.md#secret-management) | Auto-masked API responses · Credential isolation via connectors |
| [Input Validation](../features/security.md#input-validation) | Per-channel JSONLogic rules · Payload size limits · Header & query param access |
| [Network Security](../features/security.md#network-security) | SSRF protection (private IP blocking) · TLS/HTTPS support · Security headers (CSP, X-Frame-Options) |
| [Access Control](../features/security.md#access-control) | Admin API authentication · Per-channel CORS enforcement · Origin allowlist |
| [Data Safety](../features/security.md#data-safety) | Parameterized SQL queries · Injection protection · URL validation |

## Scalability — C

| Area | Capabilities |
|------|-------------|
| [Rate Limiting](../features/scalability.md#rate-limiting) | Token bucket algorithm · Per-client keying via JSONLogic · Platform & per-channel limits |
| [Backpressure](../features/scalability.md#backpressure) | Semaphore concurrency limits · 503 load shedding · Per-channel configuration |
| [Async Processing](../features/scalability.md#async-processing) | Multi-worker trace queue · Bounded buffer channels · DLQ retry processor |
| [Horizontal Scaling](../features/scalability.md#horizontal-scaling) | Stateless instances · Channel include/exclude filters · Multi-database backends |

## Deployability — C

| Area | Capabilities |
|------|-------------|
| [Packaging](../features/deployability.md#packaging) | Single binary · SQLite, PostgreSQL, MySQL · Minimal footprint |
| [Containerization](../features/deployability.md#containerization) | Multi-stage Docker build · Non-root execution · Built-in health probes |
| [Configuration](../features/deployability.md#configuration) | TOML + env var overrides · Sensible defaults · Runtime configuration |
| [Distribution](../features/deployability.md#distribution) | Homebrew tap · Shell & PowerShell installers · Multi-platform binaries |

## Extensibility — S

| Area | Capabilities |
|------|-------------|
| [Connectors](../features/extensibility.md#connectors) | HTTP & Webhooks · Kafka pub/sub · Database (SQL) · Cache (Memory & Redis) · Storage (S3/GCS) · MongoDB (NoSQL) |
| [Custom Functions](../features/extensibility.md#custom-functions) | Async function handlers · Built-in function library · JSONLogic expressions |
| [Channel Protocols](../features/extensibility.md#channel-protocols) | REST with route matching (sync) · Simple HTTP (sync) · Kafka (async) |

## Availability — C

| Area | Capabilities |
|------|-------------|
| [Hot-Reload](../features/availability.md#hot-reload) | Zero-downtime engine swap · Channel registry rebuild · Kafka consumer restart |
| [Canary Rollouts](../features/availability.md#canary-rollouts) | Percentage-based traffic split · Gradual migration · Instant rollback |
| [Versioning](../features/availability.md#versioning) | Draft / Active / Archived lifecycle · Multi-version history · Workflow import & export |
| [Performance](../features/availability.md#performance) | Response caching · Request deduplication · Connection pool caching |

## Maintainability — B

| Area | Capabilities |
|------|-------------|
| [Admin APIs](../features/maintainability.md#admin-apis) | Full CRUD for all entities · Version management · Engine control · OpenAPI / Swagger UI |
| [CI/CD Integration](../features/maintainability.md#cicd-integration) | Bulk import & export · Pre-deploy validation · GitOps-friendly |
| [Testing](../features/maintainability.md#testing) | Dry-run execution · Workflow validation · Step-by-step traces |
| [Operations](../features/maintainability.md#operations) | Audit logging · Database backup & restore · Config validation CLI |

<script src="https://d3js.org/d3.v7.min.js"></script>
<script>
(function () {
  var container = document.getElementById('mindmap');
  if (!container) return;

  var isDark = document.documentElement.classList.contains('coal')
    || document.documentElement.classList.contains('navy')
    || document.documentElement.classList.contains('ayu');

  var TYPE_COLORS = { C: '#119FCD', S: '#4CBD97', B: '#FFD167' };

  /* ═══ DATA — inlined from original features.json with doc links ═══ */
  var categories = [
    {
      name: 'Observability', type: 'S',
      color: isDark ? '#102838' : '#D0EBF5', border: '#119FCD',
      children: [
        { name: 'Structured Logging', link: '../features/observability.html#structured-logging',
          children: ['JSON & pretty-print formats', 'Configurable log levels', 'Per-request context', 'Per-crate filtering'] },
        { name: 'Prometheus Metrics', link: '../features/observability.html#prometheus-metrics',
          children: ['Request counters & error rates', 'Latency histograms', 'Circuit breaker metrics', 'Rate limit rejections'] },
        { name: 'Distributed Tracing', link: '../features/observability.html#distributed-tracing',
          children: ['W3C Trace Context', 'OpenTelemetry OTLP export', 'Configurable sampling rate', 'Per-task span tracking'] },
        { name: 'Health Monitoring', link: '../features/observability.html#health-monitoring',
          children: ['Component-level health checks', 'Automatic degradation', 'Request ID propagation', 'Kubernetes probes'] }
      ]
    },
    {
      name: 'Resilience', type: 'S',
      color: isDark ? '#0F2238' : '#D5EDF8', border: '#7DD3FC',
      children: [
        { name: 'Circuit Breakers', link: '../features/resilience.html#circuit-breakers',
          children: ['Lock-free state machine', 'Per-connector isolation', 'Auto-recovery after cooldown', 'Admin API to inspect & reset'] },
        { name: 'Retry & Backoff', link: '../features/resilience.html#retry-backoff',
          children: ['Exponential backoff (capped 60s)', 'Configurable max retries', 'Retryable error detection'] },
        { name: 'Timeouts', link: '../features/resilience.html#timeouts',
          children: ['Per-channel enforcement', 'Workflow execution limits', 'Per-connector query timeout'] },
        { name: 'Fault Tolerance', link: '../features/resilience.html#fault-tolerance',
          children: ['Graceful shutdown (SIGTERM/SIGINT)', 'Connection draining', 'Dead letter queue with retry', 'Panic recovery middleware'] }
      ]
    },
    {
      name: 'Security', type: 'B',
      color: isDark ? '#251520' : '#F5D5DD', border: '#EF476F',
      children: [
        { name: 'Secret Management', link: '../features/security.html#secret-management',
          children: ['Auto-masked API responses', 'Credential isolation via connectors'] },
        { name: 'Input Validation', link: '../features/security.html#input-validation',
          children: ['Per-channel JSONLogic rules', 'Payload size limits', 'Header & query param access'] },
        { name: 'Network Security', link: '../features/security.html#network-security',
          children: ['SSRF protection (private IP blocking)', 'TLS/HTTPS support', 'Security headers'] },
        { name: 'Access Control', link: '../features/security.html#access-control',
          children: ['Admin API authentication', 'Per-channel CORS enforcement', 'Origin allowlist'] },
        { name: 'Data Safety', link: '../features/security.html#data-safety',
          children: ['Parameterized SQL queries', 'Injection protection', 'URL validation'] }
      ]
    },
    {
      name: 'Scalability', type: 'C',
      color: isDark ? '#0F2530' : '#CCE8F0', border: '#119FCD',
      children: [
        { name: 'Rate Limiting', link: '../features/scalability.html#rate-limiting',
          children: ['Token bucket algorithm', 'Per-client keying via JSONLogic', 'Platform & per-channel limits'] },
        { name: 'Backpressure', link: '../features/scalability.html#backpressure',
          children: ['Semaphore concurrency limits', '503 load shedding', 'Per-channel configuration'] },
        { name: 'Async Processing', link: '../features/scalability.html#async-processing',
          children: ['Multi-worker trace queue', 'Bounded buffer channels', 'DLQ retry processor'] },
        { name: 'Horizontal Scaling', link: '../features/scalability.html#horizontal-scaling',
          children: ['Stateless instances', 'Channel include/exclude filters', 'Multi-database backends'] }
      ]
    },
    {
      name: 'Deployability', type: 'C',
      color: isDark ? '#201E10' : '#F5EDD0', border: '#FFD167',
      children: [
        { name: 'Packaging', link: '../features/deployability.html#packaging',
          children: ['Single binary', 'SQLite, PostgreSQL, MySQL', 'Minimal footprint'] },
        { name: 'Containerization', link: '../features/deployability.html#containerization',
          children: ['Multi-stage Docker build', 'Non-root execution', 'Built-in health probes'] },
        { name: 'Configuration', link: '../features/deployability.html#configuration',
          children: ['TOML + env var overrides', 'Sensible defaults', 'Runtime configuration'] },
        { name: 'Distribution', link: '../features/deployability.html#distribution',
          children: ['Homebrew tap', 'Shell & PowerShell installers', 'Multi-platform binaries'] }
      ]
    },
    {
      name: 'Extensibility', type: 'S',
      color: isDark ? '#0F2822' : '#D0F0E5', border: '#4CBD97',
      children: [
        { name: 'Connectors', link: '../features/extensibility.html#connectors',
          children: ['HTTP & Webhooks', 'Kafka pub/sub', 'Database (SQL)', 'Cache (Memory & Redis)', 'Storage (S3/GCS)', 'MongoDB (NoSQL)'] },
        { name: 'Custom Functions', link: '../features/extensibility.html#custom-functions',
          children: ['Async function handlers', 'Built-in function library', 'JSONLogic expressions'] },
        { name: 'Channel Protocols', link: '../features/extensibility.html#channel-protocols',
          children: ['REST with route matching (sync)', 'Simple HTTP (sync)', 'Kafka (async)'] }
      ]
    },
    {
      name: 'Availability', type: 'C',
      color: isDark ? '#102520' : '#D0EDE5', border: '#4CBD97',
      children: [
        { name: 'Hot-Reload', link: '../features/availability.html#hot-reload',
          children: ['Zero-downtime engine swap', 'Channel registry rebuild', 'Kafka consumer restart'] },
        { name: 'Canary Rollouts', link: '../features/availability.html#canary-rollouts',
          children: ['Percentage-based traffic split', 'Gradual migration', 'Instant rollback'] },
        { name: 'Versioning', link: '../features/availability.html#versioning',
          children: ['Draft / Active / Archived lifecycle', 'Multi-version history', 'Workflow import & export'] },
        { name: 'Performance', link: '../features/availability.html#performance',
          children: ['Response caching', 'Request deduplication', 'Connection pool caching'] }
      ]
    },
    {
      name: 'Maintainability', type: 'B',
      color: isDark ? '#151E28' : '#D8E5ED', border: '#7FAFC0',
      children: [
        { name: 'Admin APIs', link: '../features/maintainability.html#admin-apis',
          children: ['Full CRUD for all entities', 'Version management', 'Engine control', 'OpenAPI / Swagger UI'] },
        { name: 'CI/CD Integration', link: '../features/maintainability.html#cicd-integration',
          children: ['Bulk import & export', 'Pre-deploy validation', 'GitOps-friendly'] },
        { name: 'Testing', link: '../features/maintainability.html#testing',
          children: ['Dry-run execution', 'Workflow validation', 'Step-by-step traces'] },
        { name: 'Operations', link: '../features/maintainability.html#operations',
          children: ['Audit logging', 'Database backup & restore', 'Config validation CLI'] }
      ]
    }
  ];

  /* ═══ BUILD TREE ═══ */
  function makeNode(cat) {
    return {
      _id: cat.name, name: cat.name, color: cat.color, border: cat.border,
      catType: cat.type, lvl: 1, _c: null,
      children: cat.children.map(function (sub) {
        return {
          _id: cat.name + '/' + sub.name, name: sub.name, link: sub.link,
          color: cat.color, border: cat.border, lvl: 2,
          children: null,
          _c: (sub.children || []).map(function (leaf) {
            return {
              _id: cat.name + '/' + sub.name + '/' + leaf, name: leaf,
              color: cat.color, border: cat.border, lvl: 3, link: sub.link
            };
          })
        };
      })
    };
  }

  var cats = categories.map(makeNode);
  var leftCats = cats.slice(0, 4);
  var rightCats = cats.slice(4);

  /* ═══ SVG ═══ */
  var rect = container.getBoundingClientRect();
  var W = rect.width || 800, H = rect.height || 600;
  var svg = d3.select(container).append('svg')
    .attr('width', '100%').attr('height', '100%')
    .style('display', 'block').style('cursor', 'grab');
  var g = svg.append('g');
  var zoomBeh = d3.zoom().scaleExtent([0.1, 3]).on('zoom', function (e) { g.attr('transform', e.transform); });
  svg.call(zoomBeh);
  svg.on('mousedown.cursor', function () { svg.style('cursor', 'grabbing'); });
  svg.on('mouseup.cursor mouseleave.cursor', function () { svg.style('cursor', 'grab'); });

  var defs = g.append('defs');
  [['shadow', 2, 4, 0.10], ['shadow-sm', 1, 2.5, 0.06]].forEach(function (s) {
    var f = defs.append('filter').attr('id', s[0])
      .attr('x', '-25%').attr('y', '-25%').attr('width', '150%').attr('height', '150%');
    f.append('feDropShadow').attr('dx', 0).attr('dy', s[1])
      .attr('stdDeviation', s[2]).attr('flood-color', 'rgba(0,0,0,' + s[3] + ')');
  });

  var gLinks = g.append('g');
  var gBadges = g.append('g');
  var gNodes = g.append('g');

  /* ═══ LAYOUT ═══ */
  var linkH = d3.linkHorizontal().x(function (d) { return d[0]; }).y(function (d) { return d[1]; });
  var SIB = 36, DEPTH = 185, CAT_GAP = 50;

  function computeAll() {
    var nodes = [], links = [], badges = [];
    nodes.push({ _id: '__center__', name: 'Orion', x: 0, y: 0, lvl: 0, parentId: null });

    function doSide(catArr, xSign) {
      var metas = catArr.map(function (cat) {
        var root = d3.hierarchy(cat, function (d) { return d.children; });
        d3.tree().nodeSize([SIB, DEPTH])(root);
        var ys = root.descendants().map(function (d) { return d.x; });
        return { cat: cat, root: root, minY: d3.min(ys), maxY: d3.max(ys), h: d3.max(ys) - d3.min(ys) };
      });

      var cursor = 0, offsets = [];
      metas.forEach(function (m) { offsets.push(cursor - m.minY); cursor += m.h + CAT_GAP; });
      var totalH = cursor - CAT_GAP;
      var yShift = -totalH / 2;

      metas.forEach(function (m, mi) {
        var yOff = offsets[mi] + yShift;
        m.root.descendants().forEach(function (d) {
          var data = d.data;
          var sx = xSign * (d.y + 160);
          var sy = d.x + yOff;
          var parentId = d.depth === 0 ? '__center__' : d.parent.data._id;
          var px = d.depth === 0 ? 0 : xSign * (d.parent.y + 160);
          var py = d.depth === 0 ? 0 : d.parent.x + yOff;

          nodes.push({
            _id: data._id, name: data.name, x: sx, y: sy,
            lvl: data.lvl, color: data.color, border: data.border,
            catType: data.catType, parentId: parentId, link: data.link,
            hasKids: !!(data.children || data._c),
            expanded: !!data.children, data: data
          });
          links.push({ _id: 'l:' + data._id, sourceId: parentId, sx: px, sy: py, tx: sx, ty: sy });

          if (data.lvl === 1 && data.catType)
            badges.push({ _id: 'b:' + data._id, type: data.catType, x: (px + sx) / 2, y: (py + sy) / 2 });
        });
      });
    }

    doSide(leftCats, -1);
    doSide(rightCats, 1);
    return { nodes: nodes, links: links, badges: badges };
  }

  /* ═══ RENDER ═══ */
  function render(animate) {
    var computed = computeAll();
    var nodes = computed.nodes, links = computed.links, badges = computed.badges;
    var dur = animate ? 400 : 0;
    var t = dur ? d3.transition().duration(dur).ease(d3.easeCubicOut) : null;
    function tr(s) { return t ? s.transition(t) : s; }

    // Links
    var lS = gLinks.selectAll('.mm-link').data(links, function (d) { return d._id; });
    tr(lS.exit()).attr('opacity', 0).remove();
    var lE = lS.enter().append('path').attr('class', 'mm-link').attr('opacity', 0)
      .attr('fill', 'none').attr('stroke', isDark ? '#3D6B7D' : '#7FAFC0')
      .attr('stroke-width', 1.8).attr('stroke-linecap', 'round')
      .attr('d', function () { return linkH({ source: [0, 0], target: [0, 0] }); });
    tr(lE.merge(lS))
      .attr('d', function (d) { return linkH({ source: [d.sx, d.sy], target: [d.tx, d.ty] }); })
      .attr('opacity', 0.5);

    // Badges
    var bS = gBadges.selectAll('.type-badge').data(badges, function (d) { return d._id; });
    bS.exit().remove();
    var bE = bS.enter().append('g').attr('class', 'type-badge');
    bE.append('circle'); bE.append('text').attr('text-anchor', 'middle').attr('dominant-baseline', 'central').attr('pointer-events', 'none');
    var bA = bE.merge(bS);
    bA.each(function (d) {
      var el = d3.select(this), col = TYPE_COLORS[d.type] || '#999';
      el.select('circle').attr('r', 10).attr('fill', isDark ? '#07111A' : '#F4F8FB').attr('stroke', col).attr('stroke-width', 1.5).attr('stroke-dasharray', '3 2');
      el.select('text').attr('font-size', 8).attr('font-weight', 700).attr('fill', col).attr('font-family', "'Inter',system-ui,sans-serif").text(d.type);
    });
    tr(bA).attr('transform', function (d) { return 'translate(' + d.x + ',' + d.y + ')'; }).attr('opacity', 1);

    // Nodes
    var nS = gNodes.selectAll('.mm-node').data(nodes, function (d) { return d._id; });
    tr(nS.exit()).attr('opacity', 0).remove();
    var nE = nS.enter().append('g').attr('class', 'mm-node').attr('opacity', 0)
      .attr('transform', 'translate(0,0)');
    nE.append('rect').attr('class', 'node-bg');
    nE.append('text').attr('text-anchor', 'middle').attr('dominant-baseline', 'central');
    nE.append('text').attr('class', 'expand-hint').attr('dominant-baseline', 'central').attr('pointer-events', 'none');
    var nA = nE.merge(nS);

    nA.each(function (d) {
      var el = d3.select(this);
      var L = d.lvl;
      var fs = [15, 12, 10.5, 9.5][L];
      var fw = [700, 700, 600, 500][L];
      var tc = L === 0 ? '#ECF4F8' : (isDark ? '#ECF4F8' : '#07111A');
      var txt = el.select('text:not(.expand-hint)').attr('font-size', fs).attr('font-weight', fw).attr('fill', tc)
        .attr('font-family', "'Inter',system-ui,sans-serif").text(d.name);
      var tw = txt.node().getComputedTextLength();
      var px = [26, 12, 9, 7][L], py = [11, 7, 5, 4][L];
      var rw = tw + px * 2, rh = fs + py * 2, rx = [8, 5, 4, 4][L];

      function blendWhite(hex, amount) {
        var c = d3.color(hex);
        return d3.rgb(
          Math.round(c.r + (255 - c.r) * amount),
          Math.round(c.g + (255 - c.g) * amount),
          Math.round(c.b + (255 - c.b) * amount)
        ) + '';
      }
      function blendDark(hex, amount) {
        var c = d3.color(hex);
        return d3.rgb(
          Math.round(c.r * (1 - amount)),
          Math.round(c.g * (1 - amount)),
          Math.round(c.b * (1 - amount))
        ) + '';
      }

      var fill, stroke;
      if (L === 0) { fill = '#119FCD'; stroke = '#0D7FA3'; }
      else if (L === 1) { fill = d.color; stroke = d.border; }
      else if (L === 2) { fill = isDark ? blendDark(d.color, 0.3) : blendWhite(d.color, 0.6); stroke = d.border; }
      else { fill = isDark ? blendDark(d.color, 0.5) : blendWhite(d.color, 0.78); stroke = d.border; }

      el.select('rect').attr('x', -rw / 2).attr('y', -rh / 2).attr('width', rw).attr('height', rh)
        .attr('rx', rx).attr('ry', rx).attr('fill', fill).attr('stroke', stroke)
        .attr('stroke-width', L === 0 ? 2.5 : L === 1 ? 1.8 : 1.2)
        .attr('stroke-dasharray', L === 3 ? '4 3' : null)
        .attr('filter', L <= 1 ? 'url(#shadow)' : L === 2 ? 'url(#shadow-sm)' : null);

      // Chevron hint for collapsed nodes
      var hint = el.select('.expand-hint');
      var collapsed = d.hasKids && !d.expanded;
      if (collapsed && L > 0) {
        var isLeft = d.x < 0;
        hint.attr('x', isLeft ? -rw / 2 - 4 : rw / 2 + 4).attr('y', 1)
          .attr('text-anchor', isLeft ? 'end' : 'start')
          .attr('font-size', 12).attr('font-weight', 600)
          .attr('fill', d.border || (isDark ? '#3D6B7D' : '#7FAFC0'))
          .attr('font-family', "'Inter',system-ui,sans-serif")
          .text(isLeft ? '\u2039' : '\u203a').attr('opacity', 0.7);
      } else {
        hint.attr('opacity', 0).text('');
      }

      // Cursor style for clickable nodes
      el.style('cursor', (d.hasKids || d.link) ? 'pointer' : 'default');
    });

    tr(nA).attr('transform', function (d) { return 'translate(' + d.x + ',' + d.y + ')'; }).attr('opacity', 1);

    // Z-order
    gLinks.each(function () { g.node().insertBefore(this, g.node().firstChild); });
    gBadges.raise();
    gNodes.raise();

    // Click handlers — expand/collapse or navigate to link
    nA.on('click', null);
    nA.filter(function (d) { return d.hasKids; }).on('click', function (event, d) {
      event.stopPropagation();
      var data = d.data;
      if (data.children) { data._c = data.children; data.children = null; }
      else { data.children = data._c; data._c = null; }
      render(true);
    });
    nA.filter(function (d) { return d.link && !d.hasKids; }).on('click', function (event, d) {
      event.stopPropagation();
      window.location.href = d.link;
    });
    // Also allow middle-click / ctrl-click to open link in new tab for lvl 2 nodes with links
    nA.filter(function (d) { return d.link; }).on('auxclick', function (event, d) {
      if (event.button === 1) window.open(d.link, '_blank');
    });
  }

  /* ═══ FIT ═══ */
  function fitToWidth() {
    var box = g.node().getBBox();
    if (!box.width || !box.height) return;
    var pad = 30;
    var cr = container.getBoundingClientRect();
    var cw = cr.width, ch = cr.height;
    var s = Math.min((cw - pad * 2) / box.width, (ch - pad * 2) / box.height);
    var cx = box.x + box.width / 2;
    var cy = box.y + box.height / 2;
    svg.call(zoomBeh.transform, d3.zoomIdentity.translate(cw / 2 - cx * s, ch / 2 - cy * s).scale(s));
  }

  render(false);
  requestAnimationFrame(fitToWidth);

  svg.on('dblclick.zoom', null);
  svg.on('dblclick', function () {
    var box = g.node().getBBox();
    if (!box.width || !box.height) return;
    var pad = 30;
    var cr = container.getBoundingClientRect();
    var cw = cr.width, ch = cr.height;
    var s = Math.min((cw - pad * 2) / box.width, (ch - pad * 2) / box.height);
    var cx = box.x + box.width / 2;
    var cy = box.y + box.height / 2;
    svg.transition().duration(400).ease(d3.easeCubicOut)
      .call(zoomBeh.transform, d3.zoomIdentity.translate(cw / 2 - cx * s, ch / 2 - cy * s).scale(s));
  });

  /* ── Hint ── */
  var hint = document.createElement('div');
  hint.className = 'diagram-hint';
  hint.textContent = 'Scroll to zoom \u00b7 Drag to pan \u00b7 Click to expand \u00b7 Double-click to reset';
  container.appendChild(hint);

})();
</script>
