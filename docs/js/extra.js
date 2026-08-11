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
