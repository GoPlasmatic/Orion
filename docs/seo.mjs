// Post-process the built mdBook into something a search engine and an answer
// engine can actually read. Run from docs/build.sh after `mdbook build`.
//
// Node, not Python: `npx wrangler deploy` is the deploy command, so Node is
// guaranteed present on Cloudflare's build image in a way python3 is not.
// No dependencies — this runs in a build sandbox with no install step.
//
// mdBook 0.5 gives every page the same <meta name="description"> (the
// book.toml one) and emits no canonical, no Open Graph and no structured
// data. That is four problems:
//
//   1. 90 pages sharing one description is a page-quality signal, and the
//      description is what an answer engine quotes when it summarises a page.
//   2. index.html and introduction.html are byte-identical content on two
//      URLs. Without a canonical, that is a duplicate a crawler has to guess
//      about.
//   3. Every docs link shared in Slack, X or LinkedIn renders bare.
//   4. Nothing tells a crawler this is technical documentation for a named
//      piece of software, which is what JSON-LD carries.
//
// Per-page descriptions come from an HTML comment on the first line of each
// source .md — `<!-- description: … -->`. That is co-located with the prose
// it describes, so it is in front of whoever rewrites the page, and
// docs/lint.sh check 15 fails the build when one is missing, too long, or
// duplicated.

import { readFileSync, writeFileSync, readdirSync, statSync } from "node:fs";
import { join, relative, dirname, sep } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const SRC = join(HERE, "src");
const BOOK = join(HERE, "book");
const ORIGIN = "https://docs.goplasmatic.io";
const OG_IMAGE = `${ORIGIN}/images/og-card.png`;
const SITE_NAME = "Orion Documentation";

// ── helpers ──────────────────────────────────────────────────────────────

function walk(dir, ext, out = []) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) walk(p, ext, out);
    else if (name.endsWith(ext)) out.push(p);
  }
  return out;
}

const esc = (s) =>
  s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");

// mdBook writes titles as "<chapter> - <book title>". The chapter half is what
// belongs in og:title; the site name is carried separately by og:site_name, and
// repeating it there is what makes a shared card read "Foo - Orion
// Documentation | Orion Documentation".
function chapterTitle(html) {
  const m = html.match(/<title>([\s\S]*?)<\/title>/i);
  if (!m) return null;
  const full = m[1].trim();
  const suffix = ` - ${SITE_NAME}`;
  return full.endsWith(suffix) ? full.slice(0, -suffix.length) : full;
}

// ── the part each chapter sits under, from SUMMARY.md ────────────────────
//
// A breadcrumb tells a crawler where a page sits in the book, and is what
// Google renders in place of a bare URL under a result. mdBook knows the part
// structure but does not emit it, so read it back out of SUMMARY.md: a
// `# Heading` opens a part, and every chapter line under it belongs to it.
// Parts are not pages — there is no index to link — so the trail is
// Docs > Part > Page with only the ends addressable.

const partOf = new Map(); // "reference/workflows" -> "Reference"
{
  let part = null;
  for (const line of readFileSync(join(SRC, "SUMMARY.md"), "utf8").split("\n")) {
    const h = line.match(/^#\s+(.+?)\s*$/);
    if (h) {
      part = h[1] === "Summary" ? null : h[1];
      continue;
    }
    const c = line.match(/\]\(\.\/([A-Za-z0-9_./-]+)\.md\)/);
    if (c && part) partOf.set(c[1], part);
  }
}

// ── descriptions, read from the source markdown ──────────────────────────

const descriptions = new Map(); // "reference/workflows" -> text
for (const md of walk(SRC, ".md")) {
  const rel = relative(SRC, md).split(sep).join("/");
  if (rel === "SUMMARY.md") continue;
  const first = readFileSync(md, "utf8").split("\n", 1)[0];
  const m = first.match(/^<!--\s*description:\s*([\s\S]*?)\s*-->\s*$/);
  if (m) descriptions.set(rel.replace(/\.md$/, ""), m[1]);
}

// ── the pass ─────────────────────────────────────────────────────────────

const BOOK_DESCRIPTION = (() => {
  const idx = readFileSync(join(BOOK, "index.html"), "utf8");
  const m = idx.match(/<meta name="description" content="([\s\S]*?)">/);
  return m ? m[1] : "";
})();

const pages = [];
let injected = 0;
let skipped = [];

for (const file of walk(BOOK, ".html")) {
  let html = readFileSync(file, "utf8");

  // mdBook's book.toml redirect stubs are meta-refresh pages that already
  // carry a canonical pointing at their target. Leave them alone, and keep
  // them out of the sitemap — a sitemap of redirects is a crawl-budget sink.
  if (/<title>Redirecting\.\.\.<\/title>/.test(html)) continue;

  const relHtml = relative(BOOK, file).split(sep).join("/");
  const slug = relHtml.replace(/\.html$/, "");

  // toc.html is the sidebar iframe, not a page: it is every chapter title with
  // no prose, which is exactly the thin duplicate a crawler should never see.
  // 404.html is real but must never rank. Both get noindex and nothing else.
  if (slug === "toc" || slug === "404") {
    if (!/name="robots"/.test(html)) {
      html = html.replace(
        "</head>",
        `        <meta name="robots" content="noindex, follow">\n    </head>`,
      );
      writeFileSync(file, html);
    }
    continue;
  }

  // index.html and introduction.html are the same chapter rendered twice.
  // The root URL is the one linked from everywhere else, so it wins the
  // canonical and introduction.html points at it.
  const isHome = slug === "index" || slug === "introduction";
  const canonical = isHome ? `${ORIGIN}/` : `${ORIGIN}/${relHtml}`;

  const lookup = slug === "index" ? "introduction" : slug;
  const description = descriptions.get(lookup);
  if (!description) skipped.push(relHtml);

  const title = chapterTitle(html) ?? SITE_NAME;
  const desc = description ?? BOOK_DESCRIPTION;

  // Replace the book-wide description rather than adding a second one.
  html = html.replace(
    /<meta name="description" content="[\s\S]*?">/,
    `<meta name="description" content="${esc(desc)}">`,
  );

  const jsonLd = {
    "@context": "https://schema.org",
    "@type": "TechArticle",
    headline: title,
    description: desc,
    url: canonical,
    inLanguage: "en",
    isPartOf: {
      "@type": "WebSite",
      name: SITE_NAME,
      url: `${ORIGIN}/`,
    },
    about: {
      "@type": "SoftwareApplication",
      name: "Orion",
      applicationCategory: "DeveloperApplication",
      operatingSystem: "Linux, macOS, Windows",
      offers: { "@type": "Offer", price: "0", priceCurrency: "USD" },
    },
    publisher: {
      "@type": "Organization",
      name: "Plasmatic",
      url: "https://goplasmatic.io",
    },
  };

  const part = partOf.get(lookup);
  const breadcrumb =
    part && !isHome
      ? {
          "@context": "https://schema.org",
          "@type": "BreadcrumbList",
          itemListElement: [
            { "@type": "ListItem", position: 1, name: "Orion Docs", item: `${ORIGIN}/` },
            { "@type": "ListItem", position: 2, name: part },
            { "@type": "ListItem", position: 3, name: title, item: canonical },
          ],
        }
      : null;

  const tags = [
    `<link rel="canonical" href="${canonical}">`,
    `<meta property="og:type" content="article">`,
    `<meta property="og:site_name" content="${esc(SITE_NAME)}">`,
    `<meta property="og:title" content="${esc(title)}">`,
    `<meta property="og:description" content="${esc(desc)}">`,
    `<meta property="og:url" content="${canonical}">`,
    `<meta property="og:image" content="${OG_IMAGE}">`,
    `<meta property="og:image:width" content="1200">`,
    `<meta property="og:image:height" content="630">`,
    `<meta name="twitter:card" content="summary_large_image">`,
    `<meta name="twitter:title" content="${esc(title)}">`,
    `<meta name="twitter:description" content="${esc(desc)}">`,
    `<meta name="twitter:image" content="${OG_IMAGE}">`,
    `<script type="application/ld+json">${JSON.stringify(jsonLd)}</script>`,
    breadcrumb
      ? `<script type="application/ld+json">${JSON.stringify(breadcrumb)}</script>`
      : null,
  ]
    .filter(Boolean)
    .join("\n        ");

  html = html.replace("</head>", `        ${tags}\n    </head>`);
  writeFileSync(file, html);
  injected++;

  if (slug !== "introduction" && !/^404$/.test(slug)) {
    pages.push(canonical);
  }
}

// ── sitemap ──────────────────────────────────────────────────────────────

const urls = [...new Set(pages)].sort();
const sitemap =
  `<?xml version="1.0" encoding="UTF-8"?>\n` +
  `<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n` +
  urls
    .map(
      (u) =>
        `  <url><loc>${u}</loc><changefreq>weekly</changefreq>` +
        `<priority>${u === `${ORIGIN}/` ? "1.0" : "0.7"}</priority></url>`,
    )
    .join("\n") +
  `\n</urlset>\n`;
writeFileSync(join(BOOK, "sitemap.xml"), sitemap);

console.log(
  `docs/seo.mjs: ${injected} pages tagged, ${urls.length} in sitemap` +
    (skipped.length ? `, no description for: ${skipped.join(", ")}` : ""),
);
