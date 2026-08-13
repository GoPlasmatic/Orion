// ── Orion mindmap renderer ──
// Renders ```orion-mindmap fenced JSON blocks as a two-sided, collapsible mindmap.
// Branches split left/right around a centre node; level-2 nodes start collapsed and
// expand on click; level-3 leaves navigate to the doc page their parent names.
//
// Deliberately dependency-free. The book's other diagrams carry React/ReactFlow/dagre
// (see diagram-widget.js), but a fixed-depth tree needs none of it: the layout is a
// stack-and-centre walk, and every colour comes from the --node-* custom properties
// custom.css already defines per `rf-<type>`, so themes work without any JS colour logic.
//
// Loaded lazily by diagram-loader.js, only on pages that contain a mindmap block.
(function () {
  "use strict";
  if (window.__orionMindmaps) return;
  window.__orionMindmaps = true;

  var SVGNS = "http://www.w3.org/2000/svg";

  // Per-level type: 0 centre, 1 branch, 2 area, 3 capability.
  var FONT = [15, 12, 10.5, 9.5];
  var PADX = [22, 12, 9, 7];
  var PADY = [11, 7, 5, 4];
  var SIB = 28;      // vertical slot per visible leaf
  var COL_GAP = 44;  // horizontal gap between depth columns
  var CAT_GAP = 40;  // vertical gap between branches on one side
  var CENTRE_GAP = 90;
  var FIT_PAD = 18;

  function el(name, attrs) {
    var n = document.createElementNS(SVGNS, name);
    for (var k in attrs) if (attrs[k] != null) n.setAttribute(k, attrs[k]);
    return n;
  }

  // ── Model ───────────────────────────────────────────────────────────────────
  // Each node: {id, name, level, type, link, kids, open}. `open` drives what renders.
  function toNode(raw, level, type, parentId, parentLink) {
    var name = typeof raw === "string" ? raw : raw.name;
    var id = parentId + "/" + name;
    var link = (typeof raw === "string" ? null : raw.link) || parentLink || null;
    var kids = (typeof raw === "string" ? null : raw.children) || null;
    return {
      id: id, name: name, level: level, type: type, link: link,
      // Areas (level 2) start closed, branches (level 1) start open — the initial
      // view is every characteristic and its areas, with capabilities one click away.
      open: level < 2,
      kids: kids ? kids.map(function (k) { return toNode(k, level + 1, type, id, link); }) : null
    };
  }

  // ── Layout ──────────────────────────────────────────────────────────────────
  // One side at a time: walk visible nodes, give every leaf a slot, centre each
  // parent on its children. Columns are placed from measured widths so a long
  // label can never overlap the next depth.
  function layoutSide(branches, sign, measure) {
    var placed = [];
    var cursor = 0;

    function walk(node, depth) {
      var y;
      var open = node.open && node.kids && node.kids.length;
      if (open) {
        var ys = node.kids.map(function (k) { return walk(k, depth + 1); });
        y = (ys[0] + ys[ys.length - 1]) / 2;
      } else {
        y = cursor;
        cursor += SIB;
      }
      var size = measure(node);
      placed.push({ node: node, depth: depth, y: y, w: size.w, h: size.h });
      return y;
    }

    branches.forEach(function (b, i) {
      if (i) cursor += CAT_GAP;
      walk(b, 0);
    });

    // Column centres from the widest node at each depth.
    var colW = [];
    placed.forEach(function (p) { colW[p.depth] = Math.max(colW[p.depth] || 0, p.w); });
    var centres = [];
    for (var d = 0; d < colW.length; d++) {
      centres[d] = d === 0
        ? CENTRE_GAP + colW[0] / 2
        : centres[d - 1] + colW[d - 1] / 2 + COL_GAP + colW[d] / 2;
    }
    placed.forEach(function (p) { p.x = sign * centres[p.depth]; });

    var ys = placed.map(function (p) { return p.y; });
    return { placed: placed, min: Math.min.apply(null, ys), max: Math.max.apply(null, ys) };
  }

  // ── One mindmap ─────────────────────────────────────────────────────────────
  function mount(container, spec) {
    var branches = (spec.branches || []).map(function (b, i) {
      return toNode(b, 1, b.type || "service", "b" + i, b.link || null);
    });
    var half = Math.ceil(branches.length / 2);
    var sides = [
      { list: branches.slice(0, half), sign: -1 },
      { list: branches.slice(half), sign: 1 }
    ];

    var svg = el("svg", { width: "100%", height: "100%" });
    var root = el("g", {});
    var gLinks = el("g", { class: "mm-links" });
    var gNodes = el("g", { class: "mm-nodes" });
    var gMeasure = el("g", { class: "mm-measure", visibility: "hidden" });
    root.appendChild(gLinks);
    root.appendChild(gNodes);
    svg.appendChild(root);
    svg.appendChild(gMeasure);
    container.appendChild(svg);

    var view = { k: 1, x: 0, y: 0 };
    var els = {};       // id -> <g>, reused so CSS transitions animate moves
    var widthCache = {};

    function measure(node) {
      if (!widthCache[node.id]) {
        var t = el("text", { class: "mm-label", "font-size": FONT[node.level] });
        t.textContent = node.name;
        gMeasure.appendChild(t);
        var tw = t.getComputedTextLength();
        gMeasure.removeChild(t);
        widthCache[node.id] = {
          w: Math.round(tw + PADX[node.level] * 2),
          h: Math.round(FONT[node.level] + PADY[node.level] * 2)
        };
      }
      return widthCache[node.id];
    }

    function applyView() {
      root.setAttribute("transform",
        "translate(" + view.x + "," + view.y + ") scale(" + view.k + ")");
    }

    function render() {
      var laid = [];
      sides.forEach(function (s) {
        if (!s.list.length) return;
        var r = layoutSide(s.list, s.sign, measure);
        var shift = -(r.min + r.max) / 2;
        r.placed.forEach(function (p) { p.y += shift; laid.push(p); });
      });

      // Centre node, sized like a branch.
      var centreName = spec.centre || spec.center || "Orion";
      var cSize = measure({ id: "__centre__", name: centreName, level: 0 });
      laid.push({
        node: { id: "__centre__", name: centreName, level: 0, type: spec.centreType || "accent", kids: null },
        depth: -1, x: 0, y: 0, w: cSize.w, h: cSize.h
      });

      var byId = {};
      laid.forEach(function (p) { byId[p.node.id] = p; });

      // Edges: parent's inner edge to child's outer edge, one cubic each.
      var paths = [];
      laid.forEach(function (p) {
        if (p.node.id === "__centre__") return;
        var parentId = p.depth === 0 ? "__centre__" : p.node.id.slice(0, p.node.id.lastIndexOf("/"));
        var q = byId[parentId];
        if (!q) return;
        var sign = p.x < 0 ? -1 : 1;
        var sx = q.node.id === "__centre__" ? sign * (q.w / 2) : q.x + sign * (q.w / 2);
        var tx = p.x - sign * (p.w / 2);
        var mx = (sx + tx) / 2;
        paths.push("M" + sx + "," + q.y + "C" + mx + "," + q.y + " " + mx + "," + p.y + " " + tx + "," + p.y);
      });
      gLinks.textContent = "";
      paths.forEach(function (d) { gLinks.appendChild(el("path", { class: "mm-link", d: d })); });

      // Nodes.
      var seen = {};
      laid.forEach(function (p) {
        var n = p.node;
        seen[n.id] = true;
        var g = els[n.id];
        if (!g) {
          g = el("g", {});
          g.appendChild(el("rect", { class: "mm-box" }));
          g.appendChild(el("text", { class: "mm-label" }));
          g.appendChild(el("text", { class: "mm-chevron" }));
          els[n.id] = g;
          gNodes.appendChild(g);
          g.addEventListener("click", function (ev) { activate(ev, n); });
          g.addEventListener("keydown", function (ev) {
            if (ev.key === "Enter" || ev.key === " ") { ev.preventDefault(); activate(ev, n); }
          });
        }

        var hasKids = !!(n.kids && n.kids.length);
        var interactive = hasKids || !!n.link;
        g.setAttribute("class", "mm-node mm-l" + n.level + " rf-" + (n.type || "service") +
          (interactive ? " is-interactive" : ""));
        g.setAttribute("transform", "translate(" + p.x + "," + p.y + ")");
        if (interactive) {
          g.setAttribute("tabindex", "0");
          g.setAttribute("role", hasKids ? "button" : "link");
          g.setAttribute("aria-label", hasKids
            ? n.name + (n.open ? " — collapse" : " — expand")
            : n.name);
          if (hasKids) g.setAttribute("aria-expanded", n.open ? "true" : "false");
        } else {
          g.removeAttribute("tabindex");
        }

        g.querySelector(".mm-box").setAttribute("x", -p.w / 2);
        g.querySelector(".mm-box").setAttribute("y", -p.h / 2);
        g.querySelector(".mm-box").setAttribute("width", p.w);
        g.querySelector(".mm-box").setAttribute("height", p.h);
        g.querySelector(".mm-box").setAttribute("rx", n.level >= 2 ? 4 : 7);

        var label = g.querySelector(".mm-label");
        label.setAttribute("font-size", FONT[n.level]);
        label.setAttribute("text-anchor", "middle");
        label.setAttribute("dominant-baseline", "central");
        label.textContent = n.name;

        // Chevron marks a node that has more behind it.
        var chev = g.querySelector(".mm-chevron");
        if (hasKids && !n.open) {
          var left = p.x < 0;
          chev.setAttribute("x", left ? -p.w / 2 - 5 : p.w / 2 + 5);
          chev.setAttribute("y", 0);
          chev.setAttribute("text-anchor", left ? "end" : "start");
          chev.setAttribute("dominant-baseline", "central");
          chev.textContent = left ? "‹" : "›";
        } else {
          chev.textContent = "";
        }
      });

      Object.keys(els).forEach(function (id) {
        if (!seen[id]) { gNodes.removeChild(els[id]); delete els[id]; }
      });
    }

    function activate(ev, n) {
      ev.stopPropagation();
      var newTab = ev.metaKey || ev.ctrlKey || ev.button === 1;
      // A node with children toggles, as the mindmap's whole point is drilling in —
      // except on cmd/ctrl-click, which is the one way to reach an area's own page
      // without first expanding it.
      if (n.kids && n.kids.length && !(newTab && n.link)) {
        n.open = !n.open;
        render();
      } else if (n.link) {
        if (newTab) window.open(n.link, "_blank");
        else window.location.href = n.link;
      }
    }

    function fit(animate) {
      var box = root.getBBox();
      if (!box.width || !box.height) return;
      var r = container.getBoundingClientRect();
      // Extra room at the foot so a full-height map never renders under the hint pill.
      var padBottom = FIT_PAD + 20;
      var usableH = r.height - FIT_PAD - padBottom;
      var k = Math.min((r.width - FIT_PAD * 2) / box.width, usableH / box.height);
      view.k = k;
      view.x = r.width / 2 - (box.x + box.width / 2) * k;
      view.y = FIT_PAD + usableH / 2 - (box.y + box.height / 2) * k;
      svg.classList.toggle("is-animating", !!animate);
      applyView();
      if (animate) setTimeout(function () { svg.classList.remove("is-animating"); }, 380);
    }

    // ── Pan / zoom ──
    svg.addEventListener("wheel", function (e) {
      e.preventDefault();
      var r = svg.getBoundingClientRect();
      var px = e.clientX - r.left, py = e.clientY - r.top;
      var next = Math.max(0.15, Math.min(3, view.k * Math.pow(0.999, e.deltaY)));
      view.x = px - (px - view.x) * (next / view.k);
      view.y = py - (py - view.y) * (next / view.k);
      view.k = next;
      applyView();
    }, { passive: false });

    var drag = null;
    svg.addEventListener("pointerdown", function (e) {
      if (e.target.closest(".mm-node")) return;
      drag = { x: e.clientX, y: e.clientY, vx: view.x, vy: view.y };
      svg.setPointerCapture(e.pointerId);
      svg.classList.add("is-dragging");
    });
    svg.addEventListener("pointermove", function (e) {
      if (!drag) return;
      view.x = drag.vx + (e.clientX - drag.x);
      view.y = drag.vy + (e.clientY - drag.y);
      applyView();
    });
    ["pointerup", "pointercancel"].forEach(function (t) {
      svg.addEventListener(t, function () { drag = null; svg.classList.remove("is-dragging"); });
    });
    svg.addEventListener("dblclick", function () { fit(true); });

    // ── Chrome ──
    var hint = document.createElement("div");
    hint.className = "diagram-hint";
    hint.textContent = "Click to expand · click a leaf to open its page · scroll to zoom · double-click to reset";
    container.appendChild(hint);

    var btn = document.createElement("button");
    btn.type = "button";
    btn.className = "mm-fs-btn";
    btn.textContent = "⤢ Fullscreen";
    btn.setAttribute("aria-label", "Toggle fullscreen");
    container.appendChild(btn);

    var saved = null, isFs = false;
    function eachNode(fn) {
      branches.forEach(function walk(n) {
        fn(n);
        if (n.kids) n.kids.forEach(walk);
      });
    }
    function setFullscreen(on) {
      if (on === isFs) return;
      isFs = on;
      if (on) {
        saved = {};
        eachNode(function (n) { saved[n.id] = n.open; n.open = true; });
      } else if (saved) {
        eachNode(function (n) { if (n.id in saved) n.open = saved[n.id]; });
      }
      container.classList.toggle("is-fullscreen", on);
      document.body.style.overflow = on ? "hidden" : "";
      btn.textContent = on ? "⤡ Exit" : "⤢ Fullscreen";
      render();
      requestAnimationFrame(function () { fit(false); });
    }
    btn.addEventListener("click", function (e) { e.stopPropagation(); setFullscreen(!isFs); });
    document.addEventListener("keydown", function (e) { if (e.key === "Escape") setFullscreen(false); });
    window.addEventListener("resize", function () { requestAnimationFrame(function () { fit(false); }); });

    render();
    requestAnimationFrame(function () { fit(false); });
  }

  function boot() {
    document.querySelectorAll("pre > code.language-orion-mindmap").forEach(function (code) {
      var spec;
      try {
        spec = JSON.parse(code.textContent);
      } catch (err) {
        console.error("[orion-mindmap] invalid JSON", err);
        return;
      }
      var pre = code.parentElement;
      var container = document.createElement("div");
      container.className = "orion-mindmap";
      container.style.height = (spec.height || 620) + "px";
      pre.parentNode.insertBefore(container, pre.nextSibling);
      pre.style.display = "none"; // the raw JSON stays as the no-JS fallback
      mount(container, spec);
    });
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot);
  else boot();
})();
