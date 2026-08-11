# Vendored third-party assets — docs/js, docs/css and docs/src/webfonts

Minified upstream builds committed so the mdBook site is fully self-hosted
(no CDN at page-load time). `deny.toml` covers only the Rust dependency tree,
so this file is the license/provenance record for these assets.

| File | Package | Version | License | Upstream |
|------|---------|---------|---------|----------|
| `docs/js/asciinema-player.min.js` | asciinema-player | not embedded in the minified build — record the exact version on the next update | Apache-2.0 | <https://github.com/asciinema/asciinema-player> |
| `docs/css/asciinema-player.css` | asciinema-player (same release as the JS) | ditto | Apache-2.0 | <https://github.com/asciinema/asciinema-player> |

When updating: download the versioned release bundle from the upstream GitHub
releases page, replace both files together (they ship as a pair), and write the
version number into this table.

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
