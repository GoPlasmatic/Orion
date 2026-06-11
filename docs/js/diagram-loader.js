// ── Orion diagram lazy loader ──
// The only diagram script in additional-js, so it runs on every page but does nothing
// unless the page actually contains an `orion-diagram` block. When it does, it injects
// the (heavy) React / ReactFlow / dagre stack + the widget, in dependency order, so
// diagram-less pages never download ~450 KB.
(function () {
  "use strict";
  if (!document.querySelector("pre > code.language-orion-diagram")) return;

  // Vendored React/ReactFlow/dagre + the widget live under src/diagram-assets so mdBook
  // copies them verbatim (stable, un-fingerprinted paths the loader can reference).
  var prefix = (typeof path_to_root !== "undefined" ? path_to_root : "") + "diagram-assets/";

  var link = document.createElement("link");
  link.rel = "stylesheet";
  link.href = prefix + "reactflow.css";
  document.head.appendChild(link);

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

  // react → react-dom (needs React) → reactflow + dagre (parallel) → widget
  js("react.production.min.js")
    .then(function () { return js("react-dom.production.min.js"); })
    .then(function () { return Promise.all([js("reactflow.umd.min.js"), js("dagre.min.js")]); })
    .then(function () { return js("diagram-widget.js"); })
    .catch(function (e) { console.error("[orion-diagram] load failed", e); });
})();
