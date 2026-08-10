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
> - Phase 2b `2131624c` — install split (install + first-service),
>   test-and-promote, six concept pages, introduction/comparison rewrites, the
>   AI trio revised, `operate/upgrades.md`, examples ownership inversion,
>   Architecture dissolved early. Book builds on mdBook 0.5.2; docs-lint and a
>   Python twin of `docs_link_test` green.
>
> - Phase 3 `55490521` — the twelve-page Operate estate; `features/` and
>   `topology/` dissolved; the promotion essay left admin-api.md and the
>   Production Checklist left configuration.md; `DOCS2_PHASE=3`.
>
> **Next: Phase 4** — Build + Guides.

Standing constraints (from the settled-facts pass; full text in git history):

- **Quarantine** is settled and written: operate/troubleshooting.md owns the
  lifecycle narrative (triggers, 503-vs-404, /health shape, Kafka-to-DLQ
  replay, recovery-only-by-reload, no sync-path metric), concepts/lifecycle.md
  carries the two-sentence version, glossary.md the one-line definition.
- **MCP:** settled in Phase 2b. ai/mcp-setup.md is the single owner of client
  setup (one stdio block for Claude Code/Desktop, the Cursor object, and the
  server-side `--http` transport).
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
| D5 | Redirect targets for multi-destination splits | Phases 3–4 | §Redirects below, as written |

**D4 settled (Phase 2b), by its default.** mcp-setup.md documents server-side
`orion-cli mcp serve --http [--bind]` and names the endpoint
(`http://host:8081/mcp`, verified against `mcp/mod.rs`), with no client-side
JSON block and a warning that the transport carries no auth of its own.

## Redirects still to ship (book.toml `[output.html.redirect]`)

The Phase 2b (architecture) and Phase 3 (features/, topology/, the two evicted
reference sections) blocks are shipped. What remains:

```toml
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

## Phase 4 — Build + Guides (PR 5)

- [ ] **T4.1** Build how-tos: workflows, channels, connectors, testing (the
  content parked in getting-started/test-and-promote.md, marked `TODO(docs2)`),
  versioning (single owner of import/export; the lifecycle-ops content now
  linked from concepts/lifecycle.md dissolves into it).
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

- **Interim duplication windows: closed.** features/* and topology/* are
  gone; every operator narrative now links its reference owner instead of
  restating it. The rule still stands for Phase 4: never edit a fact in two
  places, the reference page is the owner.
- **Forward links:** two `TODO(docs2)` markers remain, both for Phase 4 —
  concepts/lifecycle.md → build/versioning.md, and
  getting-started/test-and-promote.md's stub/case-format ownership note →
  build/testing.md. Grep `TODO(docs2)` before closing the phase.
- **Tests that pin doc paths:** before each rename phase run
  `git grep -ln "docs/src" crates | grep -v '\.md'` — jsonlogic, config-drift
  and metrics-drift are already repointed; new pins may appear.
- **Checklist-row numbers in the 1.0 upgrade guide are pinned by the
  binary.** `preflight.rs` emits `[3]` and `[14]` as finding labels, so the
  rows could not be renumbered sequentially. The `3b/7b/14b/17b` suffixes were
  removed instead by merging each into its parent row; the four typed groups
  (renames / security / API shape / runtime behaviour) replaced the
  "Smaller behaviour changes" heap. Any future renumbering has to move
  `preflight.rs` in the same commit.

## Definition of done (unchanged)

1. SUMMARY.md matches proposal §3 (± the D2-gated Kafka guide).
2. All 31 original pages accounted for per the migration map; old URLs
   redirect; every hard inbound link resolves.
3. docs-lint green in CI at `DOCS2_PHASE=4` strictness.
4. Each fact has one owner; the guard greps hold.
5. A cold read of Get Started succeeds end-to-end against a locally built
   server.
