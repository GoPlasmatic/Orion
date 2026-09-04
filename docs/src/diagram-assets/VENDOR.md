# Vendored third-party assets — docs/src/diagram-assets

Minified UMD builds committed so the docs site's interactive workflow diagrams
render without any CDN request (the book is fully self-hosted). `deny.toml`
covers only the Rust dependency tree, so this file is the license/provenance
record for these assets.

| File | Package | Version | License | Upstream |
|------|---------|---------|---------|----------|
| `react.production.min.js` | react | 18.3.1 (canary `f1338f8080-20240426`, from the file's own `version:` field) | MIT | <https://github.com/facebook/react> |
| `react-dom.production.min.js` | react-dom | 18.3.1 (same canary as react) | MIT | <https://github.com/facebook/react> |
| `reactflow.umd.min.js` | reactflow (React Flow 11 UMD build) | 11.11.4 | MIT | <https://github.com/xyflow/xyflow> |
| `reactflow.css` | reactflow (same release as the JS) | 11.11.4 | MIT | <https://github.com/xyflow/xyflow> |
| `dagre.min.js` | dagre | 0.8.5 | MIT | <https://github.com/dagrejs/dagre> |

The MIT permission notice and each package's copyright line — required of any
redistribution — are in [`LICENSE-third-party.txt`](./LICENSE-third-party.txt)
beside these files, and summarised in the repository's root `NOTICE`.

The reactflow and dagre versions are not embedded in the minified builds; they
were recovered by byte-comparing against the published npm tarballs. Keep this
table current instead of relying on that.

When updating: take the published npm dist files for a tagged release, replace
the related files together (reactflow JS + CSS pair with the react build they
were built against), and write the version numbers into this table.

## Why these three are not on the newest release

`reactflow` 11.11.4 and `dagre` 0.8.5 are the **last** versions published under
those package names — neither is behind. Moving either forward is a package
switch, not a version bump:

- React Flow 12 is published as `@xyflow/react` (12.x). It still ships a UMD
  build (`dist/umd/index.js`) and its peer range accepts React >= 17, so
  vendoring it the same way is feasible, but it is a migration: `parentNode` →
  `parentId` on nodes, the removal of `nodeInternals`, and measured-dimension
  changes all touch `diagram-widget.js`.
- dagre's maintained fork is `@dagrejs/dagre` (3.x). The `graphlib.Graph` /
  `layout` surface this widget uses is unchanged, so that one is close to a
  rename.

React 18.3.1 → 19.x is available, but React Flow 11 is built and tested against
React 17–18, so upgrading React while staying on v11 takes on risk for no gain.
The three move together or not at all.
