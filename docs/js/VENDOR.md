# Vendored third-party assets — docs/js and docs/css

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
