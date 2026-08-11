# Vendored third-party assets — docs/js, docs/css and docs/src/webfonts

Minified upstream builds committed so the mdBook site is fully self-hosted
(no CDN at page-load time). `deny.toml` covers only the Rust dependency tree,
so this file is the license/provenance record for these assets.

| File | Package | Version | License | Upstream |
|------|---------|---------|---------|----------|
| `docs/js/asciinema-player.min.js` | asciinema-player | 3.17.0 | Apache-2.0 | <https://github.com/asciinema/asciinema-player> |
| `docs/css/asciinema-player.css` | asciinema-player (same release as the JS) | 3.17.0 | Apache-2.0 | <https://github.com/asciinema/asciinema-player> |

Take both files from `dist/bundle/` in the npm tarball — that build is
self-contained. The same directory also ships `asciinema-player-ui.js` and
`asciinema-player-worker.js`; those are for module consumers and are **not**
needed here, which is what keeps this a two-file drop-in.

The version is not embedded in the minified build, so it cannot be read back
out of the file. It was recovered for 3.8.0 (the previous vendored release) by
byte-comparing against the published tarballs; keep this table current instead
of relying on that.

When updating: replace both files together (they ship as a pair), write the
version into this table, and re-check that the three cast embeds still mount —
`docs/src/getting-started/first-service.md` has two,
`docs/src/ai/mcp-setup.md` has one.

## Webfonts

`docs/src/webfonts/*.woff2`, declared in `docs/css/fonts.css`. All three are
variable fonts, so one file per subset covers the full weight range the theme
uses. Only `latin` loads on an English page; `latin-ext` is gated behind
`unicode-range` and fetched only if an extended character appears.

| Family | Faces | Axes shipped | License | Upstream |
|--------|-------|--------------|---------|----------|
| Inter | `inter-{latin,latin-ext}.woff2`, `inter-italic-{latin,latin-ext}.woff2` | wght 400–700 (400–600 italic) | OFL-1.1 | <https://github.com/rsms/inter> |
| Montserrat | `montserrat-{latin,latin-ext}.woff2` | wght 600–700 | OFL-1.1 | <https://github.com/JulietaUla/Montserrat> |
| JetBrains Mono | `jetbrains-mono-{latin,latin-ext}.woff2` | wght 400–500 | OFL-1.1 | <https://github.com/JetBrains/JetBrainsMono> |

These were fetched from the Google Fonts CSS2 API, which serves the woff2
subsets above. They replaced a runtime `@import url(fonts.googleapis.com/…)`
that had sat at the top of `plasmatic.css` — a third-party dependency on every
page load, and one that serialised the font request behind the stylesheet
parse. When updating, re-request the same families and weight ranges, keep the
`latin` + `latin-ext` subsets only, and preserve each block's `unicode-range`.
