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
