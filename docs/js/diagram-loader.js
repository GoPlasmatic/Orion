// ── Orion diagram lazy loader ──
// The only diagram script in additional-js, so it runs on every page but does nothing
// unless the page actually contains a diagram block. Two independent kinds:
//
//   `orion-diagram`  flow diagrams — needs the (heavy) React / ReactFlow / dagre stack,
//                    injected in dependency order so diagram-less pages never download
//                    ~450 KB.
//   `orion-mindmap`  the capability mindmap — dependency-free, one small widget.
//
// A page pays only for the kinds it actually uses.
(function () {
  "use strict";

  var hasDiagram = !!document.querySelector("pre > code.language-orion-diagram");
  var hasMindmap = !!document.querySelector("pre > code.language-orion-mindmap");
  if (!hasDiagram && !hasMindmap) return;

  // Vendored React/ReactFlow/dagre + the widgets live under src/diagram-assets so mdBook
  // copies them verbatim (stable, un-fingerprinted paths the loader can reference).
  var prefix = (typeof path_to_root !== "undefined" ? path_to_root : "") + "diagram-assets/";

  function js(src) {
    return new Promise(function (resolve, reject) {
      var s = document.createElement("script");
      s.src = prefix + src;
      s.async = false; // preserve execution order
      s.onload = resolve;
      s.onerror = function () { reject(new Error("failed to load " + src)); };
      document.head.appendChild(s);
    });
  }

  if (hasMindmap) {
    js("mindmap-widget.js")
      .catch(function (e) { console.error("[orion-mindmap] load failed", e); });
  }

  if (hasDiagram) {
    var link = document.createElement("link");
    link.rel = "stylesheet";
    link.href = prefix + "reactflow.css";
    document.head.appendChild(link);

    // react → react-dom (needs React) → reactflow + dagre (parallel) → widget
    js("react.production.min.js")
      .then(function () { return js("react-dom.production.min.js"); })
      .then(function () { return Promise.all([js("reactflow.umd.min.js"), js("dagre.min.js")]); })
      .then(function () { return js("diagram-widget.js"); })
      .catch(function (e) { console.error("[orion-diagram] load failed", e); });
  }
})();
