// ── Inject Plasmatic logo into sidebar ──
(function () {
  function injectLogo() {
    var scrollbox =
      document.querySelector("mdbook-sidebar-scrollbox") ||
      document.querySelector(".sidebar-scrollbox");
    if (!scrollbox || scrollbox.querySelector(".sidebar-logo")) return;

    // mdBook injects `const path_to_root` in each page (e.g. "../" for nested pages, "" for root)
    var prefix = typeof path_to_root !== "undefined" ? path_to_root : "";

    var wrapper = document.createElement("a");
    wrapper.href = prefix + "introduction.html";
    wrapper.className = "sidebar-logo";

    var img = document.createElement("img");
    img.src = prefix + "images/plasmatic-logo.png";
    img.alt = "Plasmatic";

    var label = document.createElement("span");
    label.textContent = "Plasmatic";

    wrapper.appendChild(img);
    wrapper.appendChild(label);
    scrollbox.insertBefore(wrapper, scrollbox.firstChild);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", injectLogo);
  } else {
    injectLogo();
  }
})();

// ── Icon sprite ──
// One hidden <svg> of <symbol>s, injected once, referenced everywhere as
// <svg class="orion-ico"><use href="#i-name"></use></svg> via window.orionIcon.
// The glyphs used to be built as SVG strings and assigned with innerHTML at
// each site, which is fine at one use per glyph and wasteful at the hundreds
// per page the table marks will reach.
//
// They are drawn here rather than pulled from an icon font: a viewBox and two
// strokes each, no third-party asset to vendor, license or keep in sync
// (js/VENDOR.md tracks the ones that are). Every symbol carries the same
// presentation attributes — no fill, `currentColor` stroke at 1.7, round caps —
// so an instance inherits its colour from whatever it sits in.
//
// This sprite is for marks the SCRIPT inserts. A mark drawn by a CSS
// pseudo-element cannot hold a <use>, so those live in plasmatic.css §1 as
// `--p-icon-*` data URIs applied through `mask-image`. Same geometry, two
// delivery mechanisms; reach for whichever the insertion point allows.
(function () {
  var GLYPHS = {
    // Open book — the introduction.
    book:
      '<path d="M12 6.5c-1.6-1.4-3.6-2-6-2-.9 0-1.7.1-2.4.2a.7.7 0 0 0-.6.7v11.9c0 .4.4.8.9.7.6-.1 1.3-.1 2.1-.1 2.4 0 4.4.6 6 2 1.6-1.4 3.6-2 6-2 .8 0 1.5 0 2.1.1.5.1.9-.3.9-.7V5.4a.7.7 0 0 0-.6-.7c-.7-.1-1.5-.2-2.4-.2-2.4 0-4.4.6-6 2Z"/>' +
      '<path d="M12 6.5V19"/>',
    // Question mark in a circle — "Is Orion Right for You?".
    help:
      '<circle cx="12" cy="12" r="9"/>' +
      '<path d="M9.6 9.3a2.5 2.5 0 1 1 3.4 2.5c-.7.3-1 .9-1 1.6v.4"/>' +
      '<path d="M12 17h.01"/>',
    // Stacked layers — the characteristics.
    layers:
      '<path d="m12 3 9 5-9 5-9-5 9-5Z"/>' +
      '<path d="m3.5 12.5 8.5 4.7 8.5-4.7"/>' +
      '<path d="m3.5 16.5 8.5 4.7 8.5-4.7"/>',
    // Arrows pointing opposite ways — Compare.
    compare:
      '<path d="M4 9h13"/><path d="m14 6 3 3-3 3"/>' +
      '<path d="M20 15H7"/><path d="m10 12-3 3 3 3"/>',
    // Play button — Get Started.
    start: '<circle cx="12" cy="12" r="9"/><path d="m10 8.5 6 3.5-6 3.5z"/>',
    // Cube — Concepts.
    concepts:
      '<path d="m12 2.8 8 4.6v9.2l-8 4.6-8-4.6V7.4z"/>' +
      '<path d="m4.3 7.6 7.7 4.4 7.7-4.4"/><path d="M12 21v-9"/>',
    // Sparkles — Build with AI.
    ai:
      '<path d="m12 3.5 1.9 5.3 5.3 1.9-5.3 1.9-1.9 5.3-1.9-5.3-5.3-1.9 5.3-1.9z"/>' +
      '<path d="m18.5 16.5.7 1.8 1.8.7-1.8.7-.7 1.8-.7-1.8-1.8-.7 1.8-.7z"/>',
    // Angle brackets — Build.
    build:
      '<path d="m8.5 8.5-4 3.5 4 3.5"/><path d="m15.5 8.5 4 3.5-4 3.5"/>' +
      '<path d="m13.5 5-3 14"/>',
    // Folded map — Guides.
    guides:
      '<path d="m9 4.5-5.5 2.6v12.4L9 16.9l6 2.6 5.5-2.6V4.5L15 7.1z"/>' +
      '<path d="M9 4.5v12.4"/><path d="M15 7.1v12.4"/>',
    // Two server bays — Operate.
    operate:
      '<rect x="3.2" y="4.2" width="17.6" height="6" rx="1.6"/>' +
      '<rect x="3.2" y="13.8" width="17.6" height="6" rx="1.6"/>' +
      '<path d="M7 7.2h.01"/><path d="M7 16.8h.01"/>',
    // Document — Reference.
    reference:
      '<path d="M13.5 3.5H7A1.5 1.5 0 0 0 5.5 5v14A1.5 1.5 0 0 0 7 20.5h10a1.5 1.5 0 0 0 1.5-1.5V8.5z"/>' +
      '<path d="M13.5 3.5v5h5"/><path d="M9 13h6"/><path d="M9 16.5h4"/>',

    // ── Connector types ──
    // The five values the `type` field takes. Used in the types table on
    // concepts/connectors.md; `kafka` doubles as the stream ingress below.
    "type-http":
      '<circle cx="12" cy="12" r="8.4"/><path d="M3.6 12h16.8"/>' +
      '<path d="M12 3.6c2.2 2.3 3.4 5.3 3.4 8.4s-1.2 6.1-3.4 8.4c-2.2-2.3-3.4-5.3-3.4-8.4S9.8 5.9 12 3.6Z"/>',
    "type-db":
      '<ellipse cx="12" cy="6.4" rx="7.4" ry="2.9"/>' +
      '<path d="M4.6 6.4v11.2c0 1.6 3.3 2.9 7.4 2.9s7.4-1.3 7.4-2.9V6.4"/>' +
      '<path d="M4.6 12c0 1.6 3.3 2.9 7.4 2.9s7.4-1.3 7.4-2.9"/>',
    "type-cache": '<path d="M13.2 3.2 5.6 13.6h5.9L10.8 20.8l7.6-10.4h-5.9z"/>',
    "type-es":
      '<path d="M13.4 3.4H7A1.5 1.5 0 0 0 5.5 4.9v14.2A1.5 1.5 0 0 0 7 20.6h5"/>' +
      '<path d="M13.4 3.4v5h5"/><circle cx="16.6" cy="15.6" r="3.1"/>' +
      '<path d="m18.9 17.9 2.1 2.1"/>',
    "type-kafka":
      '<path d="M3.6 8.6c3-2.6 6-2.6 9 0s6 2.6 9 0"/>' +
      '<path d="M3.6 15.4c3-2.6 6-2.6 9 0s6 2.6 9 0"/>',

    // ── Ingress kinds ──
    // The four ways a request reaches a channel, for the guards matrix
    // headers. `ingress-kafka` is type-kafka reused — one topic, one glyph.
    "ingress-sync": '<circle cx="12" cy="12" r="8.4"/><path d="M8 12h7"/><path d="m12.4 9 3 3-3 3"/>',
    "ingress-async": '<circle cx="12" cy="12" r="8.4"/><path d="M12 7.2V12l3.2 1.9"/>',
    "ingress-call":
      '<path d="M10.2 13.8a3.6 3.6 0 0 0 5.1 0l2.8-2.8a3.6 3.6 0 0 0-5.1-5.1l-1.3 1.3"/>' +
      '<path d="M13.8 10.2a3.6 3.6 0 0 0-5.1 0l-2.8 2.8a3.6 3.6 0 0 0 5.1 5.1l1.3-1.3"/>',

    // Chain link — the per-row permalink on a reference table.
    anchor:
      '<path d="M10.2 13.8a3.6 3.6 0 0 0 5.1 0l2.8-2.8a3.6 3.6 0 0 0-5.1-5.1l-1.3 1.3"/>' +
      '<path d="M13.8 10.2a3.6 3.6 0 0 0-5.1 0l-2.8 2.8a3.6 3.6 0 0 0 5.1 5.1l1.3-1.3"/>',

    // ── Table marks ──
    // Yes in a matrix or a Required column.
    check: '<path d="m4.5 12.5 5 5 10-11"/>',
    // No. A dash, not a cross: channel-config states that every No in the
    // guards matrix is deliberate — those guards are not offered on that
    // ingress by design. A cross reads as a failure; a dash reads as
    // not-applicable, which is what the sentence already says.
    dash: '<path d="M6 12h12"/>',
  };

  var SVG_NS = "http://www.w3.org/2000/svg";

  function build() {
    if (!document.body || document.getElementById("orion-sprite")) return;
    var symbols = [];
    for (var name in GLYPHS) {
      if (!Object.prototype.hasOwnProperty.call(GLYPHS, name)) continue;
      // No stroke-width here, deliberately. A presentation attribute on the
      // <symbol> is a specified value, and a specified value beats an
      // inherited one — so a CSS rule on the instance could never override
      // it. `stroke-width` is an inherited property, so leaving it off lets
      // plasmatic.css set 1.7 as the house default on .orion-ico and a
      // heavier weight on the small marks. `stroke="currentColor"` can stay:
      // it resolves against the instance's own inherited `color`.
      symbols.push(
        '<symbol id="i-' +
          name +
          '" viewBox="0 0 24 24" fill="none" stroke="currentColor" ' +
          'stroke-linecap="round" stroke-linejoin="round">' +
          GLYPHS[name] +
          "</symbol>",
      );
    }
    var host = document.createElement("div");
    host.id = "orion-sprite";
    host.setAttribute("aria-hidden", "true");
    host.innerHTML =
      '<svg xmlns="' + SVG_NS + '" width="0" height="0">' +
      symbols.join("") +
      "</svg>";
    document.body.insertBefore(host, document.body.firstChild);
  }

  // Returns a detached <svg> instancing one symbol, or null for an unknown
  // name — a caller naming a glyph that does not exist gets nothing rather
  // than an empty box.
  window.orionIcon = function (name, className) {
    if (!Object.prototype.hasOwnProperty.call(GLYPHS, name)) return null;
    build();
    var svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("class", "orion-ico" + (className ? " " + className : ""));
    svg.setAttribute("aria-hidden", "true");
    var use = document.createElementNS(SVG_NS, "use");
    use.setAttribute("href", "#i-" + name);
    svg.appendChild(use);
    return svg;
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", build);
  } else {
    build();
  }
})();

// ── Sidebar navigation icons ──
// One glyph for each of the three front-matter chapters and one for each part
// title, so the sidebar's eight sections are findable by shape as well as by
// reading the labels. Chapters inside a part stay text-only: an icon per page
// would be 60 of them, which is decoration, not navigation.
//
// They inherit `currentColor`, so the muted / hover / active colours in
// plasmatic.css §13 carry to them for free.
(function () {
  // Keyed by the page the sidebar link points at — the hrefs mdBook generates
  // carry a `path_to_root` prefix ("../introduction.html" on a nested page),
  // so matching is on the file name. Values are sprite symbol names.
  var CHAPTER_ICONS = {
    "introduction.html": "book",
    "comparison.html": "help",
    "characteristics.html": "layers",
  };

  // Keyed by part title, lower-cased. These are the `# Heading` lines in
  // SUMMARY.md; a part renamed there needs its key renamed here, and an
  // unmatched part simply keeps its plain text label.
  var PART_ICONS = {
    compare: "compare",
    "get started": "start",
    concepts: "concepts",
    "build with ai": "ai",
    build: "build",
    guides: "guides",
    operate: "operate",
    reference: "reference",
  };

  function decorate(el, name) {
    if (!name || el.querySelector(".nav-icon")) return;
    var glyph = window.orionIcon(name);
    if (!glyph) return;
    var icon = document.createElement("span");
    icon.className = "nav-icon";
    icon.appendChild(glyph);
    el.insertBefore(icon, el.firstChild);
    el.classList.add("with-nav-icon");
  }

  function injectIcons() {
    var chapter = document.querySelector(".sidebar .chapter");
    if (!chapter) return false;

    var links = chapter.querySelectorAll("a[href]");
    for (var i = 0; i < links.length; i++) {
      var href = links[i].getAttribute("href").split("#")[0];
      decorate(links[i], CHAPTER_ICONS[href.split("/").pop()]);
    }

    var parts = chapter.querySelectorAll("li.part-title");
    for (var j = 0; j < parts.length; j++) {
      decorate(parts[j], PART_ICONS[parts[j].textContent.trim().toLowerCase()]);
    }
    return true;
  }

  function start() {
    // The sidebar is built by mdBook's toc.js custom element, whose script is
    // in <head> — so the list is normally there already. The observer is the
    // belt to that braces: if it ever renders later, the icons still land.
    if (injectIcons()) return;
    var sidebar = document.getElementById("mdbook-sidebar") || document.body;
    var obs = new MutationObserver(function () {
      if (injectIcons()) obs.disconnect();
    });
    obs.observe(sidebar, { childList: true, subtree: true });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start);
  } else {
    start();
  }
})();

// ── On-page table of contents ──
// Built from the page's own h2/h3 headings, with a scroll-spy on the
// content scroller. Only rendered when there are at least two h2s —
// a short page does not need a second navigation. CSS hides the rail
// below 1400px, so this is progressive: no layout depends on it.
(function () {
  // The rail is hidden below 1400px (CSS §14), which is every 1280 and 1366
  // laptop — and the pages that need in-page navigation most are the 500-line
  // reference chapters. Same links, folded into a disclosure under the title
  // and closed by default, so it costs one line until someone wants it.
  function insertInlineToc(main, nav, count) {
    var box = document.createElement("details");
    box.className = "page-toc-inline";

    var summary = document.createElement("summary");
    summary.appendChild(document.createTextNode("On this page "));
    var tally = document.createElement("span");
    tally.className = "page-toc-inline-count";
    tally.textContent = "· " + count + " section" + (count === 1 ? "" : "s");
    summary.appendChild(tally);

    box.appendChild(summary);
    box.appendChild(nav);

    // Under the title, not above it. The h1 may be wrapped — the hero on the
    // introduction page puts it inside a div — so anchor on main's own child
    // that contains the h1 rather than on the h1 itself.
    var h1 = main.querySelector("h1");
    var anchor = null;
    if (h1) {
      anchor = h1;
      while (anchor && anchor.parentNode !== main) anchor = anchor.parentNode;
    }
    if (anchor) {
      main.insertBefore(box, anchor.nextSibling);
    } else {
      main.insertBefore(box, main.firstChild);
    }
  }

  function buildToc() {
    // print.html concatenates every chapter — 400+ h2s there would build a
    // rail longer than most pages. The sidebar is the right index for it.
    if (/\/print\.html$/.test(location.pathname)) return;

    var content = document.querySelector(".content");
    var main = content && content.querySelector("main");
    if (!main || content.querySelector(".page-toc")) return;

    var headings = main.querySelectorAll("h2[id], h3[id]");
    var h2count = main.querySelectorAll("h2[id]").length;
    if (h2count < 2) return;

    var nav = document.createElement("nav");
    nav.className = "page-toc";
    nav.setAttribute("aria-label", "On this page");

    var title = document.createElement("span");
    title.className = "page-toc-title";
    title.textContent = "On this page";
    nav.appendChild(title);

    var links = [];
    // The inline copy for viewports below the rail's breakpoint. Built from
    // the same walk rather than cloned, so the two can never disagree; CSS
    // §14 and §25 decide which of them is visible at a given width.
    var inlineNav = document.createElement("nav");
    inlineNav.setAttribute("aria-label", "On this page");

    for (var i = 0; i < headings.length; i++) {
      var h = headings[i];
      var a = document.createElement("a");
      a.href = "#" + h.id;
      a.className = h.tagName === "H3" ? "lvl-3" : "lvl-2";
      // The heading text includes mdBook's anchor link; textContent is enough.
      a.textContent = h.textContent.replace(/^»\s*/, "").trim();
      nav.appendChild(a);
      inlineNav.appendChild(a.cloneNode(true));
      links.push({ el: a, heading: h });
    }

    // Sits after <main> so the grid in plasmatic.css §14 can place it.
    main.parentNode.insertBefore(nav, main.nextSibling);
    insertInlineToc(main, inlineNav, h2count);

    var current = null;
    function spy() {
      // Viewport-relative: the document is what scrolls, and the sticky rail
      // sits just under the menu bar.
      var top = 120;
      var active = links[0];
      for (var i = 0; i < links.length; i++) {
        if (links[i].heading.getBoundingClientRect().top <= top) {
          active = links[i];
        }
      }
      if (active === current) return;
      if (current) current.el.classList.remove("active");
      active.el.classList.add("active");
      current = active;
    }

    var ticking = false;
    function onScroll() {
      if (ticking) return;
      ticking = true;
      requestAnimationFrame(function () {
        spy();
        ticking = false;
      });
    }

    // Capture phase on `document` catches the scroll wherever it originates —
    // the document itself today, or `.content` if mdBook ever caps its height.
    // Scroll events do not bubble, so a plain listener would miss one of them.
    document.addEventListener("scroll", onScroll, {
      capture: true,
      passive: true,
    });
    spy();

    // Opening or closing a fold moves every heading below it, so the rail's
    // highlight is stale until the spy runs again. The fold pass calls this.
    window.orionRespyToc = spy;
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", buildToc);
  } else {
    buildToc();
  }
})();

// ── Dark / Light theme toggle ──
(function () {
  var SUN_SVG =
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M12 18a6 6 0 1 1 0-12 6 6 0 0 1 0 12zm0-2a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM11 1h2v3h-2V1zm0 19h2v3h-2v-3zM3.515 4.929l1.414-1.414L7.05 5.636 5.636 7.05 3.515 4.93zM16.95 18.364l1.414-1.414 2.121 2.121-1.414 1.414-2.121-2.121zm2.121-14.85l1.414 1.415-2.121 2.121-1.414-1.414 2.121-2.121zM5.636 16.95l1.414 1.414-2.121 2.121-1.414-1.414 2.121-2.121zM23 11v2h-3v-2h3zM4 11v2H1v-2h3z"/></svg>';
  var MOON_SVG =
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M10 7a7 7 0 0 0 12 4.9v.1c0 5.523-4.477 10-10 10S2 17.523 2 12 6.477 2 12 2h.1A6.98 6.98 0 0 0 10 7zm-6 5a8 8 0 0 0 15.062 3.762A9 9 0 0 1 8.238 4.938 7.999 7.999 0 0 0 4 12z"/></svg>';

  function isDark() {
    return document.documentElement.classList.contains("navy");
  }

  function setTheme(theme) {
    // Use mdBook's hidden theme popup buttons to trigger a proper theme change
    var btn = document.getElementById("mdbook-theme-" + theme);
    if (btn) {
      btn.click();
    }
    updateIcon();
  }

  function updateIcon() {
    var toggle = document.getElementById("plasmatic-theme-toggle");
    if (!toggle) return;
    // Show sun icon in dark mode (click to go light), moon in light mode (click to go dark)
    toggle.innerHTML = isDark() ? SUN_SVG : MOON_SVG;
    toggle.title = isDark() ? "Switch to light theme" : "Switch to dark theme";
    toggle.setAttribute("aria-label", toggle.title);
  }

  function inject() {
    var origBtn = document.getElementById("mdbook-theme-toggle");
    if (!origBtn || document.getElementById("plasmatic-theme-toggle")) return;

    var toggle = document.createElement("button");
    toggle.id = "plasmatic-theme-toggle";
    toggle.type = "button";
    origBtn.parentNode.insertBefore(toggle, origBtn);

    toggle.addEventListener("click", function () {
      setTheme(isDark() ? "light" : "navy");
    });

    updateIcon();

    // Watch for external theme changes (e.g. OS prefers-color-scheme)
    new MutationObserver(updateIcon).observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["class"],
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", inject);
  } else {
    inject();
  }
})();

// ── Home button → goplasmatic.io ──
// First of the menu bar's right buttons, ahead of the repo icon: the two
// links that leave this site sit together. Injected rather than templated:
// mdBook has no config key for an extra menu bar link, and overriding
// theme/index.hbs would pin this book to one mdBook version's template. Same
// approach as the sidebar logo and theme toggle above.
(function () {
  var HOME_SVG =
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 576 512"><path d="M575.8 255.5c0 18-15 32.1-32 32.1h-32l.7 160.2c0 2.7-.2 5.4-.5 8.1V472c0 22.1-17.9 40-40 40H456c-1.1 0-2.2 0-3.3-.1c-1.4 .1-2.8 .1-4.2 .1H416 392c-22.1 0-40-17.9-40-40V448 384c0-17.7-14.3-32-32-32H256c-17.7 0-32 14.3-32 32v64 24c0 22.1-17.9 40-40 40H160 128.1c-1.5 0-3-.1-4.5-.2c-1.2 .1-2.4 .2-3.6 .2H104c-22.1 0-40-17.9-40-40V360c0-.9 0-1.9 .1-2.8V287.6H32c-18 0-32-14-32-32.1c0-9 3-17 10-24L266.4 8c7-7 15-8 22-8s15 2 21 7L564.8 231.5c8 7 12 15 11 24z"/></svg>';

  function inject() {
    var right = document.querySelector("#mdbook-menu-bar .right-buttons");
    if (!right || document.getElementById("plasmatic-home")) return;

    var home = document.createElement("a");
    home.id = "plasmatic-home";
    home.className = "icon-button";
    home.href = "https://goplasmatic.io";
    home.title = "Plasmatic home";
    home.setAttribute("aria-label", "Plasmatic home");

    var icon = document.createElement("span");
    icon.className = "fa-svg";
    icon.innerHTML = HOME_SVG;

    home.appendChild(icon);
    right.insertBefore(home, right.firstChild);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", inject);
  } else {
    inject();
  }
})();

// ── Section folding ──
// Opt in from the markdown with an empty marker div at the top of the page,
// matching the .doc-cards / .table-filter convention:
//
//   <div class="fold-sections" data-level="3" data-default="closed"
//        data-skip="related"></div>
//
// Each heading at that level, plus everything under it up to the next heading
// of the same level or higher, becomes one <details>. Doing it over the
// rendered DOM rather than in the source is what keeps 58 sections from
// becoming 58 hand-written HTML wrappers in a file that llms-full.txt
// concatenates verbatim.
//
// The heading element itself is MOVED, not copied, so it keeps the id and the
// <a class="header"> anchor mdBook generated for it. That is what lets the ToC
// rail, the search index and every inbound deep link go on working: mdBook
// emits a search entry per heading, including headings inside a closed fold,
// so a hit has to be able to open its way out. See openTo() below.
//
// Find-in-page cannot see closed content. Chromium expands a closed <details>
// on find; elsewhere "Expand all" is the answer, which is why that control is
// not optional. plasmatic.css §26 carries the styling.
(function () {
  var STORE_PREFIX = "orion-folds:";

  function storeKey() {
    return STORE_PREFIX + location.pathname;
  }

  function readState() {
    try {
      var raw = localStorage.getItem(storeKey());
      return raw ? JSON.parse(raw) : null;
    } catch (e) {
      return null;
    }
  }

  function writeState(ids) {
    try {
      localStorage.setItem(storeKey(), JSON.stringify(ids));
    } catch (e) {
      /* private mode, quota — the folds still work, they just do not persist */
    }
  }

  function headingLevel(el) {
    return /^H[1-6]$/.test(el.tagName) ? parseInt(el.tagName.charAt(1), 10) : 0;
  }

  function build(marker, main) {
    var level = parseInt(marker.dataset.level, 10);
    if (!level || level < 2 || level > 4) return [];

    var skip = (marker.dataset.skip || "")
      .split(",")
      .map(function (s) { return s.trim(); })
      .filter(Boolean);

    // Snapshot first: the walk reparents nodes, and a live child list would
    // shift underneath it.
    var children = Array.prototype.slice.call(main.children);
    var folds = [];

    for (var i = 0; i < children.length; i++) {
      var node = children[i];
      if (headingLevel(node) !== level) continue;
      if (!node.id || skip.indexOf(node.id) !== -1) continue;

      var body = [];
      for (var j = i + 1; j < children.length; j++) {
        var lvl = headingLevel(children[j]);
        if (lvl && lvl <= level) break;
        body.push(children[j]);
      }
      // A heading with nothing under it is a heading, not a disclosure.
      if (!body.length) continue;

      var details = document.createElement("details");
      details.className = "fold";
      var summary = document.createElement("summary");

      main.insertBefore(details, node);
      summary.appendChild(node);
      details.appendChild(summary);
      for (var k = 0; k < body.length; k++) details.appendChild(body[k]);

      folds.push(details);
    }

    // §22 gives the last of a run its closing hairline. `:last-of-type` only
    // ever matches one element per parent, so a run that ends before some
    // other block needs saying explicitly.
    for (var f = 0; f < folds.length; f++) {
      var after = folds[f].nextElementSibling;
      if (!after || after.tagName !== "DETAILS") folds[f].classList.add("is-last");
    }

    return folds;
  }

  function foldId(details) {
    var h = details.querySelector("summary > :is(h2,h3,h4)");
    return h ? h.id : null;
  }

  // Opens every fold on the path to `hash`, so a search result, a ToC click or
  // an inbound link lands on open content instead of a closed box.
  function openTo(hash) {
    if (!hash || hash.length < 2) return null;
    var target;
    try {
      target = document.getElementById(decodeURIComponent(hash.slice(1)));
    } catch (e) {
      return null;
    }
    if (!target) return null;
    var node = target.closest ? target.closest("details") : null;
    while (node) {
      node.open = true;
      node = node.parentNode && node.parentNode.closest
        ? node.parentNode.closest("details")
        : null;
    }
    return target;
  }

  function addControls(marker, folds, defaultOpen) {
    var bar = document.createElement("div");
    bar.className = "fold-controls";

    var label = document.createElement("span");
    label.className = "fold-controls-count";
    label.textContent = folds.length + " sections";

    var toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "fold-toggle";

    function anyClosed() {
      for (var i = 0; i < folds.length; i++) if (!folds[i].open) return true;
      return false;
    }

    function sync() {
      var expand = anyClosed();
      toggle.textContent = expand ? "Expand all" : "Collapse all";
      toggle.setAttribute("aria-expanded", expand ? "false" : "true");
    }

    toggle.addEventListener("click", function () {
      var expand = anyClosed();
      for (var i = 0; i < folds.length; i++) folds[i].open = expand;
      persist();
      sync();
      if (window.orionRespyToc) window.orionRespyToc();
    });

    function persist() {
      var open = [];
      for (var i = 0; i < folds.length; i++) {
        if (folds[i].open) {
          var id = foldId(folds[i]);
          if (id) open.push(id);
        }
      }
      // Nothing to remember when the page is exactly as it ships.
      var isDefault = defaultOpen ? open.length === folds.length : open.length === 0;
      if (isDefault) {
        try {
          localStorage.removeItem(storeKey());
        } catch (e) { /* see writeState */ }
      } else {
        writeState(open);
      }
    }

    bar.appendChild(label);
    bar.appendChild(toggle);
    marker.appendChild(bar);
    sync();

    return { sync: sync, persist: persist };
  }

  function run() {
    var main = document.querySelector(".content main");
    var marker = main && main.querySelector(".fold-sections");
    if (!marker) return;

    var folds = build(marker, main);
    if (!folds.length) return;

    var defaultOpen = marker.dataset.default !== "closed";
    var remembered = readState();

    for (var i = 0; i < folds.length; i++) {
      var id = foldId(folds[i]);
      folds[i].open = remembered
        ? remembered.indexOf(id) !== -1
        : defaultOpen;
    }

    var controls = addControls(marker, folds, defaultOpen);

    // A click on the heading's own anchor should copy/visit the link, not
    // toggle the section it names — the summary's default action would close
    // the very thing the link points at.
    for (var f = 0; f < folds.length; f++) {
      folds[f].addEventListener("click", function (e) {
        var anchor = e.target.closest && e.target.closest("summary a.header");
        if (!anchor) return;
        e.preventDefault();
        this.open = true;
        var href = anchor.getAttribute("href");
        if (history.replaceState) history.replaceState(null, "", href);
        else location.hash = href;
        controls.sync();
        controls.persist();
      });
      folds[f].addEventListener("toggle", function () {
        controls.sync();
        controls.persist();
        if (window.orionRespyToc) window.orionRespyToc();
      });
    }

    // The page may already have been asked for a heading inside a fold —
    // by a search result, a ToC link or an inbound URL. The browser resolved
    // that hash before this script ran and found nothing scrollable.
    var target = openTo(location.hash);
    if (target) target.scrollIntoView();

    window.addEventListener("hashchange", function () {
      var el = openTo(location.hash);
      if (el) el.scrollIntoView();
      controls.sync();
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", run);
  } else {
    run();
  }
})();

// ── Table marks: HTTP methods and status codes ──
// Both run over the rendered DOM because CSS cannot select on the text in a
// cell, and doing it here keeps the markdown as markdown: an endpoint table
// stays a markdown table and `` `503` `` stays inline code, so llms-full.txt
// (a raw concatenation of the sources) sees no markup it has to ignore.
// plasmatic.css §23 carries the colours.
(function () {
  var METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

  // An allowlist, not /^[1-5]\d\d$/. The loose pattern also matches the
  // rollout percentage `100`, `300` as a default in seconds, and the 255
  // character cap — none of which are statuses. These are the codes the
  // book actually documents as responses.
  var STATUS = {
    200: 1, 201: 1, 202: 1, 204: 1, 207: 1,
    400: 1, 401: 1, 403: 1, 404: 1, 405: 1, 408: 1, 409: 1,
    413: 1, 415: 1, 422: 1, 429: 1,
    500: 1, 502: 1, 503: 1, 504: 1,
  };

  function markMethods(main) {
    var tables = main.querySelectorAll("table");
    for (var i = 0; i < tables.length; i++) {
      var head = tables[i].querySelector("thead th");
      if (!head || head.textContent.trim().toLowerCase() !== "method") continue;

      var cells = tables[i].querySelectorAll("tbody tr > td:first-child");
      for (var j = 0; j < cells.length; j++) {
        var cell = cells[j];
        var verb = cell.textContent.trim().toUpperCase();
        if (METHODS.indexOf(verb) === -1) continue;

        var badge = document.createElement("span");
        badge.className = "m-badge m-" + verb.toLowerCase();
        badge.textContent = verb;
        cell.textContent = "";
        cell.appendChild(badge);
        cell.classList.add("http-method");
      }
    }
  }

  function markStatusCodes(main) {
    var codes = main.querySelectorAll("code");
    for (var i = 0; i < codes.length; i++) {
      var el = codes[i];
      // Only inline code. A block is highlighted markup with its own spans.
      if (el.closest("pre")) continue;
      var text = el.textContent.trim();
      if (!Object.prototype.hasOwnProperty.call(STATUS, text)) continue;
      el.classList.add("status-code", "sc-" + text.charAt(0));
    }
  }

  // A cell whose ENTIRE text is yes or no is a boolean, wherever it sits —
  // the guards-by-ingress matrix, a Required column, the paging table. The
  // word stays in the DOM and the glyph carries aria-hidden, so a screen
  // reader and llms-full.txt both still read "Yes"; nothing depends on the
  // mark. That is also why this is not a CSS ::before with a character in it.
  function markBooleans(main) {
    var cells = main.querySelectorAll("table tbody td");
    for (var i = 0; i < cells.length; i++) {
      var cell = cells[i];
      var text = cell.textContent.trim();
      var glyph;
      if (text === "Yes" || text === "yes") glyph = "check";
      else if (text === "No" || text === "no") glyph = "dash";
      else continue;

      var icon = window.orionIcon(glyph, "bool-mark bool-" + glyph);
      if (!icon) continue;
      var wrap = document.createElement("span");
      wrap.className = "bool-cell";
      wrap.appendChild(icon);
      wrap.appendChild(document.createTextNode(text));
      cell.textContent = "";
      cell.appendChild(wrap);
    }
  }

  // Two families of categorical value that repeat across the book: the five
  // connector types, and the four ingresses a channel can be reached on.
  // Both are matched narrowly — the type family only inside a table whose
  // first header is "Type", the ingress family only in the header row of the
  // guards matrix — so a stray cell that happens to read "cache" somewhere
  // else is never decorated.
  var CONNECTOR_TYPES = {
    http: "type-http",
    db: "type-db",
    cache: "type-cache",
    es: "type-es",
    kafka: "type-kafka",
  };

  var INGRESSES = {
    "http sync": "ingress-sync",
    "http /async": "ingress-async",
    kafka: "type-kafka",
    channel_call: "ingress-call",
  };

  function prefixGlyph(cell, name, extraClass) {
    var icon = window.orionIcon(name, extraClass);
    if (!icon) return;
    var wrap = document.createElement("span");
    wrap.className = "cat-cell";
    wrap.appendChild(icon);
    while (cell.firstChild) wrap.appendChild(cell.firstChild);
    cell.appendChild(wrap);
  }

  function markCategories(main) {
    var tables = main.querySelectorAll("table");
    for (var i = 0; i < tables.length; i++) {
      var first = tables[i].querySelector("thead th");
      if (!first) continue;
      var head = first.textContent.trim().toLowerCase();

      if (head === "type") {
        var cells = tables[i].querySelectorAll("tbody tr > td:first-child");
        for (var j = 0; j < cells.length; j++) {
          var key = cells[j].textContent.trim().toLowerCase();
          if (CONNECTOR_TYPES[key]) prefixGlyph(cells[j], CONNECTOR_TYPES[key], "cat-mark");
        }
      } else if (head === "guard") {
        var heads = tables[i].querySelectorAll("thead th");
        for (var k = 1; k < heads.length; k++) {
          var label = heads[k].textContent.trim().toLowerCase();
          if (INGRESSES[label]) prefixGlyph(heads[k], INGRESSES[label], "cat-mark");
        }
      }
    }
  }

  function run() {
    var main = document.querySelector(".content main");
    if (!main) return;
    markMethods(main);
    markStatusCodes(main);
    markBooleans(main);
    markCategories(main);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", run);
  } else {
    run();
  }
})();

// ── Destination glyphs on cross-links ──
// Nearly every page ends in "## Related" or "## Next steps", and those lists
// mix concepts, reference, guides and operate pages with nothing to tell them
// apart. The same eight part glyphs the sidebar uses go in front of each link,
// so the vocabulary a reader learns in the rail is readable again in the link
// lists and the card grids: the mark says what KIND of page is on the other
// end, which is the question a Related list raises and never answers.
//
// Keyed by directory because that is what a SUMMARY part is on disk. A part
// renamed in SUMMARY.md needs no change here; a directory renamed does.
(function () {
  var BY_DIR = {
    compare: "compare",
    "getting-started": "start",
    concepts: "concepts",
    ai: "ai",
    build: "build",
    guides: "guides",
    operate: "operate",
    reference: "reference",
  };

  var BY_PAGE = {
    "introduction.html": "book",
    "comparison.html": "help",
    "characteristics.html": "layers",
  };

  function glyphFor(href) {
    if (!href || /^[a-z]+:/i.test(href) || href.charAt(0) === "#") return null;

    // Resolved against the current page, not parsed as written. A sibling
    // link inside one part is `./errors.md` with no directory to read, and
    // the site is served under a /Orion/ prefix on Pages — letting the URL
    // parser do it handles `./`, `../` and the prefix in one step.
    var path;
    try {
      path = new URL(href, location.href).pathname;
    } catch (e) {
      return null;
    }

    var parts = path.split("/").filter(Boolean);
    if (!parts.length) return null;
    var file = parts[parts.length - 1];
    if (BY_PAGE[file]) return BY_PAGE[file];
    return parts.length > 1 ? BY_DIR[parts[parts.length - 2]] || null : null;
  }

  function decorate(list) {
    var items = list.querySelectorAll(":scope > li");
    for (var i = 0; i < items.length; i++) {
      var link = items[i].querySelector("a[href]");
      if (!link || items[i].querySelector(".dest-mark")) continue;
      var glyph = window.orionIcon(glyphFor(link.getAttribute("href")), "dest-mark");
      if (!glyph) continue;
      items[i].insertBefore(glyph, items[i].firstChild);
      items[i].classList.add("with-dest-mark");
    }
  }

  function run() {
    var main = document.querySelector(".content main");
    if (!main) return;

    // The closing link list on nearly every page. After folding, the heading
    // can sit inside a <summary>, so the list is a sibling of the details
    // rather than of the heading — look in both places.
    var heads = main.querySelectorAll("h2#related, h2#next-steps");
    for (var i = 0; i < heads.length; i++) {
      var list = null;
      var sib = heads[i].nextElementSibling;
      while (sib && !list) {
        if (sib.tagName === "UL") list = sib;
        else if (/^H[1-3]$/.test(sib.tagName)) break;
        sib = sib.nextElementSibling;
      }
      if (!list) {
        var box = heads[i].closest("details");
        if (box) list = box.querySelector("ul");
      }
      if (list) decorate(list);
    }

    // Card grids get the same vocabulary.
    var cards = main.querySelectorAll(".doc-cards > ul");
    for (var c = 0; c < cards.length; c++) decorate(cards[c]);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", run);
  } else {
    run();
  }
})();

// ── Table filter ──
// Opt in from the markdown with an empty marker div immediately before the
// table, matching the .doc-cards / .themed-media convention:
//
//   <div class="table-filter" data-label="Filter metrics"></div>
//
// Progressive: with JS off the marker renders nothing and the table is
// untouched. plasmatic.css §24 carries the styling.
(function () {
  function attach(marker) {
    // mdBook wraps every table in <div class="table-wrapper">, so the table
    // is inside the marker's next element sibling.
    var next = marker.nextElementSibling;
    var table = next && next.querySelector ? next.querySelector("table") : null;
    if (!table) return;

    var rows = table.querySelectorAll("tbody tr");
    if (!rows.length) return;

    var box = document.createElement("div");
    box.className = "table-filter-box";

    var input = document.createElement("input");
    input.type = "search";
    input.setAttribute("autocomplete", "off");
    input.placeholder = marker.dataset.label || "Filter…";
    input.setAttribute("aria-label", input.placeholder);

    var count = document.createElement("span");
    count.className = "table-filter-count";
    count.setAttribute("aria-live", "polite");

    box.appendChild(input);
    box.appendChild(count);
    marker.appendChild(box);

    // Lower-cased once per row rather than on every keystroke.
    var haystack = [];
    for (var i = 0; i < rows.length; i++) {
      haystack.push(rows[i].textContent.toLowerCase());
    }

    function apply() {
      var q = input.value.trim().toLowerCase();
      var shown = 0;
      for (var i = 0; i < rows.length; i++) {
        var hit = !q || haystack[i].indexOf(q) !== -1;
        rows[i].hidden = !hit;
        if (hit) shown++;
      }
      count.textContent = q ? shown + " of " + rows.length : "";
    }

    input.addEventListener("input", apply);
    // Escape clears rather than leaving a filtered table behind.
    input.addEventListener("keydown", function (e) {
      if (e.key === "Escape" && input.value) {
        e.stopPropagation();
        input.value = "";
        apply();
      }
    });
  }

  function run() {
    var markers = document.querySelectorAll(".content main .table-filter");
    for (var i = 0; i < markers.length; i++) attach(markers[i]);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", run);
  } else {
    run();
  }
})();

// ── Per-row anchors on reference tables ──
// A config key, an error code, a metric name and a CLI flag are all things
// people link each other to, and until now the finest anchor in the book was
// the section. Each first-column cell in a reference table gets an id derived
// from its own text, plus a link that appears on hover — so a support answer
// can point at one key instead of one page.
//
// Reference pages only: elsewhere a table is illustrating a point, not
// serving as a lookup surface, and 700 more ids would just be weight.
(function () {
  function slug(text) {
    return text
      .trim()
      .toLowerCase()
      .replace(/[`'"]/g, "")
      .replace(/[^a-z0-9._/-]+/g, "-")
      .replace(/^-+|-+$/g, "");
  }

  function run() {
    if (!/\/reference\//.test(location.pathname)) return;
    var main = document.querySelector(".content main");
    if (!main) return;

    var seen = {};
    // mdBook's heading ids are already in the document; a row id must not
    // collide with one, or the two would fight over the same fragment.
    var taken = main.querySelectorAll("[id]");
    for (var t = 0; t < taken.length; t++) seen[taken[t].id] = 1;

    var cells = main.querySelectorAll("table tbody tr > td:first-child");
    for (var i = 0; i < cells.length; i++) {
      var cell = cells[i];
      var text = cell.textContent;
      if (!text || text.length > 60) continue;
      var base = slug(text);
      if (!base || /^[0-9]+$/.test(base)) continue;

      var id = "row-" + base;
      var n = 2;
      while (seen[id]) id = "row-" + base + "-" + n++;
      seen[id] = 1;
      cell.id = id;

      var a = document.createElement("a");
      a.className = "row-anchor";
      a.href = "#" + id;
      a.setAttribute("aria-label", "Link to " + text.trim());
      var glyph = window.orionIcon("anchor");
      if (!glyph) continue;
      a.appendChild(glyph);
      cell.appendChild(a);
      cell.classList.add("has-row-anchor");
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", run);
  } else {
    run();
  }
})();

// ── Disclosures: open everything for printing ──
// A closed <details> prints as a title with no content, which on paper is
// indistinguishable from a section that has nothing in it. Open them all for
// the print, then restore exactly what was open before.
(function () {
  var reopened = [];

  function expand() {
    reopened = [];
    var all = document.querySelectorAll(".content main details");
    for (var i = 0; i < all.length; i++) {
      if (!all[i].open) {
        all[i].open = true;
        reopened.push(all[i]);
      }
    }
  }

  function restore() {
    for (var i = 0; i < reopened.length; i++) reopened[i].open = false;
    reopened = [];
  }

  window.addEventListener("beforeprint", expand);
  window.addEventListener("afterprint", restore);

  // Safari fires neither event; it drives the same transition through the
  // print media query instead.
  if (window.matchMedia) {
    var mq = window.matchMedia("print");
    var onChange = function (e) {
      if (e.matches) expand();
      else restore();
    };
    if (mq.addEventListener) mq.addEventListener("change", onChange);
    else if (mq.addListener) mq.addListener(onChange);
  }
})();

// ── Search: show the keyboard shortcut ──
// mdBook binds `/` and `s` to the search box and says so only in the toggle
// button's title attribute, which nobody reads. A chip in the field itself is
// how the shortcut gets learned for next time.
(function () {
  function inject() {
    var form = document.getElementById("mdbook-searchbar-outer");
    var input = document.getElementById("mdbook-searchbar");
    if (!form || !input || document.getElementById("search-key-hint")) return;

    var hint = document.createElement("kbd");
    hint.id = "search-key-hint";
    hint.setAttribute("aria-hidden", "true");
    hint.textContent = "/";
    form.appendChild(hint);

    // Nothing to hint at once the field has focus and content.
    function sync() {
      hint.hidden = input.value.length > 0;
    }
    input.addEventListener("input", sync);
    sync();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", inject);
  } else {
    inject();
  }
})();

// ── Page footer: named prev/next, and a way to report a page ──
// mdBook's own chapter navigation is two unlabelled chevrons pinned to the
// window edges. They say a next chapter exists; they never say what it is —
// and with `no-section-label` on, nothing else in the book carries the
// reading order either. The names come from the sidebar, which already has
// every chapter title on the page.
//
// The report links are deliberately NOT a "was this page helpful?" thumbs
// pair. A vote widget with no backend records nothing; these two links go
// somewhere a reply can come back from, prefilled with the page and its
// source file so the reader does not have to describe where they were.
(function () {
  var REPO = "https://github.com/GoPlasmatic/Orion";

  function resolve(href) {
    try {
      return new URL(href, location.href).pathname;
    } catch (e) {
      return null;
    }
  }

  // The sidebar is the only place the chapter titles exist on a rendered
  // page, so it is where the names come from.
  function titleFor(href) {
    var want = resolve(href);
    if (!want) return null;
    var links = document.querySelectorAll(".sidebar .chapter a[href]");
    for (var i = 0; i < links.length; i++) {
      if (resolve(links[i].getAttribute("href")) === want) {
        return links[i].textContent.trim();
      }
    }
    return null;
  }

  function arrow(href, rel, label) {
    var title = titleFor(href);
    if (!title) return null;
    var a = document.createElement("a");
    a.className = "page-nav-link page-nav-" + rel;
    a.href = href;
    a.rel = rel;
    var kicker = document.createElement("span");
    kicker.className = "page-nav-kicker";
    kicker.textContent = label;
    var name = document.createElement("b");
    name.textContent = title;
    a.appendChild(kicker);
    a.appendChild(name);
    return a;
  }

  // docs/src/reference/errors.html -> docs/src/reference/errors.md, so a
  // report names the file to edit rather than the page that rendered it.
  function sourcePath() {
    var parts = location.pathname.split("/").filter(Boolean);
    var file = parts.pop() || "index.html";
    var root = parts.indexOf("Orion");
    var dirs = root === -1 ? parts : parts.slice(root + 1);
    return "docs/src/" + dirs.concat(file.replace(/\.html$/, ".md")).join("/");
  }

  function reportLinks() {
    var wrap = document.createElement("div");
    wrap.className = "page-report";

    var pageTitle = document.querySelector(".content main h1");
    var name = pageTitle ? pageTitle.textContent.trim() : document.title;

    var lead = document.createElement("span");
    lead.textContent = "Something wrong on this page?";

    var body =
      "Page: " + location.href + "\nSource: " + sourcePath() + "\n\n";
    var issue = document.createElement("a");
    issue.href =
      REPO +
      "/issues/new?labels=documentation&title=" +
      encodeURIComponent("Docs: " + name) +
      "&body=" +
      encodeURIComponent(body);
    issue.textContent = "Report an issue";
    issue.rel = "noopener";

    var ask = document.createElement("a");
    ask.href = REPO + "/discussions";
    ask.textContent = "Ask in Discussions";
    ask.rel = "noopener";

    wrap.appendChild(lead);
    wrap.appendChild(issue);
    wrap.appendChild(ask);
    return wrap;
  }

  function run() {
    var main = document.querySelector(".content main");
    if (!main || main.querySelector(".page-nav")) return;
    if (/\/print\.html$/.test(location.pathname)) return;

    var foot = document.createElement("footer");
    foot.className = "page-foot";

    var prevHref = document.querySelector(".nav-wrapper a[rel~='prev']");
    var nextHref = document.querySelector(".nav-wrapper a[rel~='next']");

    var prev = prevHref && arrow(prevHref.getAttribute("href"), "prev", "Previous");
    var next = nextHref && arrow(nextHref.getAttribute("href"), "next", "Next");

    if (prev || next) {
      var nav = document.createElement("nav");
      nav.className = "page-nav";
      nav.setAttribute("aria-label", "Chapter navigation");
      // An empty cell so a page with only a next link still puts it on the
      // right, where the reading order says it belongs.
      nav.appendChild(prev || document.createElement("span"));
      nav.appendChild(next || document.createElement("span"));
      foot.appendChild(nav);
    }

    foot.appendChild(reportLinks());
    main.appendChild(foot);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", run);
  } else {
    run();
  }
})();

// ── asciinema player embeds ──
// Renders any <div class="asciinema-player" data-cast="casts/foo.cast"></div>.
// The data-cast path is book-root-relative; we prepend mdBook's path_to_root so
// it resolves from any page depth.
(function () {
  function mount() {
    if (typeof AsciinemaPlayer === "undefined") return;
    var prefix = typeof path_to_root !== "undefined" ? path_to_root : "";
    var nodes = document.querySelectorAll(".asciinema-player[data-cast]");
    for (var i = 0; i < nodes.length; i++) {
      var el = nodes[i];
      if (el.dataset.mounted) continue;
      el.dataset.mounted = "1";
      AsciinemaPlayer.create(prefix + el.dataset.cast, el, {
        fit: "width",
        terminalFontSize: "13px",
        theme: "asciinema",
        controls: true,
        poster: "npt:0:02",
        idleTimeLimit: 2,
      });
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", mount);
  } else {
    mount();
  }
})();
