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

// ── Sidebar navigation icons ──
// One glyph for each of the three front-matter chapters and one for each part
// title, so the sidebar's eight sections are findable by shape as well as by
// reading the labels. Chapters inside a part stay text-only: an icon per page
// would be 60 of them, which is decoration, not navigation.
//
// The glyphs are drawn here rather than pulled from an icon font: two strokes
// and a viewBox each, no third-party asset to vendor, license or keep in sync
// (js/VENDOR.md tracks the ones that are). They inherit `currentColor`, so the
// muted / hover / active colours in plasmatic.css §13 carry to them for free.
(function () {
  var ATTRS =
    'fill="none" stroke="currentColor" stroke-width="1.7" ' +
    'stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"';

  function svg(body) {
    return (
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" ' +
      ATTRS +
      ">" +
      body +
      "</svg>"
    );
  }

  // Keyed by the page the sidebar link points at — the hrefs mdBook generates
  // carry a `path_to_root` prefix ("../introduction.html" on a nested page),
  // so matching is on the file name.
  var CHAPTER_ICONS = {
    // Open book.
    "introduction.html": svg(
      '<path d="M12 6.5c-1.6-1.4-3.6-2-6-2-.9 0-1.7.1-2.4.2a.7.7 0 0 0-.6.7v11.9c0 .4.4.8.9.7.6-.1 1.3-.1 2.1-.1 2.4 0 4.4.6 6 2 1.6-1.4 3.6-2 6-2 .8 0 1.5 0 2.1.1.5.1.9-.3.9-.7V5.4a.7.7 0 0 0-.6-.7c-.7-.1-1.5-.2-2.4-.2-2.4 0-4.4.6-6 2Z"/>' +
        '<path d="M12 6.5V19"/>',
    ),
    // Question mark in a circle.
    "comparison.html": svg(
      '<circle cx="12" cy="12" r="9"/>' +
        '<path d="M9.6 9.3a2.5 2.5 0 1 1 3.4 2.5c-.7.3-1 .9-1 1.6v.4"/>' +
        '<path d="M12 17h.01"/>',
    ),
    // Stacked layers.
    "characteristics.html": svg(
      '<path d="m12 3 9 5-9 5-9-5 9-5Z"/>' +
        '<path d="m3.5 12.5 8.5 4.7 8.5-4.7"/>' +
        '<path d="m3.5 16.5 8.5 4.7 8.5-4.7"/>',
    ),
  };

  // Keyed by part title, lower-cased. These are the `# Heading` lines in
  // SUMMARY.md; a part renamed there needs its key renamed here, and an
  // unmatched part simply keeps its plain text label.
  var PART_ICONS = {
    // Arrows pointing opposite ways.
    compare: svg(
      '<path d="M4 9h13"/><path d="m14 6 3 3-3 3"/>' +
        '<path d="M20 15H7"/><path d="m10 12-3 3 3 3"/>',
    ),
    // Play button.
    "get started": svg(
      '<circle cx="12" cy="12" r="9"/><path d="m10 8.5 6 3.5-6 3.5z"/>',
    ),
    // Cube.
    concepts: svg(
      '<path d="m12 2.8 8 4.6v9.2l-8 4.6-8-4.6V7.4z"/>' +
        '<path d="m4.3 7.6 7.7 4.4 7.7-4.4"/><path d="M12 21v-9"/>',
    ),
    // Sparkles.
    "build with ai": svg(
      '<path d="m12 3.5 1.9 5.3 5.3 1.9-5.3 1.9-1.9 5.3-1.9-5.3-5.3-1.9 5.3-1.9z"/>' +
        '<path d="m18.5 16.5.7 1.8 1.8.7-1.8.7-.7 1.8-.7-1.8-1.8-.7 1.8-.7z"/>',
    ),
    // Angle brackets.
    build: svg(
      '<path d="m8.5 8.5-4 3.5 4 3.5"/><path d="m15.5 8.5 4 3.5-4 3.5"/>' +
        '<path d="m13.5 5-3 14"/>',
    ),
    // Folded map.
    guides: svg(
      '<path d="m9 4.5-5.5 2.6v12.4L9 16.9l6 2.6 5.5-2.6V4.5L15 7.1z"/>' +
        '<path d="M9 4.5v12.4"/><path d="M15 7.1v12.4"/>',
    ),
    // Two server bays.
    operate: svg(
      '<rect x="3.2" y="4.2" width="17.6" height="6" rx="1.6"/>' +
        '<rect x="3.2" y="13.8" width="17.6" height="6" rx="1.6"/>' +
        '<path d="M7 7.2h.01"/><path d="M7 16.8h.01"/>',
    ),
    // Document.
    reference: svg(
      '<path d="M13.5 3.5H7A1.5 1.5 0 0 0 5.5 5v14A1.5 1.5 0 0 0 7 20.5h10a1.5 1.5 0 0 0 1.5-1.5V8.5z"/>' +
        '<path d="M13.5 3.5v5h5"/><path d="M9 13h6"/><path d="M9 16.5h4"/>',
    ),
  };

  function decorate(el, markup) {
    if (!markup || el.querySelector(".nav-icon")) return;
    var icon = document.createElement("span");
    icon.className = "nav-icon";
    icon.innerHTML = markup;
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
    for (var i = 0; i < headings.length; i++) {
      var h = headings[i];
      var a = document.createElement("a");
      a.href = "#" + h.id;
      a.className = h.tagName === "H3" ? "lvl-3" : "lvl-2";
      // The heading text includes mdBook's anchor link; textContent is enough.
      a.textContent = h.textContent.replace(/^»\s*/, "").trim();
      nav.appendChild(a);
      links.push({ el: a, heading: h });
    }

    // Sits after <main> so the grid in plasmatic.css §14 can place it.
    main.parentNode.insertBefore(nav, main.nextSibling);

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
