# Vendored third-party assets — docs/src/diagram-assets

Minified UMD builds committed so the docs site's interactive workflow diagrams
render without any CDN request (the book is fully self-hosted). `deny.toml`
covers only the Rust dependency tree, so this file is the license/provenance
record for these assets.

| File | Package | Version | License | Upstream |
|------|---------|---------|---------|----------|
| `react.production.min.js` | react | 18.3.1 (canary `f1338f8080-20240426`, from the file's own `version:` field) | MIT | <https://github.com/facebook/react> |
| `react-dom.production.min.js` | react-dom | 18.3.1 (same canary as react) | MIT | <https://github.com/facebook/react> |
| `reactflow.umd.min.js` | reactflow (React Flow 11 UMD build) | not embedded in the minified build — record the exact version on the next update | MIT | <https://github.com/xyflow/xyflow> |
| `reactflow.css` | reactflow (same release as the JS) | ditto | MIT | <https://github.com/xyflow/xyflow> |
| `dagre.min.js` | dagre | not embedded in the minified build — record the exact version on the next update | MIT | <https://github.com/dagrejs/dagre> |

When updating: take the published npm dist files for a tagged release, replace
the related files together (reactflow JS + CSS pair with the react build they
were built against), and write the version numbers into this table.
