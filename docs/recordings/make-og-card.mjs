// Render the 1200x630 Open Graph card into docs/src/images/og-card.png.
//
// Generated, not hand-designed in an image editor — same reason the terminal
// GIFs and the console screenshots next to it are generated: the card carries
// the brand's real type and palette, and when those change it is re-rendered
// rather than redrawn from memory.
//
// One-time, not part of the build: the card has no per-page content, so it
// changes only when the brand does. Run it, commit the PNG.
//
//   cd docs/recordings/ui && npm install      # first time only
//   node ../make-og-card.mjs
//
// Fonts are the repo's own vendored woff2 files, embedded as data: URIs so the
// render never depends on a network fetch or a locally installed face.

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

// playwright is CommonJS and installed under ui/, so resolve it from there
// rather than requiring this script to be run from that directory.
const { chromium } = createRequire(import.meta.url)("./ui/node_modules/playwright");

const HERE = dirname(fileURLToPath(import.meta.url));
const DOCS = join(HERE, "..");
const OUT = join(DOCS, "src/images/og-card.png");

const font = (f) =>
  `data:font/woff2;base64,${readFileSync(join(DOCS, "src/webfonts", f)).toString("base64")}`;
const logo = `data:image/png;base64,${readFileSync(join(DOCS, "src/images/plasmatic-logo.png")).toString("base64")}`;

// Palette is docs/css/plasmatic.css's dark theme, copied deliberately: a card
// is a static image and cannot read a CSS variable, so the values are duplicated
// here and this comment is the pointer back to their owner.
const html = `<!doctype html>
<meta charset="utf-8">
<style>
  @font-face { font-family: "Montserrat"; src: url("${font("montserrat-latin.woff2")}") format("woff2"); font-weight: 100 900; font-display: block; }
  @font-face { font-family: "Inter"; src: url("${font("inter-latin.woff2")}") format("woff2"); font-weight: 100 900; font-display: block; }
  @font-face { font-family: "JetBrains Mono"; src: url("${font("jetbrains-mono-latin.woff2")}") format("woff2"); font-weight: 100 800; font-display: block; }

  * { margin: 0; padding: 0; box-sizing: border-box; }
  html, body { width: 1200px; height: 630px; }
  body {
    background: #0B141C;
    color: #D7E3EB;
    font-family: "Inter", system-ui, sans-serif;
    font-feature-settings: "cv05" 1, "cv11" 1;
    position: relative;
    overflow: hidden;
  }
  /* A wash from the accent, so the card is not a flat rectangle. */
  .glow {
    position: absolute; inset: -30% -10% auto -20%; height: 150%;
    background: radial-gradient(60% 55% at 22% 32%, rgba(53,180,222,0.20) 0%, rgba(53,180,222,0) 68%);
  }
  .rule { position: absolute; left: 0; right: 0; top: 0; height: 6px;
          background: linear-gradient(90deg, #35B4DE 0%, #7DD3FC 42%, rgba(125,211,252,0) 100%); }
  .frame { position: relative; height: 100%; padding: 74px 88px; display: flex; flex-direction: column; }

  .brand { display: flex; align-items: center; gap: 18px; }
  .brand img { width: 52px; height: 52px; display: block; }
  .brand .name { font-family: "Montserrat", sans-serif; font-weight: 700; font-size: 34px; letter-spacing: -0.01em; }
  .brand .by { color: #67838F; font-size: 19px; letter-spacing: 0.02em; }

  h1 { font-family: "Montserrat", sans-serif; font-weight: 700; font-size: 68px;
       line-height: 1.06; letter-spacing: -0.025em; margin-top: 52px; max-width: 15.5ch; }
  h1 em { font-style: normal; color: #35B4DE; }

  p { margin-top: 26px; font-size: 27px; line-height: 1.45; color: #93ABB9; max-width: 44ch; }

  .foot { margin-top: auto; display: flex; align-items: center; justify-content: space-between; }
  .url { font-family: "JetBrains Mono", monospace; font-size: 22px; color: #35B4DE; letter-spacing: -0.01em; }
  .tags { display: flex; gap: 10px; }
  .tag { font-size: 17px; color: #93ABB9; border: 1px solid rgba(147,171,185,0.28);
         border-radius: 999px; padding: 7px 17px; white-space: nowrap; }
</style>
<div class="glow"></div><div class="rule"></div>
<div class="frame">
  <div class="brand">
    <img src="${logo}" alt="">
    <span class="name">Orion</span>
    <span class="by">by Plasmatic</span>
  </div>

  <h1>Declare a service.<br><em>Ship it in seconds.</em></h1>
  <p>A declarative services runtime. One JSON document holds the logic, the
     connectors and the endpoint — live a second after you post it.</p>

  <div class="foot">
    <span class="url">docs.goplasmatic.io</span>
    <div class="tags">
      <span class="tag">REST &amp; Kafka</span>
      <span class="tag">JSONLogic</span>
      <span class="tag">Rust</span>
    </div>
  </div>
</div>`;

const browser = await chromium.launch();
const page = await browser.newPage({
  viewport: { width: 1200, height: 630 },
  deviceScaleFactor: 1,
});
await page.setContent(html, { waitUntil: "load" });
await page.evaluate(() => document.fonts.ready);
const buf = await page.screenshot({ type: "png" });
await browser.close();

writeFileSync(OUT, buf);
console.log(`make-og-card: wrote ${OUT} (${buf.length.toLocaleString()} bytes)`);
