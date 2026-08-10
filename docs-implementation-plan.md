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
> - Phase 4 `433397e4` — the five Build how-tos and four Guides;
>   tutorials/use-cases.md retired; SUMMARY at proposal §3's final shape;
>   `DOCS2_PHASE=4`, so llms.txt now covers every chapter.
>
> - Phase 5 `f9a4e5cc` — style sweep: `TODO(docs2)` markers to zero, Related
>   blocks on every chapter, H3 depth restored, the worst sentences split, and
>   the three persona journeys verified to link end to end with no dead ends.
>
> **The restructure is complete.** What follows is the residue.

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
*(None. D2, D3, D4 and D5 all settled by their defaults — see below.)*

**D2 and D3 settled (Phase 4), by their defaults.** guides/kafka-channels.md is
built from the configuration surface with a scoping note saying so, and
guides/workflow-patterns.md marks the `channel_call` composition pattern as
documented-from-config rather than extracted from a tested package. Both remain
worth funding with a runnable example package later; neither blocks anything now.

**D5 settled.** Every redirect shipped as written, plus five fragment redirects
the sweep turned up (`#request-processing-flow`, `#health-monitoring`,
`#what-to-alert-on`, `#production-checklist`, `#the-orion-server-package-cli`).

**D4 settled (Phase 2b), by its default.** mcp-setup.md documents server-side
`orion-cli mcp serve --http [--bind]` and names the endpoint
(`http://host:8081/mcp`, verified against `mcp/mod.rs`), with no client-side
JSON block and a warning that the transport carries no auth of its own.

## Redirects

All shipped. `book.toml` carries the Phase 1, 2a, 2b, 3 and 4 blocks; every
retired URL resolves, including the fragment redirects the link sweeps turned
up.

Before each phase merges, re-run the inbound-link sweep and cover any newly
discovered fragment link:

```bash
grep -oE "goplasmatic\.github\.io/Orion/[A-Za-z0-9_./#-]+" \
  README.md examples/README.md docs/src/llms.txt | sort -u
```

---

## Code-hygiene follow-ups

- [x] orion-cli clap help told operators to "run 'engine reload' after to
  apply" on activate/archive; the server auto-reloads on status changes. Fixed
  in six places — the four subcommand help strings plus both command-group
  `long_about` lifecycle lines. The `--defer` help is unchanged and still
  correct.
- [x] Spelling: **British throughout**, settled. `behaviour` / `behaviours` /
  `behavioural`, `defence`, `labelled` normalised across 38 files (docs, Rust
  comments, SQL comments, shell, markdown). Deliberately untouched, because
  they are identifiers or external vocabulary rather than prose: the `colored`
  crate and `NO_COLOR`, CSS `color`/`center`, `<div align="center">`,
  Elasticsearch's `analyzed`, PostgreSQL's system `catalog`, SPDX `license`,
  and serde's `Serialize`. Oxford `-ize` is British and stays, so no
  `serialise`/`normalise` churn against the surrounding identifiers.
- [x] `engine/functions/schema.rs` comment claimed only `http_call.output`
  carries the `response_path` alias. `channel_call.output` carries one too.
  Comment corrected.

**One live doc bug found while closing the above.**
`reference/workflows.md` claimed "`output` wins when both are present" for the
`response_path` alias. A serde alias cannot express precedence: supplying both
spellings is a duplicate-field refusal, which `reference/support.md` already
stated correctly and `output_field_test.rs` asserts.

**A second, found by validating the Phase 4 examples against a live server.**
`build/channels.md` showed the deduplication block as
`{enabled, header, ttl_secs}`. The real shape is
`{header, window_secs, connector, on_backend_error}` — the page was refused by
the server. All eight `config` examples on that page are now validated against
a running instance.

## Follow-ups: done

- [x] **A runnable Kafka example package.**
  `examples/packages/kafka-order-events` — a Kafka-protocol channel plus the
  workflow that stamps a record's topic coordinates. It deploys with
  `./examples/deploy.sh kafka-order-events` and needs no broker to create or
  activate; two offline `*.case.json` cases cover the logic.
  guides/kafka-channels.md drops its scoping note and `{{#include}}`s both
  files.
- [x] **A runnable `channel_call` composition example.**
  `examples/packages/channel-composition` — two services, where
  `order-enrichment` calls `customer-lookup` in-process. Covered three ways:
  offline cases (including one that stubs the `channel_call`), a live e2e case
  in `examples/use-cases/`, and `deploy.sh`. guides/workflow-patterns.md
  `{{#include}}`s both workflows and documents the four things the shape gets
  wrong easily.
- [x] **Sentence surgery** on the flagged sentences in data-dialect.md,
  admin-api.md and upgrading-to-1.0.md.

Two pieces of machinery came with them, both backward compatible:
`examples/deploy.sh` now deploys multi-entity packages (`workflow-*.json`,
`channel-*.json`) and packages with no HTTP route, and the e2e case format
takes an optional `channels` array binding each channel to a named workflow —
without it, the previous first-workflow binding still applies.

## Risks that remain live

- **Interim duplication windows: closed.** features/* and topology/* are
  gone; every operator narrative now links its reference owner instead of
  restating it. The rule stands for anything added later: never edit a fact in
  two places, the reference page is the owner.
- **Forward links: none.** `TODO(docs2)` is at zero. Any future parked
  reference should reuse the marker so the same grep keeps working.
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

1. **Done.** SUMMARY.md matches proposal §3 — 58 pages, every part in the
   specified order.
2. **Done.** All 31 original pages accounted for; every retired URL redirects
   (including the fragment redirects the sweeps turned up); every hard inbound
   link in README.md, examples/README.md and llms.txt resolves.
3. **Done.** docs-lint green at `DOCS2_PHASE=4`, which is full strictness:
   include targets, relative links, hard site links, magic numbers, the single
   function-count owner, review IDs, the Rust-internals guard, and
   SUMMARY↔llms.txt parity.
4. **Done.** Each fact has one owner; all guard greps hold; `TODO(docs2)` is
   zero.
5. **Done.** The Get Started arc was executed against a locally built 1.0
   server in Phase 2b, and four sample outputs were corrected to match what the
   binary actually prints (`cc9bd1f9`).
