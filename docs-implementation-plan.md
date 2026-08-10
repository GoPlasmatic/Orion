# Docs 2.0 — Implementation Plan (remaining work)

Execution tracker for [`docs-proposal.md`](./docs-proposal.md). Completed items
are pruned; git history of this file carries the full record.

> **Done (all on `v1.0.0`):**
> - Phase 0 `1edad10c` — live factual errors fixed in place (retry/masking/
>   counts/statuses/performance claims), two stale code comments.
> - Phase 1a `6bc77413` — unified Reference part, redirects, hard-link sweep,
>   `docs/lint.sh` + CI gate.
> - Phase 1b `e1d6149a` — all nine new reference pages (channel-config,
>   connectors, expressions, errors, metrics, cli, openapi, design-notes,
>   glossary), four pages revised, cross-page cuts applied, stale
>   `Arc<RwLock<Arc<Engine>>>` claims fixed everywhere (code is ArcSwap),
>   three doc-pinned test suites repointed. Integration suite 635/635.
> - Phase 2a `e435b540` — cli-setup → install.md; the AI trio → `ai/`;
>   upgrading → `operate/upgrading-to-1.0.md`; SUMMARY parts + redirects.
>
> **Next: Phase 2b** — the spine splits, six concept pages, landing-page
> rewrites, the upgrades home.

Standing constraints (from the settled-facts pass; full text in git history):

- **Quarantine is a channel-*load* failure state, not auth.** Full ruling
  (triggers, 404-vs-503, /health shape, DLQ replay, recovery-only-by-reload,
  no sync-path metric) is in this file's history at §2.3 and encoded in
  glossary.md; operate/troubleshooting.md (Phase 3) owns the lifecycle
  narrative; concepts/lifecycle.md gets two sentences.
- **MCP:** Streamable HTTP is implemented (`orion-cli mcp serve --http`,
  endpoint `/mcp`, default `0.0.0.0:8081`); a Cursor config exists at
  `crates/orion-cli/README.md:347-364` to port verbatim; **no client-side
  HTTP config exists anywhere** — see D4.
- One owner per fact now holds for the Reference estate — new pages must link
  the owners (channel-config, connectors, errors, metrics, expressions,
  configuration, cli), never restate them.
- mdBook 0.5: native admonitions (`> [!NOTE]`/`[!WARNING]`/`[!TIP]`);
  fragment redirects supported; `docs_link_test` validates links **and
  anchors** in CI.

---

## Decisions still open (defaults apply if undecided)

| # | Decision | Blocks | Default |
|---|---|---|---|
| D2 | Build a runnable Kafka example package under `examples/packages/`? | guides/kafka-channels.md (Phase 4) | Guide builds from the existing config surface only |
| D3 | Add a runnable `channel_call` composition example? | guides/workflow-patterns.md (Phase 4) | Pattern marked config-documented-only |
| D4 | Author a client-side HTTP-transport MCP config (no repo precedent; `server.json` advertises stdio only)? | ai/mcp-setup.md (Phase 2b) | Document server-side `--http` + point clients at `http://host:8081/mcp`; no JSON block until verified against a real client |
| D5 | Redirect targets for multi-destination splits | Phases 3–4 | §Redirects below, as written |

## Redirects still to ship (book.toml `[output.html.redirect]`)

```toml
# Phase 3 — operate + topology + architecture
"/architecture/overview.html" = "../concepts/how-orion-works.html"
"/architecture/overview.html#three-primitives" = "../concepts/how-orion-works.html#three-primitives"
"/architecture/overview.html#deployment-topology" = "../concepts/how-orion-works.html#deployment-topology"
"/architecture/characteristics.html" = "../concepts/how-orion-works.html"
"/features/observability.html" = "../operate/monitoring.html"
"/features/resilience.html" = "../operate/failure-handling.html"
"/features/security.html" = "../operate/security.html"
"/features/scalability.html" = "../reference/channel-config.html"
"/features/deployability.html" = "../operate/docker.html"
"/features/extensibility.html" = "../reference/connectors.html"
"/features/availability.html" = "../build/versioning.html"
"/features/maintainability.html" = "../operate/backup-restore.html"
"/topology/environments.html" = "../operate/cluster.html"
"/topology/packages.html" = "../concepts/packages.html"
"/topology/kubernetes.html" = "../operate/kubernetes.html"

# Phase 4 — guides
"/tutorials/use-cases.html" = "../guides/worked-examples.html"
```

Before each phase merges, re-run the inbound-link sweep and cover any newly
discovered fragment link:

```bash
grep -oE "goplasmatic\.github\.io/Orion/[A-Za-z0-9_./#-]+" \
  README.md examples/README.md docs/src/llms.txt | sort -u
```

---

## Phase 2b — spine, Concepts, landing pages (finishes PR 3)

- [ ] **T2b.1 Split install.md.** The moved page still carries all of old
  cli-setup: extract "Create Your First Service" → `getting-started/first-service.md`
  (hosts the asciinema cast cut from the intro; ends with verification + a
  Next Steps router that names operate/security.md — as a link to
  `features/security.md` until Phase 3 flips it); extract "Testing Workflows
  Offline" + "A regression suite" → parked for build/testing.md (Phase 4) with
  a `TODO(docs2)` marker or trimmed to links; the promotion paragraph and the
  Configuration digression become links. install.md narrows to install + run +
  verify.
- [ ] **T2b.2 Write `getting-started/test-and-promote.md`** — the arc-closing
  tutorial: lint → dry-run with stubs → one regression case → export → plan →
  apply to a second local instance, observable verification throughout.
  Sources: old cli-setup bottom half, packages.md try-it.
- [ ] **T2b.3 Six concept pages** (`concepts/`): how-orion-works (absorbs
  overview.md's user-level half + environments.md's dev-vs-prod table; honest
  extension-surface statement), channels, workflows, connectors, packages,
  lifecycle. Template: definition → diagram → bold-lead characteristics →
  Next Steps; ~100–140 lines each; two quarantine sentences in lifecycle.md.
- [ ] **T2b.4 Rewrite introduction.md** (landing hub: one first-step name, cast
  out to first-service, link farms cut, primitives table → concepts) **and
  comparison.md** (data-plane warning gets its forward pointer; "Use for:"
  lines; plain phrasing).
- [ ] **T2b.5 AI-page revisions in place:** mcp-setup.md becomes single owner
  of client setup (one config block + note that Claude Code/Desktop are
  identical; port the Cursor config from `crates/orion-cli/README.md:347-364`;
  HTTP per D4; drop the duplicated five-step sequence); claude-code.md
  collapses setup to one `claude mcp add` + outcome bullets; prompt-pack.md
  gains its provenance note.
- [ ] **T2b.6 Write `operate/upgrades.md`** (standing home: how upgrades work,
  preflight first, per-version guides as children) and **move the
  "How a rename fails, by surface" section** out of upgrading-to-1.0.md into
  it (it was parked there in Phase 1b). SUMMARY nests upgrading-to-1.0 under
  it. Restructure upgrading-to-1.0.md's "Smaller behaviour changes" heap into
  typed groups (renames / security / API shape / behavior) and unify the
  3b/7b/14b/17b numbering.
- [ ] **T2b.7 Examples page rewrite:** explicit clone step; pull the
  walkthrough content in from examples/README.md (which shrinks to a pointer
  in the same commit); first-connector.md gets its `{{#include}}` from
  postgres-orders and the plain-language victory lap.
- [ ] **T2b.8 Concepts part enters SUMMARY** (after Get Started); Architecture
  part shrinks or dissolves early if overview.md's split completes here
  (ship the architecture redirects with it if so).
- [ ] **T2b.9 Acceptance:** build + lint + `docs_link`/drift tests green;
  casts play from their new hosts; the beginner spine reads install →
  first-service → first-connector → test-and-promote with no operator detours.

## Phase 3 — Operate (PR 4)

- [ ] **T3.1** Dissolve the eight features/* pages into: production-checklist,
  docker, kubernetes (move), cluster, security, monitoring, traces,
  failure-handling, backup-restore, audit-logs. The Phase-1b link-stubs in
  those pages mark exactly what remains to move; reconcile with the
  reference owners rather than duplicating. Set `DOCS2_PHASE=3` in
  `docs/lint.sh` (activates the Rust-internals guard).
- [ ] **T3.2** Topology: environments → cluster (+ dev-vs-prod table to
  how-orion-works if not already); packages.md → concepts/packages.md +
  operate/promotion.md; promotion.md absorbs admin-api's parked essay
  (marked `TODO(docs2)`), adds ORION_ADMIN_TOKEN note + mid-apply
  failure-modes subsection.
- [ ] **T3.3** troubleshooting.md — symptom-first; quarantine section fully
  specified by the settled facts (see Standing constraints).
- [ ] **T3.4** Delete characteristics.md (+ its d3 CDN dependency) and
  overview.md remnants. Update `config_docs_drift_test`'s excuse-list row for
  deployability.md when it dissolves.
- [ ] **T3.5** Phase-3 redirects; flip the two forward links (first-service
  Next Steps, comparison warning) to operate/security.md; sizing section in
  cluster.md from the measured benchmark numbers only.
- [ ] **T3.6** Acceptance: features/, architecture/, topology/ directories
  gone; `git grep -l "d3js.org" docs/` → 0; all gates green.

## Phase 4 — Build + Guides (PR 5)

- [ ] **T4.1** Build how-tos: workflows, channels, connectors, testing (the
  content parked from install.md), versioning (single owner of
  import/export; availability.md's remnants dissolve into it).
- [ ] **T4.2** Guides: worked-examples (setup-first fix, `{{#include}}` from
  examples/packages, honest simulated-effect notes), workflow-patterns (D3),
  ci-cd (replaces the broken curl-loop example; workflow YAML actionlint-
  checked), kafka-channels (D2; the parked Kafka-consumer block in
  extensibility.md moves here + configuration.md). Tutorials part dissolves.
- [ ] **T4.3** use-cases redirect; SUMMARY reaches proposal §3 final shape;
  set `DOCS2_PHASE=4` (activates SUMMARY↔llms.txt parity — curate llms.txt to
  cover every chapter).
- [ ] **T4.4** Acceptance: all gates green; every `{{#include}}` resolves.

## Phase 5 — Style sweep (PR 6)

- [ ] **T5.1** Page-by-page pass enforcing proposal §6: sentence surgery,
  admonition conversion, "Use for:" selector lines, Next-Steps blocks,
  one-altitude checks, duplicate-block removal.
- [ ] **T5.2** All guard greps green; grep for leftover `TODO(docs2)` markers
  → 0.
- [ ] **T5.3** Full-book read-through in SUMMARY order (llms-full.txt) for the
  three persona journeys: evaluator, new builder, operator-going-to-prod.

## Code-hygiene follow-ups (small, non-blocking, any phase)

- [ ] orion-cli clap help says "run 'engine reload' after to apply" on
  activate/archive; the server auto-reloads on status changes.
- [ ] `crates/orion-server/src/query/error.rs` runtime string says
  "behaviour" (quoted verbatim by data-dialect.md) — decide on American
  spelling in runtime strings.
- [ ] `engine/functions/schema.rs:77` comment reads as contradicting the
  serde surface (`channel_call` also accepts the `response_path` alias).

## Risks that remain live

- **Interim duplication windows:** features/* pages still carry operator
  narratives whose reference facts moved in 1b — Phase 3 closes them. Never
  edit a fact in both places; the reference page is the owner.
- **Forward links:** exactly two exist (first-service → features/security.md,
  comparison.md → features/security.md), flipped in T3.5. Grep `TODO(docs2)`
  before closing each phase.
- **Tests that pin doc paths:** before each rename phase run
  `git grep -ln "docs/src" crates | grep -v '\.md'` — jsonlogic, config-drift
  and metrics-drift are already repointed; new pins may appear.
- **examples/README.md ownership inversion** lands with T2b.7 — one owner,
  same commit.

## Definition of done (unchanged)

1. SUMMARY.md matches proposal §3 (± the D2-gated Kafka guide).
2. All 31 original pages accounted for per the migration map; old URLs
   redirect; every hard inbound link resolves.
3. docs-lint green in CI at `DOCS2_PHASE=4` strictness.
4. Each fact has one owner; the guard greps hold.
5. A cold read of Get Started succeeds end-to-end against a locally built
   server.
