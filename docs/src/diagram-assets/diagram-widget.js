// ── Orion ReactFlow diagram renderer ──
// Renders ```orion-diagram fenced JSON blocks as interactive ReactFlow diagrams.
// Pipeline: parse JSON → dagre layout (ReactFlow has no auto-layout) → ReactFlow render.
// Node face colors are themed via CSS (.rf-<type> under .navy/.light); edge/background
// colors are theme-derived in JS and re-rendered on theme toggle.
//
// Loaded lazily by diagram-loader.js after react / react-dom / reactflow / dagre globals.
(function () {
  "use strict";
  if (window.__orionDiagrams) return;
  window.__orionDiagrams = true;

  function ready(fn) {
    if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", fn);
    else fn();
  }

  ready(function () {
    var React = window.React, ReactDOM = window.ReactDOM, RF = window.ReactFlow, dagre = window.dagre;
    if (!React || !ReactDOM || !RF || !dagre) {
      console.warn("[orion-diagram] missing dependencies");
      return;
    }

    var h = React.createElement;
    var ReactFlowComp = RF.ReactFlow || RF.default;
    var Background = RF.Background, Controls = RF.Controls, MiniMap = RF.MiniMap;
    var Handle = RF.Handle, Position = RF.Position, MarkerType = RF.MarkerType;

    function isDark() {
      var c = document.documentElement.classList;
      return c.contains("navy") || c.contains("coal") || c.contains("ayu");
    }

    // Theme-derived colors for things that can't be driven by CSS (SVG attributes).
    function themeColors() {
      return isDark()
        ? { edge: "#3D6B7D", marker: "#5E8DA0", dot: "#1B3140" }
        : { edge: "#9CC2D0", marker: "#7FAFC0", dot: "#CBDDE6" };
    }

    // ── Shapes ──
    // Standard architecture shapes. `shape` is explicit or inferred from `type`.
    var SHAPES = { rectangle: 1, cylinder: 1, queue: 1, cloud: 1, hexagon: 1, actor: 1 };

    function resolveShape(n) {
      if (n.shape && SHAPES[n.shape]) return n.shape;
      return n.type === "datastore" ? "cylinder" : "rectangle";
    }

    // SVG silhouette drawn in the node's own pixel space (viewBox 0 0 w h, 100% sized) →
    // 1:1 with the node box, so no aspect distortion at any zoom.
    function shapeSvg(shape, w, ht) {
      var sx = 1, sy = 1, kids;
      if (shape === "cylinder") {
        var ry = Math.max(6, Math.min(ht * 0.16, 13));
        var body = "M" + sx + "," + ry + " L" + sx + "," + (ht - ry) +
          " A" + (w / 2 - sx) + "," + ry + " 0 0 0 " + (w - sx) + "," + (ht - ry) +
          " L" + (w - sx) + "," + ry + " Z";
        kids = [h("path", { key: "b", d: body }), h("ellipse", { key: "t", cx: w / 2, cy: ry, rx: w / 2 - sx, ry: ry })];
      } else if (shape === "queue") {
        var rx = Math.max(6, Math.min(w * 0.1, 15));
        var body2 = "M" + rx + "," + sy + " L" + (w - rx) + "," + sy +
          " A" + rx + "," + (ht / 2 - sy) + " 0 0 1 " + (w - rx) + "," + (ht - sy) +
          " L" + rx + "," + (ht - sy) + " Z";
        kids = [h("path", { key: "b", d: body2 }), h("ellipse", { key: "l", cx: rx, cy: ht / 2, rx: rx, ry: ht / 2 - sy })];
      } else if (shape === "hexagon") {
        var k = Math.max(10, Math.min(w * 0.12, 28));
        var pts = [[k, sy], [w - k, sy], [w - sx, ht / 2], [w - k, ht - sy], [k, ht - sy], [sx, ht / 2]]
          .map(function (p) { return p.join(","); }).join(" ");
        kids = [h("polygon", { key: "p", points: pts })];
      } else if (shape === "cloud") {
        var W = w, H = ht;
        var d = "M" + (0.2 * W) + "," + (0.97 * H) +
          " C" + (0.04 * W) + "," + (0.97 * H) + " " + (0.02 * W) + "," + (0.64 * H) + " " + (0.18 * W) + "," + (0.58 * H) +
          " C" + (0.18 * W) + "," + (0.32 * H) + " " + (0.44 * W) + "," + (0.28 * H) + " " + (0.52 * W) + "," + (0.46 * H) +
          " C" + (0.6 * W) + "," + (0.27 * H) + " " + (0.88 * W) + "," + (0.31 * H) + " " + (0.82 * W) + "," + (0.58 * H) +
          " C" + (0.99 * W) + "," + (0.62 * H) + " " + (0.97 * W) + "," + (0.97 * H) + " " + (0.8 * W) + "," + (0.97 * H) +
          " Z";
        kids = [h("path", { key: "c", d: d })];
      } else {
        return null;
      }
      return h("svg", { className: "rf-shape", viewBox: "0 0 " + w + " " + ht, width: "100%", height: "100%" }, kids);
    }

    function ActorFigure() {
      return h(
        "svg",
        { className: "rf-actor-fig", viewBox: "0 0 26 30", width: 26, height: 30 },
        h("circle", { cx: 13, cy: 6.5, r: 5 }),
        h("path", { d: "M13,12.5 C5.5,12.5 4.5,25 4.5,29.5 L21.5,29.5 C21.5,25 20.5,12.5 13,12.5 Z" })
      );
    }

    // ── Custom nodes ──
    function OrionNode(props) {
      var d = props.data;
      var lr = d.dir === "LR";
      var shape = d.shape || "rectangle";
      var subLines = d.sublabel ? String(d.sublabel).split("\n") : [];
      var text = h(
        "div",
        { className: "rf-node-text" },
        h("div", { className: "rf-node-label" }, d.label),
        subLines.length
          ? h("div", { className: "rf-node-sub" }, subLines.map(function (l, i) { return h("div", { key: i }, l); }))
          : null
      );
      var bg = shape === "actor" ? h(ActorFigure) : (shape === "rectangle" ? null : shapeSvg(shape, d.w, d.h));
      return h(
        "div",
        { className: "rf-node rf-" + (d.type || "service") + " rf-shape-" + shape },
        h(Handle, { type: "target", position: lr ? Position.Left : Position.Top, isConnectable: false }),
        bg,
        text,
        h(Handle, { type: "source", position: lr ? Position.Right : Position.Bottom, isConnectable: false })
      );
    }

    function OrionGroup(props) {
      return h(
        "div",
        { className: "rf-group" },
        props.data.label ? h("div", { className: "rf-group-label" }, props.data.label) : null
      );
    }

    // Stable node-type map (ReactFlow warns on unstable references).
    var NODE_TYPES = { orion: OrionNode, orionGroup: OrionGroup };

    // ── Sizing + layout ──
    function nodeSize(n) {
      var shape = resolveShape(n);
      var label = String(n.label == null ? "" : n.label);
      var lines = n.sublabel ? String(n.sublabel).split("\n") : [];
      var subMax = lines.reduce(function (m, l) { return Math.max(m, l.length); }, 0);
      var w = Math.max(112, Math.min(label.length * 8 + 32, 440), Math.min(subMax * 6.4 + 32, 440));
      var hh = 30 + (lines.length ? lines.length * 14 + 10 : 8);
      // Extra room so the shape geometry doesn't crowd the label.
      if (shape === "cylinder") hh += 18;
      else if (shape === "queue") w += 28;
      else if (shape === "hexagon") w += 32;
      else if (shape === "cloud") { w = w * 1.22 + 12; hh += 20; }
      else if (shape === "actor") { w = Math.max(78, label.length * 7 + 22); hh = 40 + (lines.length ? lines.length * 13 : 12); }
      return { w: Math.round(w), h: Math.round(hh) };
    }

    function layout(spec, dir) {
      var g = new dagre.graphlib.Graph({ compound: true });
      g.setGraph({ rankdir: dir, ranksep: 58, nodesep: 30, edgesep: 18, marginx: 18, marginy: 18 });
      g.setDefaultEdgeLabel(function () { return {}; });

      (spec.groups || []).forEach(function (gr) { g.setNode(gr.id, { label: gr.label || "" }); });
      (spec.nodes || []).forEach(function (n) {
        var s = nodeSize(n);
        g.setNode(n.id, { width: s.w, height: s.h });
        if (n.group) g.setParent(n.id, n.group);
      });
      (spec.edges || []).forEach(function (e) { g.setEdge(e.from, e.to); });

      dagre.layout(g);
      return g;
    }

    // Build ReactFlow nodes/edges from the laid-out dagre graph.
    function build(spec, g, dir) {
      var colors = themeColors();
      var nodes = [];

      // Group backgrounds first (rendered behind, computed from member bbox).
      (spec.groups || []).forEach(function (gr) {
        var members = (spec.nodes || []).filter(function (n) { return n.group === gr.id; });
        if (!members.length) return;
        var minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
        members.forEach(function (n) {
          var nd = g.node(n.id);
          minX = Math.min(minX, nd.x - nd.width / 2);
          minY = Math.min(minY, nd.y - nd.height / 2);
          maxX = Math.max(maxX, nd.x + nd.width / 2);
          maxY = Math.max(maxY, nd.y + nd.height / 2);
        });
        var padX = 20, padTop = 32, padBot = 18;
        nodes.push({
          id: "grp_" + gr.id, type: "orionGroup",
          position: { x: minX - padX, y: minY - padTop },
          data: { label: gr.label },
          style: { width: maxX - minX + padX * 2, height: maxY - minY + padTop + padBot },
          selectable: false, draggable: false, connectable: false, zIndex: -1
        });
      });

      (spec.nodes || []).forEach(function (n) {
        var nd = g.node(n.id);
        nodes.push({
          id: n.id, type: "orion",
          position: { x: nd.x - nd.width / 2, y: nd.y - nd.height / 2 },
          data: {
            label: n.label, sublabel: n.sublabel, type: n.type || "service", dir: dir,
            shape: resolveShape(n), w: nd.width, h: nd.height
          },
          width: nd.width, height: nd.height,
          style: { width: nd.width, height: nd.height },
          selectable: false, draggable: false, connectable: false
        });
      });

      var edges = (spec.edges || []).map(function (e, i) {
        return {
          id: "e" + i, source: e.from, target: e.to,
          label: e.label, type: "smoothstep",
          className: e.style === "dashed" ? "rf-edge-dashed" : undefined,
          markerEnd: { type: MarkerType.ArrowClosed, width: 16, height: 16, color: colors.marker },
          style: { stroke: colors.edge, strokeWidth: 1.6 }
        };
      });

      return { nodes: nodes, edges: edges, height: (g.graph().height || 200) };
    }

    // ── Mount one diagram ──
    function mount(container, host, spec) {
      var dir = (spec.direction || "TB").toUpperCase();
      var g = layout(spec, dir);
      var state = { fullscreen: false, instance: null };
      var root = ReactDOM.createRoot(host);

      function render() {
        var built = build(spec, g, dir);
        root.render(
          h(
            ReactFlowComp,
            {
              nodes: built.nodes, edges: built.edges, nodeTypes: NODE_TYPES,
              fitView: true, fitViewOptions: { padding: 0.18 },
              nodesDraggable: false, nodesConnectable: false, elementsSelectable: false,
              zoomOnScroll: state.fullscreen, panOnScroll: false, zoomOnPinch: true,
              panOnDrag: true, preventScrolling: state.fullscreen,
              minZoom: 0.2, maxZoom: 2.5,
              onInit: function (inst) {
                state.instance = inst;
                requestAnimationFrame(function () { inst.fitView({ padding: 0.18 }); });
              }
            },
            h(Background, { gap: 18, size: 1, color: themeColors().dot }),
            h(Controls, { showInteractive: false })
          )
        );
      }

      function refit() {
        if (state.instance) requestAnimationFrame(function () { state.instance.fitView({ padding: 0.18 }); });
      }

      // Fullscreen toggle (reuses .mm-fs-btn / .is-fullscreen from the D3 diagrams).
      var btn = document.createElement("button");
      btn.type = "button";
      btn.className = "mm-fs-btn";
      btn.setAttribute("aria-label", "Toggle fullscreen");
      btn.textContent = "⤢ Fullscreen";
      container.appendChild(btn);

      var hint = document.createElement("div");
      hint.className = "diagram-hint";
      hint.textContent = "Pinch or use controls to zoom · Drag to pan";
      container.appendChild(hint);

      function toggleFs(on) {
        if (on === state.fullscreen) return;
        state.fullscreen = on;
        container.classList.toggle("is-fullscreen", on);
        document.body.style.overflow = on ? "hidden" : "";
        btn.textContent = on ? "⤢ Exit" : "⤢ Fullscreen";
        render();
        refit();
      }
      btn.addEventListener("click", function (e) { e.stopPropagation(); toggleFs(!state.fullscreen); });
      document.addEventListener("keydown", function (e) { if (e.key === "Escape") toggleFs(false); });
      window.addEventListener("resize", refit);

      render();
      return render; // expose for theme re-render
    }

    // ── Discover + render all diagram blocks ──
    var rerenders = [];
    var blocks = document.querySelectorAll("pre > code.language-orion-diagram");
    blocks.forEach(function (code) {
      var pre = code.parentElement;
      var spec;
      try {
        spec = JSON.parse(code.textContent);
      } catch (err) {
        console.error("[orion-diagram] invalid JSON", err);
        return;
      }
      var container = document.createElement("div");
      container.className = "orion-diagram";
      var natural = Math.max(240, Math.min((layout(spec, (spec.direction || "TB").toUpperCase()).graph().height || 240) + 56, 680));
      container.style.height = natural + "px";
      var host = document.createElement("div");
      host.className = "orion-diagram-rf";
      container.appendChild(host);

      pre.parentNode.insertBefore(container, pre.nextSibling);
      pre.style.display = "none"; // keep raw JSON as no-JS fallback

      rerenders.push(mount(container, host, spec));
    });

    // Re-render all diagrams on theme toggle (updates edge/marker/background colors).
    if (rerenders.length) {
      var last = isDark();
      new MutationObserver(function () {
        var now = isDark();
        if (now !== last) { last = now; rerenders.forEach(function (fn) { fn(); }); }
      }).observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
    }
  });
})();
